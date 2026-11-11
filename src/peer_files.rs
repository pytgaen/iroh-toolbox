//! Files between paired machines, without a code (see docs/guides/files.md):
//! `receive --inbox` waits for `send PATH peer:NAME`, and `send PATH
//! --peers-only` waits for `receive peer:NAME`.
//!
//! Servers run on a slot: the client proves its machine key, and the server
//! checks its peer book and `--allow` before anything else. After `ready`, the
//! handshake stream carries the messages below; the files travel as an
//! iroh-blobs collection on the same authenticated connection, served by the
//! side that offers them and verified with BLAKE3 by the side that receives.
//!
//! ```text
//! inbox:
//! sender → inbox   offer      { version, hash, name, bytes, files }
//! inbox  → sender  wait       another transfer goes first
//!                  receiving  the inbox downloads the offer now
//!                  refused    { reason }   nothing was published
//!                  identical  the destination already holds exactly these files
//!                  received   verified and published
//! sender → inbox   thanks     the result arrived
//!
//! send --peers-only:
//! send    → receiver  offer { … }, or refused { reason } (reserved, full)
//! receiver → send     received | identical | declined { reason }
//! send    → receiver  thanks
//! ```
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh_blobs::Hash;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Semaphore, watch},
    task::JoinSet,
};

use crate::{
    config::Settings,
    files::{self, PeerReception, PreparedSend, ReceiveOptions, Refusal, Replacement},
    network,
    progress::{Progress, safe_text},
    slot::{self, Handshake, Protocol, Remote, Serve},
    target::{self, Destination, Target},
};

const VERSION: u32 = 1;
const FRAME_LIMIT: usize = 64 * 1024;
/// Senders an inbox keeps waiting behind the current transfer.
const WAITING_LIMIT: usize = 8;
/// How long an inbox waits for the sender to acknowledge a result.
const THANKS_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    Offer {
        version: u32,
        hash: String,
        name: String,
        bytes: u64,
        files: u64,
    },
    Wait {},
    Receiving {},
    Refused {
        reason: Reason,
    },
    Identical {},
    Received {},
    /// The receiver of `send --peers-only` published nothing.
    Declined {
        reason: Reason,
    },
    Thanks {},
}

/// Why an inbox published nothing. Each has a fixed text on the sender's
/// side: no text chosen by the other machine is shown.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Reason {
    TooLarge,
    Exists,
    UnsafeNames,
    /// A top-level name starts with a dot and the receiver has no `--hidden`.
    HiddenNames,
    /// `--once` and the single transfer succeeded.
    Closing,
    /// Too many senders are already waiting, or receivers served.
    Full,
    /// `send --peers-only` without `--share` serves another receiver.
    Reserved,
    /// Anything else on the receiving side, such as a full disk.
    Failed,
}

impl Reason {
    fn of(refusal: Refusal) -> Self {
        match refusal {
            Refusal::TooLarge => Self::TooLarge,
            Refusal::Exists => Self::Exists,
            Refusal::UnsafeNames => Self::UnsafeNames,
            Refusal::HiddenNames => Self::HiddenNames,
            // Answered with `identical`, never as a refusal.
            Refusal::Identical => Self::Failed,
        }
    }

    /// What the sender says, with the inbox's local name and the offer's name.
    fn explain(self, peer: &str, name: &str) -> String {
        match self {
            Self::TooLarge => {
                format!("{peer} refused {name}: larger than its inbox accepts (--max-bytes)")
            }
            Self::Exists => format!(
                "{peer} refused {name}: its inbox already holds files with these names; nothing was replaced"
            ),
            Self::UnsafeNames => format!("{peer} refused {name}: unsafe file names"),
            Self::HiddenNames => format!(
                "{peer} refused {name}: a name starts with a dot; its inbox accepts them with --hidden"
            ),
            Self::Closing => format!(
                "{peer}'s inbox is closing: it already received its single transfer (--once); nothing was sent"
            ),
            Self::Full => {
                format!("{peer}'s inbox has too many senders waiting; try again later")
            }
            Self::Reserved | Self::Failed => {
                format!("{peer} could not store {name}; check the output of its `receive --inbox`")
            }
        }
    }

    /// What `send --peers-only` says of a receiver that published nothing.
    fn declined(self) -> &'static str {
        match self {
            Self::TooLarge => "larger than its --max-bytes",
            Self::Exists => "its destination already holds files with these names",
            Self::UnsafeNames => "unsafe file names",
            Self::HiddenNames => "a name starts with a dot; it accepts them with --hidden",
            _ => "it could not store them; see its output",
        }
    }

    /// What `receive peer:NAME` says when the offer is not served to it.
    fn unserved(self, peer: &str) -> String {
        match self {
            Self::Reserved => format!(
                "{peer} is sending to another machine; ask {peer} to share with `send PATH --peers-only --share`"
            ),
            Self::Full => format!("{peer} serves too many receivers at once; try again later"),
            _ => format!("{peer} refused to send"),
        }
    }
}

async fn write(send: &mut SendStream, message: &Message) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(bytes.len() <= FRAME_LIMIT, "file message exceeds limit");
    send.write_u32(bytes.len() as u32).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read(recv: &mut RecvStream) -> Result<Message> {
    let size = recv.read_u32().await? as usize;
    ensure!(
        size > 0 && size <= FRAME_LIMIT,
        "file message exceeds limit"
    );
    let mut bytes = vec![0; size];
    recv.read_exact(&mut bytes).await?;
    parse_message(&bytes)
}

fn parse_message(bytes: &[u8]) -> Result<Message> {
    serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("invalid file message"))
}

/// An offer as the inbox checked it: its name is safe to print.
struct Offer {
    hash: Hash,
    name: String,
    bytes: u64,
}

fn check_offer(message: Message) -> Result<Offer> {
    let Message::Offer {
        version,
        hash,
        name,
        bytes,
        files,
    } = message
    else {
        bail!("expected a file offer");
    };
    ensure!(version == VERSION, "incompatible file offer version");
    ensure!(
        !name.is_empty() && name.len() <= 4096 && files > 0,
        "invalid file offer"
    );
    let hash: [u8; 32] = hex::decode(&hash)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .context("invalid file offer")?;
    Ok(Offer {
        hash: Hash::from_bytes(hash),
        name: safe_text(&name),
        bytes,
    })
}

// --- Inbox -----------------------------------------------------------------

/// `receive --inbox`.
pub struct InboxOptions<'a> {
    pub serve: Serve<'a>,
    /// Exit after the first published transfer.
    pub once: bool,
    pub output: &'a Path,
    pub max_bytes: u64,
    pub transfer_timeout: f64,
    pub no_progress: bool,
    /// Accept top-level names starting with a dot.
    pub hidden: bool,
}

/// What the transfers of one inbox share.
struct Inbox {
    settings: Settings,
    output: PathBuf,
    max_bytes: u64,
    transfer_timeout: f64,
    no_progress: bool,
    hidden: bool,
    once: bool,
    /// One transfer at a time.
    turn: Arc<Semaphore>,
    waiting: AtomicUsize,
    /// Set once `--once` succeeded: nothing new starts.
    closing: watch::Sender<bool>,
}

pub async fn inbox(settings: &Settings, options: InboxOptions<'_>) -> Result<()> {
    files::check_destination(options.output)?;
    let (ep, server) = slot::bind_server(settings, vec![], Protocol::Inbox, options.serve).await?;
    let server = server.context("an inbox always runs on a slot")?;
    slot::announce(settings, &ep, Some(&server), "send PATH", "")?;
    let output = std::path::absolute(options.output)?;
    if options.once {
        note!(
            "Receiving into {}: one transfer, then exit",
            output.display()
        );
    } else {
        note!("Receiving into {} until Ctrl-C", output.display());
    }
    let (closing, mut closed) = watch::channel(false);
    let inbox = Arc::new(Inbox {
        settings: settings.clone(),
        output,
        max_bytes: options.max_bytes,
        transfer_timeout: options.transfer_timeout,
        no_progress: options.no_progress,
        hidden: options.hidden,
        once: options.once,
        turn: Arc::new(Semaphore::new(1)),
        waiting: AtomicUsize::new(0),
        closing,
    });
    let mut tasks = JoinSet::new();
    let pending = network::Pending::default();
    let result = loop {
        tokio::select! {
            incoming = ep.accept() => {
                let Some(incoming) = incoming else { break Err(anyhow::anyhow!("endpoint closed")) };
                let Some((incoming, mut handshaking)) = pending.screen(incoming) else {
                    continue;
                };
                let (server, inbox) = (server.clone(), inbox.clone());
                tasks.spawn(async move {
                    let Some((conn, handshake)) =
                        slot::accept_stream(&server, incoming, "inbox", &mut handshaking).await
                    else {
                        return;
                    };
                    drop(handshaking);
                    if let Err(error) = inbox.welcome(&conn, handshake).await {
                        detail!("inbox: {error:#}");
                    }
                    // Let the sender read the last message before closing.
                    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                });
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            Ok(_) = closed.wait_for(|closing| *closing) => break Ok(()),
        }
    };
    // Waiting senders are told that the inbox closes; the last result is delivered.
    let _ = tokio::time::timeout(THANKS_DEADLINE + Duration::from_secs(2), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    ep.close().await;
    result
}

impl Inbox {
    /// Serves one authenticated sender: its offer, its turn, then its transfer.
    async fn welcome(&self, conn: &Connection, handshake: Handshake) -> Result<()> {
        let Handshake {
            mut send,
            mut recv,
            peer,
        } = handshake;
        let peer = peer.context("an inbox admits proven machines only")?;
        let who = peer.name.as_str();
        let offer = tokio::time::timeout(self.settings.timeout, read(&mut recv))
            .await
            .context("the sender sent no offer in time")??;
        let offer = check_offer(offer)?;
        let name = offer.name.as_str();
        let refuse = |reason: Reason| Message::Refused { reason };
        if offer.bytes > self.max_bytes {
            note!("refused {name} from {who}: larger than --max-bytes");
            return write(&mut send, &refuse(Reason::TooLarge)).await;
        }
        let _turn = match self.wait_turn(conn, &mut send, who, name).await? {
            Some(turn) => turn,
            None => return write(&mut send, &refuse(Reason::Closing)).await,
        };
        write(&mut send, &Message::Receiving {}).await?;
        note!("receiving {name} from {who}");
        let mut progress = Progress::new("receive", !self.no_progress);
        let options = ReceiveOptions {
            max_bytes: self.max_bytes,
            transfer_timeout: self.transfer_timeout,
            no_progress: self.no_progress,
            // An inbox never replaces nor merges: nobody is there to confirm.
            replacement: Replacement::Refuse,
            confirmation: None,
            hidden: self.hidden,
        };
        let reception = PeerReception {
            conn,
            machine: &peer.machine,
            hash: offer.hash,
            resume: "it resumes when the same files are sent again",
            identical_ends: true,
        };
        let received = files::receive_offer(
            &self.settings,
            reception,
            &self.output,
            &options,
            &mut progress,
        )
        .await;
        progress.finish();
        let error = match received {
            Ok(()) => {
                note!("received {name} from {who}");
                // A success counts even if the sender never hears about it.
                if self.once {
                    self.closing.send_replace(true);
                }
                write(&mut send, &Message::Received {}).await?;
                let _ = tokio::time::timeout(THANKS_DEADLINE, read(&mut recv)).await;
                return Ok(());
            }
            Err(error) => error,
        };
        match error.downcast_ref::<Refusal>().copied() {
            Some(Refusal::Identical) => {
                note!(
                    "{who} sent {name} again: identical files are already in {}; nothing changed",
                    self.output.display()
                );
                write(&mut send, &Message::Identical {}).await
            }
            Some(refusal) => {
                note!("refused {name} from {who}: {error:#}");
                write(&mut send, &refuse(Reason::of(refusal))).await
            }
            None if files::is_lost(&error) => {
                note!(
                    "transfer of {name} from {who} interrupted; verified data is kept until the same files are sent again"
                );
                Ok(())
            }
            None => {
                note!("could not receive {name} from {who}: {error:#}");
                write(&mut send, &refuse(Reason::Failed)).await
            }
        }
    }

    /// Waits for the single transfer slot, telling the sender it waits. `None`
    /// once the inbox closes; an error if the sender leaves or too many wait.
    async fn wait_turn(
        &self,
        conn: &Connection,
        send: &mut SendStream,
        who: &str,
        name: &str,
    ) -> Result<Option<tokio::sync::OwnedSemaphorePermit>> {
        if *self.closing.borrow() {
            return Ok(None);
        }
        if let Ok(turn) = self.turn.clone().try_acquire_owned() {
            return Ok(Some(turn));
        }
        if self.waiting.fetch_add(1, Ordering::SeqCst) >= WAITING_LIMIT {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            write(
                send,
                &Message::Refused {
                    reason: Reason::Full,
                },
            )
            .await?;
            bail!("refused {name} from {who}: too many senders waiting");
        }
        write(send, &Message::Wait {}).await?;
        note!("{who} waits to send {name}");
        let mut closing = self.closing.subscribe();
        let turn = tokio::select! {
            turn = self.turn.clone().acquire_owned() => Some(turn?),
            _ = closing.wait_for(|closing| *closing) => None,
            _ = conn.closed() => {
                self.waiting.fetch_sub(1, Ordering::SeqCst);
                bail!("{who} stopped waiting to send {name}");
            }
        };
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        // The single transfer of `--once` may have succeeded meanwhile.
        Ok(turn.filter(|_| !*self.closing.borrow()))
    }
}

// --- Sender ----------------------------------------------------------------

/// The paired machine behind a `send PATH TARGET`: `peer:NAME[/SLOT]` only.
fn inbox_target(value: &str) -> Result<Target> {
    match target::parse(value)? {
        target @ Target::Peer { .. } => Ok(target),
        Target::Code(_) => bail!(
            "send creates its code itself: run `iroh-toolbox send PATH` without a target, then type the code on the other machine"
        ),
        Target::Ticket(_) => bail!("send PATH accepts peer:NAME[/SLOT] only"),
    }
}

/// The hint when nothing answers at the inbox's slot.
fn not_receiving(remote: &Remote) -> anyhow::Error {
    let peer = &remote.peer;
    let slot = if remote.slot == Protocol::Inbox.default_slot() {
        String::new()
    } else {
        format!(" --slot {}", remote.slot)
    };
    anyhow::anyhow!("{peer} is not receiving; run 'iroh-toolbox receive --inbox{slot}' on {peer}")
}

/// `send PATH peer:NAME`: pushes a file or folder to a paired machine's inbox.
pub async fn send_to(
    settings: &Settings,
    path: &Path,
    target: &str,
    addrs: &[String],
    transfer_timeout: f64,
    no_progress: bool,
) -> Result<()> {
    let destination = Destination {
        target: inbox_target(target)?,
        addrs: target::direct_addrs(addrs).await?,
    };
    let remote =
        destination
            .target
            .remote("send", Protocol::Inbox, &destination.addrs, settings)?;
    let peer = remote.peer.clone();
    let mut progress = Progress::new("send", !no_progress);
    progress.phase("preparing files (scanning)", None);
    let prepared = PreparedSend::with_progress(path, &mut progress).await?;
    let name = safe_text(&prepared.name);
    let ep = target::bind_client(settings, &destination.target).await?;
    let result = async {
        progress.phase(format!("connecting to {peer}'s inbox"), None);
        let (conn, handshake) = progress
            .wait(slot::connect_stream(
                &ep,
                &remote,
                Protocol::Inbox,
                settings,
            ))
            .await
            .map_err(|error| {
                if error.is::<slot::Unavailable>() {
                    not_receiving(&remote)
                } else {
                    error
                }
            })?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(transfer_timeout);
        tokio::time::timeout_at(
            deadline,
            deliver(&conn, handshake, &prepared, &peer, &name, &mut progress),
        )
        .await
        .with_context(|| {
            format!(
                "file transfer to {peer} timed out; increase --transfer-timeout for large transfers"
            )
        })?
    }
    .await;
    ep.close().await;
    prepared.store.shutdown().await?;
    result
}

/// Offers the files on an authenticated inbox connection and serves them until
/// the inbox says what became of them.
async fn deliver(
    conn: &Connection,
    handshake: Handshake,
    prepared: &PreparedSend,
    peer: &str,
    name: &str,
    progress: &mut Progress,
) -> Result<()> {
    let Handshake {
        mut send, mut recv, ..
    } = handshake;
    write(
        &mut send,
        &Message::Offer {
            version: VERSION,
            hash: prepared.hash().to_hex().to_string(),
            name: prepared.name.clone(),
            bytes: prepared.bytes,
            files: prepared.files as u64,
        },
    )
    .await?;
    let unknown = || {
        anyhow::anyhow!(
            "the connection to {peer} was lost before it confirmed the result; the outcome is unknown: check {peer}, then send again (verified data resumes, identical files are recognized)"
        )
    };
    loop {
        progress.phase(format!("waiting for {peer}'s inbox"), None);
        match read(&mut recv).await.map_err(|_| unknown())? {
            Message::Wait {} => {
                note!("{peer}'s inbox is receiving another transfer; waiting for its turn")
            }
            Message::Receiving {} => break,
            Message::Refused { reason } => bail!(reason.explain(peer, name)),
            Message::Identical {} => {
                note!("{peer} already holds identical files; nothing was sent again");
                return Ok(());
            }
            _ => bail!("unexpected message from {peer}'s inbox"),
        }
    }
    let sending = match prepared.files {
        1 => format!("sending {name} to {peer}"),
        files => format!("sending {name} ({files} files) to {peer}"),
    };
    progress.step(sending, Some(prepared.bytes));
    let mut served = 0;
    // Blobs are served on this connection until the inbox tells the result.
    let outcome = {
        let serving = files::serve_counting(&prepared.store, conn.clone(), progress, &mut served);
        tokio::select! {
            message = read(&mut recv) => message,
            _ = serving => Err(unknown()),
        }
    };
    match outcome.map_err(|_| unknown())? {
        Message::Received {} => {
            progress.finish();
            // Acknowledge, then close: the inbox needs nothing more.
            let _ = write(&mut send, &Message::Thanks {}).await;
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(1), send.stopped()).await;
            conn.close(0u32.into(), b"received");
            note!("{peer} received {name}: BLAKE3 verified and published");
            Ok(())
        }
        Message::Refused { reason } => bail!(reason.explain(peer, name)),
        Message::Identical {} => {
            note!("{peer} already holds identical files; nothing was sent again");
            Ok(())
        }
        _ => bail!("unexpected message from {peer}'s inbox"),
    }
}

// --- send --peers-only -----------------------------------------------------

/// Receivers a share serves at once; others are told to try again.
const SHARE_LIMIT: usize = 8;

/// `send PATH --peers-only [--share]`.
pub struct OfferOptions<'a> {
    pub serve: Serve<'a>,
    /// Keep serving after each reception, until Ctrl-C.
    pub share: bool,
    pub path: &'a Path,
    pub transfer_timeout: f64,
    pub no_progress: bool,
}

/// What the receptions of one offer share.
struct Offering {
    prepared: PreparedSend,
    name: String,
    share: bool,
    no_progress: bool,
    transfer_timeout: Duration,
    /// Without `--share`: the receiver the offer is pinned to, and when its
    /// reception must end.
    pinned: std::sync::Mutex<Option<(iroh::PublicKey, String)>>,
    deadline: watch::Sender<Option<tokio::time::Instant>>,
    /// Receptions served at once with `--share`.
    serving: Arc<Semaphore>,
    /// Without `--share`: the outcome that ends the command.
    done: watch::Sender<Option<std::result::Result<(), String>>>,
}

pub async fn offer(settings: &Settings, options: OfferOptions<'_>) -> Result<()> {
    // Check the slot and `--allow` before a long preparation.
    let slot = options.serve.slot.unwrap_or("");
    let server = slot::Server::open(slot, Protocol::Send, options.serve.allow, false)?;
    let mut progress = Progress::new("send", !options.no_progress);
    progress.phase("preparing files (scanning)", None);
    let prepared = PreparedSend::with_progress(options.path, &mut progress).await?;
    progress.finish();
    let name = safe_text(&prepared.name);
    let ep = server.bind(settings, options.serve.port).await?;
    slot::announce(
        settings,
        &ep,
        Some(&server),
        "receive",
        " --output ./received",
    )?;
    let size = crate::progress::mib(prepared.bytes);
    let files = match prepared.files {
        1 => String::new(),
        files => format!(", {files} files"),
    };
    if options.share {
        note!("Sharing {name} ({size}{files}) with paired machines until Ctrl-C");
    } else {
        note!("Offering {name} ({size}{files}) to the first paired machine that receives it");
    }
    let (deadline, mut deadlines) = watch::channel(None);
    let (done, mut finished) = watch::channel(None);
    let offering = Arc::new(Offering {
        prepared,
        name,
        share: options.share,
        no_progress: options.no_progress,
        transfer_timeout: Duration::from_secs_f64(options.transfer_timeout),
        pinned: std::sync::Mutex::new(None),
        deadline,
        serving: Arc::new(Semaphore::new(SHARE_LIMIT)),
        done,
    });
    let mut tasks = JoinSet::new();
    let pending = network::Pending::default();
    let result = loop {
        let deadline = *deadlines.borrow_and_update();
        let expired = async move {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            incoming = ep.accept() => {
                let Some(incoming) = incoming else { break Err(anyhow::anyhow!("endpoint closed")) };
                let Some((incoming, mut handshaking)) = pending.screen(incoming) else {
                    continue;
                };
                let (server, offering) = (server.clone(), offering.clone());
                tasks.spawn(async move {
                    let Some((conn, handshake)) =
                        slot::accept_stream(&server, incoming, "send", &mut handshaking).await
                    else {
                        return;
                    };
                    drop(handshaking);
                    if let Err(error) = offering.serve(&conn, handshake).await {
                        detail!("send: {error:#}");
                    }
                    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                });
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            Ok(_) = deadlines.changed() => {}
            Ok(outcome) = finished.wait_for(Option::is_some) => {
                break outcome.clone().expect("checked").map_err(anyhow::Error::msg);
            }
            () = expired => break Err(anyhow::anyhow!(
                "file transfer timed out: the receiver did not confirm in time; increase --transfer-timeout for large transfers"
            )),
        }
    };
    ep.close().await;
    offering.prepared.store.shutdown().await?;
    result
}

impl Offering {
    /// Pins the offer to `peer` unless another receiver has it; returns that
    /// receiver's name. The reception deadline starts with the pin.
    fn pin(&self, peer: &slot::Admitted) -> Option<String> {
        let mut pinned = self.pinned.lock().unwrap_or_else(|e| e.into_inner());
        match pinned.as_ref() {
            Some((machine, _)) if *machine == peer.machine => None,
            Some((_, receiver)) => Some(receiver.clone()),
            None => {
                *pinned = Some((peer.machine, peer.name.clone()));
                self.deadline
                    .send_replace(Some(tokio::time::Instant::now() + self.transfer_timeout));
                None
            }
        }
    }

    /// Serves the offer to one authenticated receiver until it tells the result.
    async fn serve(&self, conn: &Connection, handshake: Handshake) -> Result<()> {
        let Handshake {
            mut send,
            mut recv,
            peer,
        } = handshake;
        let peer = peer.context("send --peers-only admits proven machines only")?;
        let (who, name) = (peer.name.as_str(), self.name.as_str());
        let _serving = if self.share {
            match self.serving.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    note!("{who} asked for {name}: too many receivers at once; refused");
                    return write(
                        &mut send,
                        &Message::Refused {
                            reason: Reason::Full,
                        },
                    )
                    .await;
                }
            }
        } else {
            // The first admitted receiver is pinned until it tells its result;
            // it alone may resume.
            if let Some(receiver) = self.pin(&peer) {
                note!("refused {who}: {name} is reserved for {receiver}");
                return write(
                    &mut send,
                    &Message::Refused {
                        reason: Reason::Reserved,
                    },
                )
                .await;
            }
            None
        };
        write(
            &mut send,
            &Message::Offer {
                version: VERSION,
                hash: self.prepared.hash().to_hex().to_string(),
                name: self.prepared.name.clone(),
                bytes: self.prepared.bytes,
                files: self.prepared.files as u64,
            },
        )
        .await?;
        note!("{who} receives {name}");
        // Concurrent receptions of a share would garble a progress line.
        let mut progress = Progress::new("send", !self.no_progress && !self.share);
        progress.step(
            format!("sending {name} to {who}"),
            Some(self.prepared.bytes),
        );
        let mut served = 0;
        let verdict = {
            let serving = files::serve_counting(
                &self.prepared.store,
                conn.clone(),
                &mut progress,
                &mut served,
            );
            tokio::select! {
                message = read(&mut recv) => message.ok(),
                _ = serving => None,
            }
        };
        progress.finish();
        let outcome = match verdict {
            Some(Message::Received {}) => {
                note!("{who} received {name}: BLAKE3 verified and published");
                Ok(())
            }
            Some(Message::Identical {}) => {
                note!("{who} already holds identical files; nothing was sent again");
                Ok(())
            }
            Some(Message::Declined { reason }) => {
                let reason = reason.declined();
                note!("{who} did not receive {name}: {reason}");
                Err(format!("{who} did not receive {name}: {reason}"))
            }
            _ => {
                // The pinned receiver may come back to resume.
                note!(
                    "the connection to {who} was lost before it confirmed the result; the outcome is unknown"
                );
                return Ok(());
            }
        };
        if outcome.is_ok() {
            let _ = write(&mut send, &Message::Thanks {}).await;
            let _ = send.finish();
        }
        if !self.share {
            self.done.send_replace(Some(outcome));
        }
        Ok(())
    }
}

// --- receive peer:NAME -----------------------------------------------------

/// The paired machine behind a `receive TARGET` that is not a code.
fn sender_target(value: &str) -> Result<Target> {
    match target::parse(value)? {
        target @ Target::Peer { .. } => Ok(target),
        _ => bail!(target::CODE_ONLY),
    }
}

/// `receive peer:NAME`: fetches what a paired machine offers with
/// `send PATH --peers-only`, then exits.
pub async fn receive_from(
    settings: &Settings,
    target: &str,
    addrs: &[String],
    output: &Path,
    options: ReceiveOptions,
) -> Result<()> {
    let destination = Destination {
        target: sender_target(target)?,
        addrs: target::direct_addrs(addrs).await?,
    };
    let remote =
        destination
            .target
            .remote("receive", Protocol::Send, &destination.addrs, settings)?;
    files::check_destination(output)?;
    let peer = remote.peer.clone();
    let mut progress = Progress::new("receive", !options.no_progress);
    let ep = target::bind_client(settings, &destination.target).await?;
    let result = async {
        progress.phase(format!("connecting to {peer}"), None);
        let (conn, handshake) = progress
            .wait(slot::connect_stream(&ep, &remote, Protocol::Send, settings))
            .await
            .map_err(|error| {
                if error.is::<slot::Unavailable>() {
                    not_sending(&remote)
                } else {
                    error
                }
            })?;
        let Handshake {
            mut send, mut recv, ..
        } = handshake;
        let offer = tokio::time::timeout(settings.timeout, read(&mut recv))
            .await
            .with_context(|| format!("{peer} sent no offer in time"))??;
        if let Message::Refused { reason } = offer {
            bail!(reason.unserved(&peer));
        }
        let offer = check_offer(offer)?;
        let name = offer.name.clone();
        let decline = |reason| Message::Declined { reason };
        if offer.bytes > options.max_bytes {
            let _ = write(&mut send, &decline(Reason::TooLarge)).await;
            bail!(
                "{name} from {peer} is larger than --max-bytes; check the expected size, then deliberately raise --max-bytes"
            );
        }
        note!("receiving {name} from {peer}");
        let reception = PeerReception {
            conn: &conn,
            machine: &remote.address.machine,
            hash: offer.hash,
            resume: "run the same command again while the sender still offers it",
            // Received before, and the confirmation was lost: say so, change nothing.
            identical_ends: true,
        };
        let received =
            files::receive_offer(settings, reception, output, &options, &mut progress).await;
        let error = match received {
            Ok(()) => {
                write(&mut send, &Message::Received {}).await?;
                let _ = tokio::time::timeout(THANKS_DEADLINE, read(&mut recv)).await;
                conn.close(0u32.into(), b"received");
                return Ok(());
            }
            Err(error) => error,
        };
        match error.downcast_ref::<Refusal>().copied() {
            Some(Refusal::Identical) => {
                write(&mut send, &Message::Identical {}).await?;
                let _ = tokio::time::timeout(THANKS_DEADLINE, read(&mut recv)).await;
                conn.close(0u32.into(), b"identical");
                note!(
                    "identical files are already in {}; nothing changed",
                    output.display()
                );
                Ok(())
            }
            Some(refusal) => {
                let _ = write(&mut send, &decline(Reason::of(refusal))).await;
                Err(error)
            }
            None if files::is_lost(&error) => Err(error.context(format!(
                "the connection to {peer} was lost; run the same command again while {peer} still offers {name}"
            ))),
            None => {
                let _ = write(&mut send, &decline(Reason::Failed)).await;
                Err(error)
            }
        }
    }
    .await;
    ep.close().await;
    result
}

/// The hint when nothing answers at the sender's slot.
fn not_sending(remote: &Remote) -> anyhow::Error {
    let peer = &remote.peer;
    let slot = if remote.slot == Protocol::Send.default_slot() {
        String::new()
    } else {
        format!(" --slot {}", remote.slot)
    };
    anyhow::anyhow!(
        "{peer} is not sending; run 'iroh-toolbox send PATH --peers-only{slot}' on {peer}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_reject_unknown_fields() {
        for message in [
            Message::Offer {
                version: VERSION,
                hash: "00".repeat(32),
                name: "photos".into(),
                bytes: 3,
                files: 1,
            },
            Message::Wait {},
            Message::Refused {
                reason: Reason::Closing,
            },
            Message::Identical {},
            Message::Received {},
            Message::Thanks {},
        ] {
            let bytes = serde_json::to_vec(&message).unwrap();
            assert_eq!(serde_json::from_slice::<Message>(&bytes).unwrap(), message);
        }
        for bad in [
            r#"{"type":"received","extra":1}"#,
            r#"{"type":"refused","reason":"because I said so"}"#,
            r#"{"type":"shout"}"#,
        ] {
            assert!(serde_json::from_str::<Message>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn offers_are_checked_and_their_names_made_safe() {
        let offer = |hash: String, name: &str, files| Message::Offer {
            version: VERSION,
            hash,
            name: name.into(),
            bytes: 1,
            files,
        };
        let checked = check_offer(offer("ab".repeat(32), "a\u{1b}[2Jb", 1)).unwrap();
        assert_eq!(checked.hash.as_bytes(), &[0xab; 32]);
        assert!(!checked.name.contains('\u{1b}'));
        for bad in [
            offer("ab".repeat(31), "photos", 1),
            offer("zz".repeat(32), "photos", 1),
            offer("ab".repeat(32), "", 1),
            offer("ab".repeat(32), "photos", 0),
            Message::Wait {},
        ] {
            assert!(check_offer(bad).is_err());
        }
    }

    #[test]
    fn refusals_are_explained_locally() {
        let text = Reason::Exists.explain("laptop", "photos");
        assert!(text.contains("laptop refused photos") && text.contains("nothing was replaced"));
        assert!(
            Reason::Closing
                .explain("laptop", "photos")
                .contains("--once")
        );
        let remote = Remote {
            peer: "laptop".into(),
            address: crate::identity::Address::from_parts(
                iroh::SecretKey::from_bytes(&[1; 32]).public(),
                [0; 10],
            ),
            slot: "depot".into(),
            direct: vec![],
        };
        assert_eq!(
            not_receiving(&remote).to_string(),
            "laptop is not receiving; run 'iroh-toolbox receive --inbox --slot depot' on laptop"
        );
    }

    #[test]
    fn file_message_parser_survives_mutated_and_random_input() {
        let hash = "00".repeat(32);
        let offer = format!(
            r#"{{"type":"offer","version":1,"hash":"{hash}","name":"photos","bytes":3,"files":1}}"#
        );
        crate::fuzz::check_decoder(
            &[
                &offer,
                r#"{"type":"wait"}"#,
                r#"{"type":"receiving"}"#,
                r#"{"type":"refused","reason":"too_large"}"#,
                r#"{"type":"identical"}"#,
                r#"{"type":"received"}"#,
                r#"{"type":"declined","reason":"failed"}"#,
                r#"{"type":"thanks"}"#,
            ],
            &[
                ("offer", &["version", "hash", "name", "bytes", "files"]),
                ("wait", &[]),
                ("receiving", &[]),
                ("refused", &["reason"]),
                ("identical", &[]),
                ("received", &[]),
                ("declined", &["reason"]),
                ("thanks", &[]),
            ],
            parse_message,
        );
    }
}
