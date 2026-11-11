//! A two-person iroh conversation. `chat serve` admits one participant: the
//! machine that uses its one-use code, or a paired machine that passes the
//! chat slot's handshake (`--peers-only` opens only the latter).
use std::{
    collections::VecDeque,
    io::IsTerminal,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, ConnectionError, RecvStream, SendStream},
};
use iroh_blobs::{BlobFormat, ticket::BlobTicket};
use reedline::{
    ColumnarMenu, Completer, CompletionResult, Emacs, ExternalPrinter, FileBackedHistory, KeyCode,
    KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, Span, Suggestion, default_emacs_keybindings,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
    task::{AbortHandle, JoinSet},
    time::{Instant, timeout, timeout_at},
};

use crate::{
    cli::{ChatConnectOptions, ChatOptions, ChatServeOptions, Timestamps},
    config::Settings,
    files::{self, PreparedSend},
    network, pairing,
    peers::PeerBook,
    progress::{self, Mark, Progress, safe_text},
    publication::ConfirmationRequest,
    slot::{self, Handshake, Protocol},
    target,
};

const ALPN: &[u8] = b"iroh-toolbox/chat/1";
const FRAME_LIMIT: usize = 64 * 1024;
const TEXT_LIMIT: usize = 4096;
/// How long a lost connection may be re-established before the chat ends.
const RECONNECT_WINDOW: Duration = Duration::from_secs(60);
/// How long a failed write waits for the reader to explain it: a peer that
/// leaves stops reading just after its goodbye, which the reader still delivers.
const WRITE_GRACE: Duration = Duration::from_secs(3);
/// Messages kept for resending until the peer acknowledges them.
const UNACKED_LIMIT: usize = 256;
/// Close code of a connection abandoned for a new one; the chat continues.
const RECONNECTING: u32 = 3;
const COMMANDS: &str = "/send PATH, /cancel, /receive [--force|--no-replace] [--hidden] DESTINATION, /confirm, /reject, /status, /help, /quit. One pending file offer per direction.";
const PROMPT: &str = "you> ";

/// Ctrl+C read by the interactive line editor; the process exits with status 130.
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

static HANDLES_INTERRUPT: AtomicBool = AtomicBool::new(false);

/// Whether an open conversation handles Ctrl+C itself, to tell the peer it
/// leaves instead of letting it wait for a reconnection.
pub fn handles_interrupt() -> bool {
    HANDLES_INTERRUPT.load(Ordering::Relaxed)
}

struct InterruptGuard;

impl InterruptGuard {
    fn new() -> Self {
        HANDLES_INTERRUPT.store(true, Ordering::Relaxed);
        Self
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        HANDLES_INTERRUPT.store(false, Ordering::Relaxed);
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    version: u32,
    message: Message,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    /// Opens each connection; `received` is the last text sequence received,
    /// so the peer resends only what is missing after a reconnection.
    Hello {
        received: u64,
    },
    Text {
        seq: u64,
        text: String,
    },
    Ack {
        seq: u64,
    },
    Offer {
        id: u64,
        name: String,
        bytes: u64,
        files: usize,
    },
    Accept {
        id: u64,
    },
    Ticket {
        id: u64,
        ticket: String,
    },
    Reject {
        id: u64,
    },
    Withdraw {
        id: u64,
    },
    Result {
        id: u64,
        received: bool,
    },
    Ping,
    Quit,
}

fn decode(bytes: &[u8]) -> Result<Message> {
    ensure!(bytes.len() <= FRAME_LIMIT, "chat frame exceeds limit");
    let frame: Frame =
        serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("invalid chat frame"))?;
    ensure!(frame.version == 1, "incompatible chat protocol version");
    match &frame.message {
        Message::Text { seq, text } => ensure!(
            *seq > 0 && !text.is_empty() && text.len() <= TEXT_LIMIT,
            "chat message exceeds limit or is empty"
        ),
        Message::Offer {
            id, name, files, ..
        } => ensure!(
            *id > 0 && !name.is_empty() && name.len() <= 4096 && *files > 0,
            "invalid file offer"
        ),
        _ => {}
    }
    Ok(frame.message)
}

/// Reads one frame: the outer error is a lost stream, the inner one a frame
/// the peer should never have sent.
async fn read_frame(recv: &mut RecvStream) -> Result<Result<Message>> {
    let size = recv.read_u32().await? as usize;
    if size == 0 || size > FRAME_LIMIT {
        return Ok(Err(anyhow::anyhow!("chat frame exceeds limit")));
    }
    let mut bytes = vec![0; size];
    recv.read_exact(&mut bytes).await?;
    Ok(decode(&bytes))
}

async fn write_frame(send: &mut SendStream, message: Message, deadline: Duration) -> Result<()> {
    let bytes = serde_json::to_vec(&Frame {
        version: 1,
        message,
    })?;
    ensure!(bytes.len() <= FRAME_LIMIT, "chat frame exceeds limit");
    timeout(deadline, async {
        send.write_u32(bytes.len() as u32).await?;
        send.write_all(&bytes).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("chat peer is not reading messages")?
}

/// Exchanges the chat greeting on a stream; returns the peer's last received text.
async fn greet(
    send: &mut SendStream,
    recv: &mut RecvStream,
    received: u64,
    deadline: Duration,
) -> Result<u64> {
    write_frame(send, Message::Hello { received }, deadline).await?;
    match timeout(deadline, read_frame(recv))
        .await
        .context("chat greeting timed out")???
    {
        Message::Hello { received } => Ok(received),
        _ => bail!("expected chat greeting"),
    }
}

/// Opens the chat stream of a code connection and greets; returns the peer's
/// last received text.
async fn handshake(
    conn: &Connection,
    joining: bool,
    received: u64,
    deadline: Duration,
) -> Result<(SendStream, RecvStream, u64)> {
    let (mut send, mut recv) = timeout(deadline, async {
        if joining {
            conn.open_bi().await
        } else {
            conn.accept_bi().await
        }
    })
    .await
    .context("chat stream handshake timed out")??;
    let received = greet(&mut send, &mut recv, received, deadline).await?;
    Ok((send, recv, received))
}

/// How the joining side reaches the other again after a loss.
#[derive(Clone)]
enum Redial {
    /// The creator's chat endpoint, learned through the code.
    Code(EndpointAddr),
    /// The paired machine's chat slot: every reconnection passes its handshake.
    Slot(Arc<slot::Remote>),
}

/// Where the serving side accepts its participant's later connections.
enum Acceptor {
    /// Connections from the participant's endpoint, on this chat endpoint.
    Endpoint,
    /// A chat slot: a chat connection passes the slot handshake as the admitted
    /// machine, from the admitted endpoint; file connections come from that
    /// endpoint. Other paired machines learn that the chat is busy.
    Slot {
        server: Arc<slot::Server>,
        machine: iroh::PublicKey,
    },
}

/// `chat serve`: opens a chat to a code, to paired machines, or to both. The
/// first participant admitted on either path is the only one.
pub async fn serve(settings: &Settings, options: ChatServeOptions) -> Result<()> {
    let ChatServeOptions {
        code_words,
        peers_only,
        allow,
        slot,
        port,
        session: options,
    } = options;
    // --allow names the only machines admitted: no code path bypasses it.
    let peers_only = peers_only || !allow.is_empty();
    let timestamps = timestamps_when(
        options.timestamps,
        std::env::var("IROH_TOOLBOX_TIMESTAMPS").ok(),
    )?;
    let pairing = pairing::Options {
        pairing_timeout: options.pairing_timeout,
        no_verify: options.no_verify,
    };
    if peers_only {
        ensure!(
            code_words.is_none() && !pairing.no_verify && pairing.pairing_timeout.is_none(),
            "--code-words, --no-verify and --pairing-timeout apply to the code; `chat serve` with --peers-only or --allow creates none"
        );
    } else {
        pairing.validate(settings).map_err(|error| {
            error.context(
                "a chat with a code needs a relay; `chat serve --peers-only` opens one for paired machines without a relay",
            )
        })?;
    }
    // Without any paired machine, a chat with a code needs no slot.
    let peer_path = peers_only || !PeerBook::load()?.is_empty();
    let server = if peer_path {
        let slot = slot.as_deref().unwrap_or("");
        Some(slot::Server::open(slot, Protocol::Chat, &allow, false)?)
    } else {
        ensure!(
            port.is_none() && slot.is_none(),
            "--slot and --port apply to paired machines, and this machine has none; pair one with `iroh-toolbox pair`"
        );
        note!("No paired machine: this chat is open by code only.");
        None
    };
    let slot_ep = match &server {
        Some(server) => Some(server.bind(settings, port).await?),
        None => None,
    };
    let code = if peers_only {
        None
    } else {
        let words = code_words.map_or(pairing::MIN_WORDS, usize::from);
        let alpns = vec![ALPN.to_vec(), iroh_blobs::ALPN.to_vec()];
        Some(tokio::try_join!(
            network::bind(settings, alpns),
            pairing::open(settings, &pairing, words)
        )?)
    };
    if let (Some(server), Some(slot_ep)) = (&server, &slot_ep) {
        slot::announce(settings, slot_ep, Some(server), "chat connect", "")?;
    }
    let (code_ep, rendezvous) = match code {
        Some((ep, rendezvous)) => (Some(ep), Some(rendezvous)),
        None => (None, None),
    };
    let result = async {
        let mut progress = Progress::new("chat", !options.no_progress);
        let admission = admit(
            settings,
            &pairing,
            code_ep.as_ref().zip(rendezvous),
            server.clone().zip(slot_ep.clone()),
            &mut progress,
        )
        .await?;
        match admission {
            Admission::Code(linked) => {
                let ep = code_ep
                    .as_ref()
                    .context("a code admission has its endpoint")?;
                // The peer path stays shut: paired machines learn that the chat is busy.
                let _busy = slot_ep.as_ref().map(|slot_ep| {
                    network::refuse_while_busy_with(
                        slot_ep,
                        server.clone(),
                        network::CHAT_BUSY_REASON,
                    )
                });
                let session = Session {
                    redial: None,
                    acceptor: Acceptor::Endpoint,
                    admission: "code".into(),
                    name: None,
                };
                conversation(ep, linked, session, settings, &options, timestamps).await
            }
            Admission::Peer {
                conn,
                handshake,
                admitted,
            } => {
                // The code path is shut: its rendezvous ended with its admission.
                if let Some(ep) = &code_ep {
                    ep.close().await;
                }
                let (slot_ep, server) = (
                    slot_ep.as_ref().context("a peer admission has its slot")?,
                    server.clone().context("a peer admission has its slot")?,
                );
                let Handshake {
                    mut send, mut recv, ..
                } = handshake;
                let received = greet(&mut send, &mut recv, 0, settings.timeout).await?;
                let linked = Linked {
                    conn,
                    send,
                    recv,
                    received,
                };
                let session = Session {
                    redial: None,
                    acceptor: Acceptor::Slot {
                        server,
                        machine: admitted.machine,
                    },
                    admission: format!("paired machine {}", admitted.name),
                    name: Some(admitted.name.to_string()),
                };
                conversation(slot_ep, linked, session, settings, &options, timestamps).await
            }
        }
    }
    .await;
    for ep in [code_ep, slot_ep].into_iter().flatten() {
        ep.close().await;
    }
    result
}

/// The participant a chat admitted, by code or as a paired machine.
enum Admission {
    Code(Linked),
    Peer {
        conn: Connection,
        handshake: Handshake,
        admitted: slot::Admitted,
    },
}

/// Admits the first participant on the code path or the peer path, never
/// both: each path claims the session once its participant is authenticated,
/// and the other path then admits nobody. A failed code pairing closes the code
/// path only, while paired machines may still join.
async fn admit(
    settings: &Settings,
    pairing: &pairing::Options,
    code: Option<(&Endpoint, pairing::Rendezvous)>,
    peer: Option<(Arc<slot::Server>, Endpoint)>,
    progress: &mut Progress,
) -> Result<Admission> {
    let claimed = AtomicBool::new(false);
    let claim = || !claimed.swap(true, Ordering::SeqCst);
    let (mut by_code, by_peer) = (code.is_some(), peer.is_some());
    let code_admission = async {
        let (ep, rendezvous) = code.context("no code path")?;
        let side = pairing::Side::Create(rendezvous);
        let (peer, _) = pairing::pair_chat(settings, pairing, ep, side, progress).await?;
        ensure!(claim(), "a paired machine joined first");
        let conn = timeout(settings.timeout, async {
            loop {
                let conn = files::accept_receiver(ep, peer).await?;
                if conn.alpn() == ALPN {
                    break Ok::<_, anyhow::Error>(conn);
                }
                conn.close(1u32.into(), b"expected chat protocol");
            }
        })
        .await
        .context("paired chat peer did not connect over iroh")??;
        detail!("Pairing complete.");
        let (send, recv, received) = handshake(&conn, false, 0, settings.timeout).await?;
        Ok(Admission::Code(Linked {
            conn,
            send,
            recv,
            received,
        }))
    };
    let peer_admission = async {
        let (server, ep) = peer.context("no peer path")?;
        let pending = network::Pending::default();
        let mut attempts = JoinSet::new();
        loop {
            tokio::select! {
                incoming = ep.accept() => {
                    let incoming = incoming.context("chat endpoint closed")?;
                    let Some((incoming, mut handshaking)) = pending.screen(incoming) else {
                        continue;
                    };
                    let server = server.clone();
                    attempts.spawn(async move {
                        slot::accept_stream(&server, incoming, "chat", &mut handshaking).await
                    });
                }
                Some(done) = attempts.join_next(), if !attempts.is_empty() => {
                    let Ok(Some((conn, handshake))) = done else { continue };
                    let Some(admitted) = handshake.peer.clone() else { continue };
                    if claim() {
                        return Ok(Admission::Peer { conn, handshake, admitted });
                    }
                    conn.close(network::BUSY.into(), network::CHAT_BUSY_REASON);
                }
            }
        }
    };
    tokio::pin!(code_admission, peer_admission);
    loop {
        tokio::select! {
            result = &mut code_admission, if by_code => match result {
                Ok(admission) => return Ok(admission),
                Err(error) if by_peer && !claimed.load(Ordering::SeqCst) => {
                    note!("code pairing failed: {}; paired machines may still join", settings.redact(format!("{error:#}")));
                    by_code = false;
                }
                Err(error) => return Err(error),
            },
            result = &mut peer_admission, if by_peer => return result,
        }
    }
}

/// `chat connect`: joins a chat by its code, or a paired machine's chat.
pub async fn connect(settings: &Settings, options: ChatConnectOptions) -> Result<()> {
    let ChatConnectOptions {
        target,
        addr,
        session: options,
    } = options;
    if target.starts_with("peer:") {
        ensure!(
            !options.no_verify && options.pairing_timeout.is_none(),
            "--no-verify and --pairing-timeout apply to code pairing; a paired machine proves its key"
        );
        return join_peer(settings, &target, &addr, options).await;
    }
    ensure!(
        addr.is_empty(),
        "--addr applies to peer: targets only; a code finds the other machine by itself"
    );
    let code = target::parse_code(&target, "chat connect")?;
    join_by_code(settings, code, options).await
}

/// The hint when nothing answers at a paired machine's chat slot.
fn not_chatting(remote: &slot::Remote) -> anyhow::Error {
    let peer = &remote.peer;
    let slot = if remote.slot == Protocol::Chat.default_slot() {
        String::new()
    } else {
        format!(" --slot {}", remote.slot)
    };
    anyhow::anyhow!("{peer} has no chat open; run 'iroh-toolbox chat serve{slot}' on {peer}")
}

async fn join_peer(
    settings: &Settings,
    target: &str,
    addrs: &[String],
    options: ChatOptions,
) -> Result<()> {
    let timestamps = timestamps_when(
        options.timestamps,
        std::env::var("IROH_TOOLBOX_TIMESTAMPS").ok(),
    )?;
    let destination = target::Destination::parse(target, addrs).await?;
    let remote = Arc::new(destination.target.remote(
        "chat connect",
        Protocol::Chat,
        &destination.addrs,
        settings,
    )?);
    // The other side downloads this side's file offers from this endpoint.
    let alpns = vec![iroh_blobs::ALPN.to_vec()];
    let ep = if settings.direct_only {
        network::bind(settings, alpns).await?
    } else {
        network::bind_resolving(settings, alpns).await?
    };
    let result = async {
        let peer = &remote.peer;
        let (conn, handshake) = slot::connect_stream(&ep, &remote, Protocol::Chat, settings)
            .await
            .map_err(|error| {
                if error.is::<slot::Unavailable>() {
                    not_chatting(&remote)
                } else {
                    error
                }
            })?;
        let Handshake {
            mut send, mut recv, ..
        } = handshake;
        let received = match greet(&mut send, &mut recv, 0, settings.timeout).await {
            Ok(received) => received,
            Err(error) => {
                // A busy chat ends the handshake stream before it closes the
                // connection with its reason: wait for that reason.
                let closed = timeout(Duration::from_secs(1), conn.closed()).await;
                return Err(match closed {
                    Ok(ConnectionError::ApplicationClosed(close))
                        if u64::from(close.error_code) == u64::from(network::BUSY) =>
                    {
                        anyhow::anyhow!("{peer} {}", network::busy_message(&close.reason))
                    }
                    _ => error.context(format!("{peer} did not complete the chat greeting")),
                });
            }
        };
        let linked = Linked {
            conn,
            send,
            recv,
            received,
        };
        let session = Session {
            redial: Some(Redial::Slot(remote.clone())),
            acceptor: Acceptor::Endpoint,
            admission: format!("paired machine {peer}"),
            name: Some(peer.to_string()),
        };
        conversation(&ep, linked, session, settings, &options, timestamps).await
    }
    .await;
    ep.close().await;
    result
}

/// The forms of `chat` without a role: refused, naming the subcommand to use.
/// A role is never guessed from the presence of a target.
pub fn without_role(target: Option<&str>, peers_only: bool) -> anyhow::Error {
    let advice = match target {
        Some(target) if target.starts_with("code:") => {
            "join with `iroh-toolbox chat connect code:` (it asks for the words), or open a chat with `iroh-toolbox chat serve`"
        }
        Some(target) if target.starts_with("peer:") => {
            "join a paired machine's chat with `iroh-toolbox chat connect peer:NAME`"
        }
        _ if peers_only => {
            "open a chat for paired machines with `iroh-toolbox chat serve --peers-only`"
        }
        _ => "`iroh-toolbox chat serve` opens a chat, `iroh-toolbox chat connect code:` joins one",
    };
    anyhow::anyhow!("chat needs a role: {advice}")
}

/// Joins the chat of a code.
async fn join_by_code(settings: &Settings, code: String, options: ChatOptions) -> Result<()> {
    let timestamps = timestamps_when(
        options.timestamps,
        std::env::var("IROH_TOOLBOX_TIMESTAMPS").ok(),
    )?;
    let pairing = pairing::Options {
        pairing_timeout: options.pairing_timeout,
        no_verify: options.no_verify,
    };
    let code = target::complete_code(code, "chat connect").await?;
    pairing::check_code(&code)?;
    pairing.validate(settings)?;
    // The creator is located by a derived endpoint ID from a temporary
    // relay-only endpoint; the chat endpoint needs no lookup, since the
    // creator's ticket carries its addresses.
    let alpns = vec![ALPN.to_vec(), iroh_blobs::ALPN.to_vec()];
    let (ep, joiner) = tokio::try_join!(
        network::bind(settings, alpns),
        pairing::prepare_join(settings)
    )?;
    let result = async {
        let mut progress = Progress::new("chat", !options.no_progress);
        let side = pairing::Side::Join(&code, joiner);
        let (_, addr) = pairing::pair_chat(settings, &pairing, &ep, side, &mut progress).await?;
        let addr = addr.context("the creator's address comes with the code")?;
        let conn = timeout(settings.timeout, ep.connect(addr.clone(), ALPN))
            .await
            .context("paired chat peer did not connect over iroh")??;
        detail!("Pairing complete.");
        let (send, recv, received) = handshake(&conn, true, 0, settings.timeout).await?;
        let linked = Linked {
            conn,
            send,
            recv,
            received,
        };
        // The joining side keeps the creator's address to redial it.
        let session = Session {
            redial: Some(Redial::Code(addr)),
            acceptor: Acceptor::Endpoint,
            admission: "code".into(),
            name: None,
        };
        conversation(&ep, linked, session, settings, &options, timestamps).await
    }
    .await;
    ep.close().await;
    result
}

struct Outgoing {
    id: u64,
    prepared: Arc<PreparedSend>,
    accepted: bool,
    serving: Option<Connection>,
    expires: Instant,
}

struct Incoming {
    id: u64,
    /// Display name, already made safe for the terminal.
    name: String,
    bytes: u64,
    requested: Option<PathBuf>,
    download: Option<AbortHandle>,
    expires: Option<Instant>,
    replacement: files::Replacement,
    hidden: bool,
    confirmation: Option<tokio::sync::oneshot::Sender<bool>>,
}

/// The current chat connection; `generation` tells its events from older ones.
struct Link {
    conn: Connection,
    send: SendStream,
    generation: u64,
    reader: AbortHandle,
    /// A write failed at this time; the reader or [`WRITE_GRACE`] concludes.
    broken: Option<(Instant, anyhow::Error)>,
}

/// A connection whose chat handshake succeeded, ready to replace a lost one.
struct Linked {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    received: u64,
}

enum Event {
    Message {
        generation: u64,
        message: Message,
    },
    /// The peer sent a frame that cannot be decoded.
    Invalid {
        generation: u64,
        error: anyhow::Error,
    },
    /// The chat stream or its connection ended without a goodbye.
    Lost {
        generation: u64,
        error: anyhow::Error,
    },
    Linked(Linked),
    Connection(Connection),
    /// The admitted machine reconnected through the chat slot's handshake.
    Rejoined {
        conn: Connection,
        handshake: Handshake,
    },
    Prepared {
        id: u64,
        result: Result<PreparedSend>,
    },
    Downloaded {
        id: u64,
        result: Result<()>,
    },
    Served {
        id: u64,
        result: Result<()>,
    },
}

/// The set owns every background future; dropping the conversation cancels all work.
struct Conversation<'a> {
    ep: &'a Endpoint,
    settings: &'a Settings,
    options: &'a ChatOptions,
    peer: EndpointId,
    /// How the joining side reconnects; `None` on the serving side.
    redial: Option<Redial>,
    /// How the participant was admitted, for `/status`.
    admission: String,
    /// `peer`, or the local name of a paired machine.
    who: String,
    link: Option<Link>,
    generation: u64,
    /// When the connection was lost, until a new one is linked.
    lost: Option<Instant>,
    linking: Option<AbortHandle>,
    jobs: JoinSet<Result<()>>,
    tx: mpsc::Sender<Event>,
    outgoing: Option<Outgoing>,
    incoming: Option<Incoming>,
    preparing: Option<(u64, AbortHandle, PathBuf)>,
    next_id: u64,
    last_incoming: u64,
    confirmations: mpsc::Sender<ConfirmationRequest>,
    /// Last text sequence sent, and texts the peer has not acknowledged yet.
    sent: u64,
    unacked: VecDeque<(u64, String)>,
    /// Last text sequence received; resent duplicates are ignored.
    received: u64,
}

/// How a conversation reconnects and accepts its participant's connections.
struct Session {
    redial: Option<Redial>,
    acceptor: Acceptor,
    /// How the participant was admitted: `code`, or `paired machine NAME`
    /// when its machine key was verified. Never a name it chose for itself.
    admission: String,
    /// The local name of a paired machine, shown before its messages.
    name: Option<String>,
}

async fn conversation(
    ep: &Endpoint,
    first: Linked,
    session: Session,
    settings: &Settings,
    options: &ChatOptions,
    timestamps: Timestamps,
) -> Result<()> {
    let Session {
        redial,
        acceptor,
        admission,
        name,
    } = session;
    let peer = first.conn.remote_id();
    let (tx, mut rx) = mpsc::channel(16);
    let (confirmations, mut confirmation_requests) = mpsc::channel::<ConfirmationRequest>(1);
    let timestamps = match timestamps {
        Timestamps::Auto => interactive_terminal(),
        when => when == Timestamps::Always,
    };
    let color = interactive_terminal() && progress::color_wanted();
    let mut input = Input::open(timestamps, color)?;
    let _style = progress::style_chat(progress::ChatStyle { timestamps, color });
    let mut chat = Conversation {
        ep,
        settings,
        options,
        peer,
        redial,
        admission,
        who: name.unwrap_or_else(|| "peer".to_owned()),
        link: None,
        generation: 0,
        lost: None,
        linking: None,
        jobs: JoinSet::new(),
        tx,
        outgoing: None,
        incoming: None,
        preparing: None,
        next_id: 1,
        last_incoming: 0,
        confirmations,
        sent: 0,
        unacked: VecDeque::new(),
        received: 0,
    };
    chat.install(first.conn, first.send, first.recv);
    let tx = chat.tx.clone();
    let endpoint = ep.clone();
    match acceptor {
        Acceptor::Endpoint => chat.jobs.spawn(async move {
            loop {
                let conn = files::accept_receiver(&endpoint, peer).await?;
                tx.send(Event::Connection(conn)).await?;
            }
        }),
        Acceptor::Slot { server, machine } => chat
            .jobs
            .spawn(accept_on_slot(endpoint, server, machine, peer, tx)),
    };
    note!(
        "Chat ready; admission: {}. Type a message or {COMMANDS}",
        chat.admission
    );
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut heartbeat = Instant::now();
    let _interrupt = InterruptGuard::new();
    let mut interrupt = std::pin::pin!(tokio::signal::ctrl_c());
    let result = async {
      loop {
        tokio::select! {
            _ = &mut interrupt => { chat.leave().await; return Err(Interrupted.into()); }
            event = input.next() => match event? {
                InputEvent::Eof => { chat.leave().await; break; }
                InputEvent::Interrupted => { chat.leave().await; return Err(Interrupted.into()); }
                InputEvent::Invalid(error) => alert!("{error}"),
                InputEvent::Line(line) => {
                    if line == "/quit" { chat.leave().await; break; }
                    if let Err(error) = chat.command(&line).await { alert!("{}", settings.redact(format!("{error:#}"))); }
                }
            },
            event = rx.recv() => {
                if chat.event(event.context("chat event channel closed")?).await? { break; }
            }
            Some(request) = confirmation_requests.recv() => {
                if let Some(offer) = chat.incoming.as_mut().filter(|offer| offer.id == request.id && offer.download.is_some()) {
                    request.existing.describe();
                    note!("Use /confirm to replace or merge these items, or /reject to keep the destination unchanged.");
                    offer.confirmation = Some(request.reply);
                } else { let _ = request.reply.send(false); }
            }
            job = chat.jobs.join_next(), if !chat.jobs.is_empty() => {
                if let Some(result) = job {
                    match result {
                        Err(error) if error.is_cancelled() => continue,
                        other => other??,
                    }
                }
            }
            _ = tick.tick() => {
                input.refresh();
                if chat.lost.is_some_and(|lost| lost.elapsed() >= RECONNECT_WINDOW) {
                    chat.report_undelivered();
                    bail!("{} unreachable for {} s; chat ended", chat.who, RECONNECT_WINDOW.as_secs());
                }
                let broken = chat.link.as_mut().and_then(|link| {
                    link.broken.take_if(|(at, _)| at.elapsed() >= WRITE_GRACE)
                });
                if let Some((_, error)) = broken {
                    chat.lose(&error).await;
                }
                if chat.link.is_some() && heartbeat.elapsed() >= Duration::from_secs(10) {
                    chat.send(Message::Ping).await;
                    heartbeat = Instant::now();
                }
                if chat.outgoing.as_ref().is_some_and(|offer| Instant::now() >= offer.expires) {
                    let id = chat.outgoing.as_ref().unwrap().id;
                    chat.clear_outgoing().await;
                    chat.send(Message::Withdraw { id }).await;
                    alert!("File offer expired; use /send to try again.");
                }
                if chat.incoming.as_ref().and_then(|o| o.expires).is_some_and(|expires| Instant::now() >= expires) {
                    let offer = chat.incoming.take().unwrap();
                    if let Some(job) = offer.download { job.abort(); }
                    chat.send(Message::Result { id: offer.id, received: false }).await;
                    alert!("File reception timed out; no successful publication was confirmed.");
                }
            }
        }
      }
      Ok::<_, anyhow::Error>(())
    }.await;
    chat.jobs.abort_all();
    while chat.jobs.join_next().await.is_some() {}
    chat.clear_outgoing().await;
    if let Some(mut link) = chat.link.take() {
        if result.is_ok() || result.as_ref().is_err_and(|e| e.is::<Interrupted>()) {
            let _ = link.send.finish();
            let _ = timeout(Duration::from_secs(1), link.send.stopped()).await;
        }
        link.conn.close(0u32.into(), b"chat ended");
    }
    input.close().await;
    result
}

/// Accepts, on a chat slot, the admitted participant's reconnections (slot
/// handshake as `machine`, from `endpoint`) and its file connections (from
/// `endpoint`). Other paired machines pass their handshake and learn that the
/// chat is busy; a removed machine is refused by the handshake itself.
async fn accept_on_slot(
    ep: Endpoint,
    server: Arc<slot::Server>,
    machine: iroh::PublicKey,
    endpoint: EndpointId,
    tx: mpsc::Sender<Event>,
) -> Result<()> {
    let pending = network::Pending::default();
    let mut attempts = JoinSet::new();
    loop {
        tokio::select! {
            incoming = ep.accept() => {
                let Some(incoming) = incoming else { return Ok(()) };
                let Some((incoming, mut handshaking)) = pending.screen(incoming) else {
                    continue;
                };
                let (server, tx) = (server.clone(), tx.clone());
                attempts.spawn(async move {
                    let accepted = slot::accept_with(incoming, "chat", &mut handshaking, |conn| async move {
                        if conn.alpn() == iroh_blobs::ALPN {
                            ensure!(conn.remote_id() == endpoint, "file connection from another endpoint");
                            return Ok(None);
                        }
                        Ok(Some(server.authenticate(&conn, network::HANDSHAKE_DEADLINE).await?))
                    })
                    .await;
                    drop(handshaking);
                    match accepted {
                        Some((conn, None)) => {
                            let _ = tx.send(Event::Connection(conn)).await;
                        }
                        Some((conn, Some(handshake))) => {
                            let admitted = handshake.peer.as_ref().is_some_and(|peer| peer.machine == machine);
                            if admitted && conn.remote_id() == endpoint {
                                let _ = tx.send(Event::Rejoined { conn, handshake }).await;
                            } else {
                                conn.close(network::BUSY.into(), network::CHAT_BUSY_REASON);
                            }
                        }
                        None => {}
                    }
                });
            }
            Some(_) = attempts.join_next(), if !attempts.is_empty() => {}
        }
    }
}

impl Conversation<'_> {
    /// Reads the chat stream of a new connection, which becomes the current one.
    fn install(&mut self, conn: Connection, send: SendStream, mut recv: RecvStream) {
        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        let reader = self.jobs.spawn(async move {
            loop {
                let event = match read_frame(&mut recv).await {
                    Ok(Ok(message)) => {
                        let quitting = matches!(message, Message::Quit);
                        tx.send(Event::Message {
                            generation,
                            message,
                        })
                        .await?;
                        if quitting {
                            return Ok(());
                        }
                        continue;
                    }
                    Ok(Err(error)) => Event::Invalid { generation, error },
                    Err(error) => Event::Lost { generation, error },
                };
                tx.send(event).await?;
                return Ok(());
            }
        });
        self.link = Some(Link {
            conn,
            send,
            generation,
            reader,
            broken: None,
        });
    }

    fn current(&self, generation: u64) -> bool {
        self.link
            .as_ref()
            .is_some_and(|link| link.generation == generation)
    }

    /// Closes the current connection and cancels the file work bound to it.
    async fn abandon(&mut self) -> bool {
        let Some(link) = self.link.take() else {
            return false;
        };
        link.reader.abort();
        link.conn.close(RECONNECTING.into(), b"reconnecting");
        let mut cancelled = self.outgoing.is_some() || self.incoming.is_some();
        if let Some((_, job, _)) = self.preparing.take() {
            job.abort();
            cancelled = true;
        }
        self.clear_outgoing().await;
        if let Some(job) = self.incoming.take().and_then(|offer| offer.download) {
            job.abort();
        }
        if cancelled {
            alert!(
                "File offers cancelled by the connection loss; no successful publication was confirmed."
            );
        }
        true
    }

    /// Handles a lost connection: texts are kept for resending and the
    /// joining side redials the creator, which waits for it.
    async fn lose(&mut self, error: &anyhow::Error) {
        if !self.abandon().await {
            return;
        }
        detail!(
            "chat connection lost: {}",
            self.settings.redact(format!("{error:#}"))
        );
        self.lost = Some(Instant::now());
        let window = RECONNECT_WINDOW.as_secs();
        let Some(redial) = self.redial.clone() else {
            alert!(
                "Connection lost; waiting up to {window} s for {} to reconnect…",
                self.who
            );
            return;
        };
        alert!("Connection lost; reconnecting for up to {window} s…");
        let ep = self.ep.clone();
        let tx = self.tx.clone();
        let settings = self.settings.clone();
        let received = self.received;
        let job = self.jobs.spawn(async move {
            loop {
                let attempt = timeout(settings.timeout, async {
                    let (conn, send, recv, received) = match &redial {
                        Redial::Code(addr) => {
                            let conn = ep.connect(addr.clone(), ALPN).await?;
                            let (send, recv, received) =
                                handshake(&conn, true, received, settings.timeout).await?;
                            (conn, send, recv, received)
                        }
                        // The slot handshake runs again: a removed peer is refused.
                        Redial::Slot(remote) => {
                            let (conn, handshake) =
                                slot::connect_stream(&ep, remote, Protocol::Chat, &settings)
                                    .await?;
                            let Handshake {
                                mut send, mut recv, ..
                            } = handshake;
                            let received =
                                greet(&mut send, &mut recv, received, settings.timeout).await?;
                            (conn, send, recv, received)
                        }
                    };
                    Ok::<_, anyhow::Error>(Linked {
                        conn,
                        send,
                        recv,
                        received,
                    })
                })
                .await
                .context("reconnection attempt timed out")
                .and_then(|r| r);
                match attempt {
                    Ok(linked) => {
                        tx.send(Event::Linked(linked)).await?;
                        return Ok(());
                    }
                    Err(error) => detail!(
                        "chat: reconnection failed: {}",
                        settings.redact(format!("{error:#}"))
                    ),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        if let Some(previous) = self.linking.replace(job) {
            previous.abort();
        }
    }

    /// Writes one message; callers only learn whether the message left. A
    /// failed write marks the link broken and leaves the verdict to the
    /// reader: a peer that left sent its goodbye before it stopped reading.
    /// Without news from the reader within [`WRITE_GRACE`], the tick treats
    /// the connection as lost.
    async fn send(&mut self, message: Message) -> bool {
        let Some(link) = self.link.as_mut().filter(|link| link.broken.is_none()) else {
            return false;
        };
        match write_frame(&mut link.send, message, self.settings.timeout).await {
            Ok(()) => true,
            Err(error) => {
                detail!(
                    "chat: write failed: {}",
                    self.settings.redact(format!("{error:#}"))
                );
                link.broken = Some((Instant::now(), error));
                false
            }
        }
    }

    async fn leave(&mut self) {
        self.send(Message::Quit).await;
        note!("Chat ended.");
    }

    fn connected(&self) -> Result<()> {
        ensure!(
            self.link.is_some(),
            "not connected; waiting for the peer to reconnect"
        );
        Ok(())
    }

    /// The peer as the subject of a sentence: `Peer`, or its paired name as
    /// written in the peer book.
    fn subject(&self) -> &str {
        if self.who == "peer" {
            "Peer"
        } else {
            &self.who
        }
    }

    /// A sentence whose subject is the peer: a paired name keeps its case.
    fn tell(&self, mark: progress::Mark, text: String) {
        progress::emit_verbatim(mark, &text);
    }

    /// Sends a text message, queued for resending until acknowledged.
    async fn say(&mut self, text: String) -> Result<()> {
        ensure!(
            self.unacked.len() < UNACKED_LIMIT,
            "too many unacknowledged messages; nothing was sent"
        );
        self.sent += 1;
        let seq = self.sent;
        self.unacked.push_back((seq, text.clone()));
        if self.link.is_none() {
            note!("Not connected; the message will be sent after reconnection.");
            return Ok(());
        }
        self.send(Message::Text { seq, text }).await;
        Ok(())
    }

    fn report_undelivered(&self) {
        if self.unacked.is_empty() {
            return;
        }
        let texts: Vec<String> = self
            .unacked
            .iter()
            .map(|(_, text)| format!("{:?}", safe_text(text)))
            .collect();
        alert!("Not delivered: {}", texts.join(", "));
    }

    fn ignored(&self, what: &str) {
        alert!("Ignored an unexpected {what} from {}", self.who);
    }

    async fn clear_outgoing(&mut self) {
        if let Some(offer) = self.outgoing.take() {
            if let Some(conn) = &offer.serving {
                conn.close(0u32.into(), b"file offer ended");
            }
            if let Err(error) = offer.prepared.store.shutdown().await {
                alert!(
                    "File store shutdown failed: {}",
                    self.settings.redact(format!("{error:#}"))
                );
            }
        }
    }

    fn status(&self) {
        // Taken first: the first line printed would otherwise show it unlabeled.
        let transfer = progress::take_console_status();
        let connection = match (&self.link, self.lost) {
            (Some(link), _) => network::path_info(&link.conn)["route"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned(),
            (None, Some(lost)) => format!("reconnecting for {} s", lost.elapsed().as_secs()),
            (None, None) => "not connected".to_owned(),
        };
        note!("Peer: admitted by {} ({connection})", self.admission);
        let outgoing = match (&self.preparing, &self.outgoing) {
            (Some((_, _, path)), _) => {
                format!("preparing {:?}", safe_text(&path.display().to_string()))
            }
            (None, Some(offer)) => format!(
                "{:?}, {}, {}",
                safe_text(&offer.prepared.name),
                progress::size(offer.prepared.bytes),
                match (offer.accepted, offer.serving.is_some()) {
                    (false, _) => "waiting for /receive on the peer",
                    (true, false) => "accepted",
                    (true, true) => "transferring",
                }
            ),
            (None, None) => "none".to_owned(),
        };
        note!("Outgoing offer: {outgoing}");
        let incoming = match &self.incoming {
            None => "none".to_owned(),
            Some(offer) => format!(
                "{:?}, {}, {}",
                offer.name,
                progress::size(offer.bytes),
                match (&offer.requested, offer.confirmation.is_some()) {
                    (None, _) => "use /receive DESTINATION or /reject".to_owned(),
                    (Some(_), true) => "use /confirm or /reject".to_owned(),
                    (Some(path), false) => format!(
                        "receiving into {:?}",
                        safe_text(&path.display().to_string())
                    ),
                }
            ),
        };
        note!("Incoming offer: {incoming}");
        note!("Unacknowledged messages: {}", self.unacked.len());
        if let Some(transfer) = transfer {
            note!("Transfer: {transfer}");
        }
    }

    async fn command(&mut self, line: &str) -> Result<()> {
        ensure!(line.len() <= TEXT_LIMIT, "input is limited to 4096 bytes");
        let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match command {
            "/send" => {
                ensure!(
                    self.preparing.is_none() && self.outgoing.is_none(),
                    "a file offer is already pending"
                );
                self.connected()?;
                let path = path_argument(rest, SEND_USAGE)?;
                let id = self.next_id;
                self.next_id = id.checked_add(1).context("file offer counter exhausted")?;
                let tx = self.tx.clone();
                let no_progress = self.options.no_progress;
                let deadline = Duration::from_secs_f64(self.options.transfer_timeout);
                let source = path.clone();
                let job = self.jobs.spawn(async move {
                    let result = timeout(deadline, PreparedSend::new(&source, no_progress))
                        .await
                        .context("file preparation timed out")
                        .and_then(|r| r);
                    tx.send(Event::Prepared { id, result }).await?;
                    Ok(())
                });
                self.preparing = Some((id, job, path));
                note!(
                    "Preparing file offer; the peer must use /receive before any file data is sent."
                );
            }
            "/receive" => {
                let (output, replacement, hidden) = receive_arguments(rest)?;
                let offer = self.incoming.as_mut().context("no incoming file offer")?;
                ensure!(
                    offer.requested.is_none(),
                    "this file offer is already being received"
                );
                offer.requested = Some(output);
                offer.replacement = replacement;
                offer.hidden = hidden;
                offer.expires =
                    Some(Instant::now() + Duration::from_secs_f64(self.options.transfer_timeout));
                let id = offer.id;
                self.send(Message::Accept { id }).await;
            }
            "/reject" => {
                ensure!(rest.trim().is_empty(), "usage: /reject");
                if let Some(confirmation) = self
                    .incoming
                    .as_mut()
                    .and_then(|offer| offer.confirmation.take())
                {
                    let _ = confirmation.send(false);
                    return Ok(());
                }
                let offer = self.incoming.take().context("no incoming file offer")?;
                if offer.requested.is_none() {
                    self.send(Message::Reject { id: offer.id }).await;
                    alert!("File offer rejected.");
                } else {
                    if let Some(job) = offer.download {
                        job.abort();
                    }
                    self.send(Message::Result {
                        id: offer.id,
                        received: false,
                    })
                    .await;
                    alert!("File reception cancelled; no successful publication was confirmed.");
                }
            }
            "/cancel" => {
                ensure!(rest.trim().is_empty(), "usage: /cancel");
                if let Some((_, job, _)) = self.preparing.take() {
                    job.abort();
                    note!("File preparation cancelled.");
                } else {
                    let id = self.outgoing.as_ref().context("no outgoing file offer")?.id;
                    self.clear_outgoing().await;
                    self.send(Message::Withdraw { id }).await;
                    note!("File offer withdrawn; use /send to offer another file.");
                }
            }
            "/confirm" => {
                ensure!(rest.trim().is_empty(), "usage: /confirm");
                let confirmation = self
                    .incoming
                    .as_mut()
                    .and_then(|offer| offer.confirmation.take())
                    .context("no file replacement awaiting confirmation")?;
                confirmation
                    .send(true)
                    .map_err(|_| anyhow::anyhow!("file replacement confirmation expired"))?;
            }
            "/status" => {
                ensure!(rest.trim().is_empty(), "usage: /status");
                self.status();
            }
            "/help" => {
                note!("Type a message or {COMMANDS}");
                note!(
                    "Paths may contain spaces; enclosing quotes are optional. /receive requires an explicit destination directory."
                );
            }
            // Simulates a network loss for reconnection tests.
            #[cfg(debug_assertions)]
            "/test-drop" => self.lose(&anyhow::anyhow!("test connection drop")).await,
            "" => {}
            value if value.starts_with("//") => self.say(line[1..].to_owned()).await?,
            value if value.starts_with('/') => match closest_command(value) {
                Some(command) => bail!("unknown command {value}; did you mean {command}?"),
                None => bail!(
                    "unknown command {value}; use /help (prefix text with // to send a leading slash)"
                ),
            },
            _ => self.say(line.to_owned()).await?,
        }
        Ok(())
    }

    /// Handles one event; returns whether the chat ends.
    async fn event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Message {
                generation,
                message,
            } => {
                if self.current(generation) {
                    return self.message(message).await;
                }
            }
            Event::Invalid { generation, error } => {
                if self.current(generation) {
                    return Err(error.context("peer sent an invalid chat frame"));
                }
            }
            Event::Lost { generation, error } => {
                if !self.current(generation) {
                    return Ok(false);
                }
                let conn = &self.link.as_ref().unwrap().conn;
                // The peer closes with code 0 only when its chat ends.
                if let Some(ConnectionError::ApplicationClosed(close)) = conn.close_reason()
                    && u64::from(close.error_code) == 0
                {
                    self.tell(Mark::Info, format!("{} ended the chat.", self.subject()));
                    self.report_undelivered();
                    return Ok(true);
                }
                self.lose(&error).await;
            }
            Event::Linked(linked) => {
                self.linking = None;
                // The creator may learn of the loss from the reconnection itself.
                self.abandon().await;
                self.install(linked.conn, linked.send, linked.recv);
                self.lost = None;
                self.unacked.retain(|(seq, _)| *seq > linked.received);
                let pending: Vec<_> = self.unacked.iter().cloned().collect();
                for (seq, text) in &pending {
                    let message = Message::Text {
                        seq: *seq,
                        text: text.clone(),
                    };
                    if !self.send(message).await {
                        return Ok(false);
                    }
                }
                match pending.len() {
                    0 => done!("Reconnected."),
                    count => done!("Reconnected; {count} pending message(s) resent."),
                }
            }
            Event::Prepared { id, result } => {
                // A preparation cancelled after completion still reports here.
                if self
                    .preparing
                    .as_ref()
                    .is_none_or(|(current, _, _)| *current != id)
                {
                    if let Ok(prepared) = result {
                        let _ = prepared.store.shutdown().await;
                    }
                    return Ok(false);
                }
                self.preparing = None;
                let prepared = match result {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        alert!(
                            "Cannot offer file: {}",
                            self.settings.redact(format!("{error:#}"))
                        );
                        return Ok(false);
                    }
                };
                let offer = Message::Offer {
                    id,
                    name: prepared.name.clone(),
                    bytes: prepared.bytes,
                    files: prepared.files,
                };
                if !self.send(offer).await {
                    let _ = prepared.store.shutdown().await;
                    alert!("Cannot offer file: the connection was lost.");
                    return Ok(false);
                }
                note!(
                    "File offered: {} — {}, {}; waiting for {} to /receive.",
                    safe_text(&prepared.name),
                    progress::size(prepared.bytes),
                    progress::file_count(prepared.files),
                    self.who
                );
                self.outgoing = Some(Outgoing {
                    id,
                    prepared: Arc::new(prepared),
                    accepted: false,
                    serving: None,
                    expires: Instant::now()
                        + Duration::from_secs_f64(self.options.transfer_timeout),
                });
            }
            Event::Rejoined { conn, handshake } => self.rejoin(conn, handshake),
            Event::Connection(conn) => {
                if conn.alpn() == ALPN {
                    self.relink(conn);
                    return Ok(false);
                }
                let Some(offer) = self
                    .outgoing
                    .as_mut()
                    .filter(|o| o.accepted && o.serving.is_none())
                else {
                    conn.close(1u32.into(), b"no accepted file offer");
                    return Ok(false);
                };
                if conn.alpn() != iroh_blobs::ALPN || conn.remote_id() != self.peer {
                    conn.close(1u32.into(), b"unexpected chat service or identity");
                    return Ok(false);
                }
                let prepared = offer.prepared.clone();
                let id = offer.id;
                offer.serving = Some(conn.clone());
                let tx = self.tx.clone();
                let expires = offer.expires;
                let no_progress = self.options.no_progress;
                self.jobs.spawn(async move {
                    let mut progress = Progress::lines("send", !no_progress);
                    progress.phase("serving accepted chat file offer", None);
                    let result = timeout_at(
                        expires,
                        files::serve_blobs(prepared.store.as_ref(), conn.clone(), &mut progress),
                    )
                    .await
                    .context("chat file transfer timed out")
                    .and_then(|r| r);
                    if result.is_err() {
                        conn.close(1u32.into(), b"file transfer failed");
                    }
                    tx.send(Event::Served { id, result }).await?;
                    Ok(())
                });
            }
            Event::Downloaded { id, result } => {
                if self.incoming.as_ref().is_some_and(|o| o.id == id) {
                    self.incoming = None;
                    self.send(Message::Result {
                        id,
                        received: result.is_ok(),
                    })
                    .await;
                    if let Err(error) = result {
                        alert!(
                            "File reception failed: {}",
                            self.settings.redact(format!("{error:#}"))
                        );
                    }
                }
            }
            Event::Served { id, result } => {
                if self.outgoing.as_ref().is_some_and(|o| o.id == id)
                    && let Err(error) = result
                {
                    alert!(
                        "File serving failed: {}",
                        self.settings.redact(format!("{error:#}"))
                    );
                }
            }
        }
        Ok(false)
    }

    /// The creator accepts a new chat connection from the paired peer, which
    /// reconnects after a loss.
    fn relink(&mut self, conn: Connection) {
        if self.redial.is_some() || conn.remote_id() != self.peer {
            conn.close(1u32.into(), b"unexpected chat connection");
            return;
        }
        let tx = self.tx.clone();
        let settings = self.settings.clone();
        let received = self.received;
        let job = self.jobs.spawn(async move {
            match handshake(&conn, false, received, settings.timeout).await {
                Ok((send, recv, received)) => {
                    tx.send(Event::Linked(Linked {
                        conn,
                        send,
                        recv,
                        received,
                    }))
                    .await?
                }
                Err(error) => {
                    detail!(
                        "chat: reconnection failed: {}",
                        settings.redact(format!("{error:#}"))
                    );
                    conn.close(1u32.into(), b"chat handshake failed");
                }
            }
            Ok(())
        });
        if let Some(previous) = self.linking.replace(job) {
            previous.abort();
        }
    }

    /// The admitted machine came back through the chat slot: greet it on the
    /// handshake stream, which becomes the chat stream.
    fn rejoin(&mut self, conn: Connection, handshake: Handshake) {
        let tx = self.tx.clone();
        let settings = self.settings.clone();
        let received = self.received;
        let job = self.jobs.spawn(async move {
            let Handshake {
                mut send, mut recv, ..
            } = handshake;
            match greet(&mut send, &mut recv, received, settings.timeout).await {
                Ok(received) => {
                    tx.send(Event::Linked(Linked {
                        conn,
                        send,
                        recv,
                        received,
                    }))
                    .await?
                }
                Err(error) => {
                    detail!(
                        "chat: reconnection failed: {}",
                        settings.redact(format!("{error:#}"))
                    );
                    conn.close(1u32.into(), b"chat handshake failed");
                }
            }
            Ok(())
        });
        if let Some(previous) = self.linking.replace(job) {
            previous.abort();
        }
    }

    /// Handles one peer message; returns whether the chat ends. Messages that
    /// do not fit the current state are reported and ignored.
    async fn message(&mut self, message: Message) -> Result<bool> {
        match message {
            Message::Text { seq, text } => {
                if seq > self.received {
                    self.received = seq;
                    progress::said(&self.who, &text);
                }
                // Debug builds only: lets a test leave the chat between a
                // displayed message and its acknowledgement.
                #[cfg(debug_assertions)]
                if let Some(delay) = std::env::var("IROH_TOOLBOX_TEST_CHAT_ACK_DELAY_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                self.send(Message::Ack { seq }).await;
            }
            Message::Ack { seq } => {
                while self.unacked.front().is_some_and(|(sent, _)| *sent <= seq) {
                    self.unacked.pop_front();
                }
            }
            Message::Offer {
                id,
                name,
                bytes,
                files,
            } => {
                if id <= self.last_incoming {
                    self.ignored("file offer");
                    return Ok(false);
                }
                self.last_incoming = id;
                if self.incoming.is_some() || bytes > self.options.max_bytes {
                    self.send(Message::Reject { id }).await;
                    alert!(
                        "Incoming file offer rejected: another offer is pending or it exceeds --max-bytes."
                    );
                } else {
                    let name = safe_text(&name);
                    self.tell(
                        Mark::Info,
                        format!(
                            "{} offers {name} — {}, {}. Use /receive DESTINATION or /reject.",
                            self.subject(),
                            progress::size(bytes),
                            progress::file_count(files),
                        ),
                    );
                    self.incoming = Some(Incoming {
                        id,
                        name,
                        bytes,
                        requested: None,
                        download: None,
                        expires: None,
                        replacement: files::Replacement::Ask,
                        hidden: false,
                        confirmation: None,
                    });
                }
            }
            Message::Accept { id } => {
                if id < self.next_id && self.outgoing.as_ref().is_none_or(|o| o.id > id) {
                    return Ok(false);
                }
                let Some(offer) = self.outgoing.as_mut().filter(|o| o.id == id && !o.accepted)
                else {
                    self.ignored("file acceptance");
                    return Ok(false);
                };
                offer.accepted = true;
                offer.expires =
                    Instant::now() + Duration::from_secs_f64(self.options.transfer_timeout);
                let ticket = offer.prepared.ticket(self.ep).to_string();
                self.send(Message::Ticket { id, ticket }).await;
            }
            Message::Ticket { id, ticket } => {
                if id <= self.last_incoming && self.incoming.as_ref().is_none_or(|o| o.id != id) {
                    return Ok(false);
                }
                if !self
                    .incoming
                    .as_ref()
                    .is_some_and(|o| o.id == id && o.requested.is_some() && o.download.is_none())
                {
                    self.ignored("file ticket");
                    return Ok(false);
                }
                if let Err(error) = self.check_ticket(&ticket) {
                    self.incoming = None;
                    self.send(Message::Result {
                        id,
                        received: false,
                    })
                    .await;
                    alert!(
                        "File reception failed: {}",
                        self.settings.redact(format!("{error:#}"))
                    );
                    return Ok(false);
                }
                let offer = self.incoming.as_mut().unwrap();
                let output = offer.requested.clone().unwrap();
                let max_bytes = self.options.max_bytes.min(offer.bytes);
                let settings = self.settings.clone();
                let ep = self.ep.clone();
                let tx = self.tx.clone();
                let transfer_timeout = self.options.transfer_timeout;
                let expires = offer.expires.context("missing file reception deadline")?;
                let no_progress = self.options.no_progress;
                let replacement = offer.replacement;
                let hidden = offer.hidden;
                let confirmation = Some((id, self.confirmations.clone()));
                offer.download = Some(self.jobs.spawn(async move {
                    let result = timeout_at(
                        expires,
                        files::receive_using(
                            &settings,
                            files::ReceiveTarget::Session(&ticket),
                            &output,
                            files::ReceiveOptions {
                                max_bytes,
                                transfer_timeout,
                                no_progress,
                                replacement,
                                confirmation,
                                hidden,
                            },
                            Some(&ep),
                        ),
                    )
                    .await
                    .context("chat file reception timed out")
                    .and_then(|r| r);
                    tx.send(Event::Downloaded { id, result }).await?;
                    Ok(())
                }));
            }
            Message::Reject { id } => {
                if id < self.next_id && self.outgoing.as_ref().is_none_or(|o| o.id > id) {
                    return Ok(false);
                }
                if !self
                    .outgoing
                    .as_ref()
                    .is_some_and(|o| o.id == id && !o.accepted)
                {
                    self.ignored("file rejection");
                    return Ok(false);
                }
                self.clear_outgoing().await;
                self.tell(
                    Mark::Alert,
                    format!("{} rejected the file offer.", self.subject()),
                );
            }
            Message::Withdraw { id } => {
                if self.incoming.as_ref().is_some_and(|o| o.id == id) {
                    if let Some(job) = self.incoming.take().unwrap().download {
                        job.abort();
                    }
                    self.tell(
                        Mark::Info,
                        format!("{} withdrew the file offer.", self.subject()),
                    );
                }
            }
            Message::Result { id, received } => {
                if self
                    .outgoing
                    .as_ref()
                    .is_some_and(|o| o.id == id && o.accepted)
                {
                    self.clear_outgoing().await;
                    if received {
                        self.tell(
                            Mark::Done,
                            format!(
                                "{} confirms: file received, BLAKE3 verified and published.",
                                self.subject()
                            ),
                        );
                    } else {
                        self.tell(
                            Mark::Alert,
                            format!(
                                "{} could not receive the file; no successful publication was confirmed.",
                                self.subject()
                            ),
                        );
                    }
                }
            }
            Message::Ping => {}
            Message::Hello { .. } => self.ignored("greeting"),
            Message::Quit => {
                self.tell(Mark::Info, format!("{} left the chat.", self.subject()));
                self.report_undelivered();
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The file ticket must come from the paired identity, as a collection
    /// reachable with the local relay selection.
    fn check_ticket(&self, ticket: &str) -> Result<()> {
        let parsed: BlobTicket = ticket
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid chat file ticket"))?;
        ensure!(
            parsed.addr().id == self.peer && parsed.format() == BlobFormat::HashSeq,
            "file ticket differs from paired identity or collection format"
        );
        network::check_addr(parsed.addr(), self.settings)
    }
}

/// Whether stdin, stdout and stderr are all a terminal: the chat then uses a
/// line editor.
fn interactive_terminal() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

/// `--timestamps`, else `IROH_TOOLBOX_TIMESTAMPS`, else `auto`.
fn timestamps_when(flag: Option<Timestamps>, env: Option<String>) -> Result<Timestamps> {
    if let Some(when) = flag {
        return Ok(when);
    }
    match env.filter(|value| !value.is_empty()) {
        None => Ok(Timestamps::Auto),
        Some(value) => <Timestamps as clap::ValueEnum>::from_str(value.trim(), true)
            .map_err(|_| anyhow::anyhow!("IROH_TOOLBOX_TIMESTAMPS must be auto, always or never")),
    }
}

/// Terminal input: a line editor when the terminal is interactive, plain
/// lines otherwise (pipes, scripts, tests).
enum Input {
    Lines {
        stdin: BufReader<tokio::io::Stdin>,
        line: InputLine,
    },
    /// Lines read by `reedline` on its own thread, which blocks on the terminal.
    Editor {
        events: mpsc::Receiver<Result<InputEvent>>,
        /// Makes the editor return and restore the terminal.
        stop: Arc<AtomicBool>,
        thread: std::thread::JoinHandle<()>,
        /// When the transfer progress was last printed.
        progress: Instant,
    },
}

enum InputEvent {
    Line(String),
    /// A line that cannot be sent; reading continues.
    Invalid(anyhow::Error),
    Eof,
    Interrupted,
}

const COMPLETION_MENU: &str = "completion_menu";
/// How often the editor prints queued messages while waiting for a key.
const EDITOR_POLL: Duration = Duration::from_millis(50);

impl Input {
    fn open(timestamps: bool, color: bool) -> Result<Self> {
        if !interactive_terminal() {
            return Ok(Self::Lines {
                stdin: BufReader::new(tokio::io::stdin()),
                line: InputLine::default(),
            });
        }
        let printer = ExternalPrinter::<String>::new(256);
        let sender = printer.sender();
        progress::attach_console(Box::new(move |text| {
            sender
                .send(text.trim_end_matches('\n').to_owned())
                .map_err(std::io::Error::other)
        }));
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu(COMPLETION_MENU.to_owned()),
                ReedlineEvent::MenuNext,
            ]),
        );
        keybindings.add_binding(
            KeyModifiers::SHIFT,
            KeyCode::BackTab,
            ReedlineEvent::MenuPrevious,
        );
        // Without a file, the history stays in memory: messages never reach the disk.
        let history = FileBackedHistory::new(100).map_err(|error| anyhow::anyhow!("{error}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut editor = Reedline::create()
            .with_history(Box::new(history))
            .with_completer(Box::new(Completion))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(
                ColumnarMenu::default().with_name(COMPLETION_MENU),
            )))
            .with_quick_completions(true)
            .with_partial_completions(true)
            .with_edit_mode(Box::new(Emacs::new(keybindings)))
            .with_external_printer(printer)
            .with_ansi_colors(color)
            .with_break_signal(stop.clone())
            .with_poll_interval(EDITOR_POLL);
        let (tx, events) = mpsc::channel(1);
        let thread = std::thread::Builder::new()
            .name("chat input".to_owned())
            .spawn(move || {
                loop {
                    let (event, last) = match editor.read_line(&ChatPrompt { timestamps }) {
                        Ok(Signal::Success(line)) if line.len() > TEXT_LIMIT => (
                            Ok(InputEvent::Invalid(anyhow::anyhow!(
                                "input exceeds 4096 bytes; nothing was sent"
                            ))),
                            false,
                        ),
                        Ok(Signal::Success(line)) => (Ok(InputEvent::Line(line)), false),
                        Ok(Signal::CtrlC(_)) => (Ok(InputEvent::Interrupted), true),
                        Ok(Signal::CtrlD) => (Ok(InputEvent::Eof), true),
                        Ok(Signal::ExternalBreak(_)) => return,
                        Ok(_) => continue,
                        Err(error) => (Err(anyhow::Error::from(error)), true),
                    };
                    if tx.blocking_send(event).is_err() || last {
                        return;
                    }
                }
            })?;
        Ok(Self::Editor {
            events,
            stop,
            thread,
            progress: Instant::now(),
        })
    }

    async fn next(&mut self) -> Result<InputEvent> {
        match self {
            Self::Lines { stdin, line } => Ok(match read_input(stdin, line).await? {
                None => InputEvent::Eof,
                Some(Ok(line)) => InputEvent::Line(line),
                Some(Err(error)) => InputEvent::Invalid(error),
            }),
            Self::Editor { events, .. } => events.recv().await.unwrap_or(Ok(InputEvent::Eof)),
        }
    }

    /// A running transfer prints its progress as a line every 10 seconds.
    fn refresh(&mut self) {
        if let Self::Editor { progress, .. } = self
            && progress.elapsed() >= Duration::from_secs(10)
            && progress::console_status().is_some()
        {
            progress::keep_status();
            *progress = Instant::now();
        }
    }

    /// Stops the editor, which gives the terminal back as it found it.
    async fn close(self) {
        let Self::Editor {
            events,
            stop,
            thread,
            ..
        } = self
        else {
            return;
        };
        progress::detach_console();
        drop(events);
        // The editor checks its stop signal before printing: let it show the
        // last messages first, such as why the chat ended.
        tokio::time::sleep(EDITOR_POLL * 3).await;
        stop.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !thread.is_finished() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Erase the last prompt, which the shell's prompt replaces.
        eprint!("\r\x1b[2K");
    }
}

/// `you> `, after the time when messages show it: rendered again at each
/// key, so a sent line keeps the time it was typed.
struct ChatPrompt {
    timestamps: bool,
}

impl Prompt for ChatPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        if self.timestamps {
            chrono::Local::now().format("%H:%M ").to_string().into()
        } else {
            "".into()
        }
    }

    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        "".into()
    }

    fn render_prompt_indicator(&self, _: PromptEditMode) -> std::borrow::Cow<'_, str> {
        PROMPT.into()
    }

    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        "... ".into()
    }

    fn render_prompt_history_search_indicator(
        &self,
        _: PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        "search> ".into()
    }
}

/// Completes command names, `/receive` options, and paths after `/send` and
/// `/receive`, which accepts only directories.
struct Completion;

impl Completer for Completion {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        CompletionResult::fresh(complete(line, pos))
    }
}

const COMPLETED_COMMANDS: [(&str, bool); 8] = [
    ("/send", true),
    ("/receive", true),
    ("/cancel", false),
    ("/confirm", false),
    ("/reject", false),
    ("/status", false),
    ("/help", false),
    ("/quit", false),
];

fn complete(line: &str, pos: usize) -> Vec<Suggestion> {
    let head = &line[..pos];
    let words = |start: usize, choices: &[(&str, bool)], word: &str| {
        choices
            .iter()
            .filter(|(choice, _)| choice.starts_with(word))
            .map(|(choice, argument)| Suggestion {
                value: (*choice).to_owned(),
                span: Span::new(start, pos),
                append_whitespace: *argument,
                ..Default::default()
            })
            .collect()
    };
    let Some((command, _)) = head.split_once(char::is_whitespace) else {
        if head.starts_with('/') && !head.starts_with("//") {
            return words(0, &COMPLETED_COMMANDS, head);
        }
        return Vec::new();
    };
    let directories = match command {
        "/send" => false,
        "/receive" => true,
        _ => return Vec::new(),
    };
    // The path starts after the command and, for /receive, its options.
    let mut start = command.len();
    loop {
        start += head[start..].len() - head[start..].trim_start().len();
        let rest = &head[start..];
        let Some((word, _)) = rest.split_once(char::is_whitespace) else {
            break;
        };
        if !(directories && word.starts_with("--")) {
            break;
        }
        start += word.len();
    }
    let argument = &head[start..];
    if directories && argument.starts_with('-') {
        return words(
            start,
            &[
                ("--force", true),
                ("--hidden", true),
                ("--no-replace", true),
            ],
            argument,
        );
    }
    complete_path(argument, directories)
        .into_iter()
        .map(|(name, value)| Suggestion {
            value,
            display_override: Some(name),
            span: Span::new(start, pos),
            ..Default::default()
        })
        .collect()
}

/// Lists the entries matching a partly typed path, as (name shown, argument
/// to insert): the argument is quoted when the path needs it.
fn complete_path(argument: &str, directories: bool) -> Vec<(String, String)> {
    let typed = decode_path(argument);
    let separators: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    let (parent, prefix) = typed
        .rfind(separators)
        .map_or(("", typed.as_str()), |i| typed.split_at(i + 1));
    let Ok(entries) = std::fs::read_dir(if parent.is_empty() { "." } else { parent }) else {
        return Vec::new();
    };
    let folded = |name: &str| {
        if cfg!(any(windows, target_os = "macos")) {
            name.to_lowercase()
        } else {
            name.to_owned()
        }
    };
    let mut found: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let visible = !name.starts_with('.') || prefix.starts_with('.');
            let directory = entry.path().is_dir();
            (visible && folded(&name).starts_with(&folded(prefix)) && (directory || !directories))
                .then(|| {
                    let shown = if directory {
                        format!("{name}{}", std::path::MAIN_SEPARATOR)
                    } else {
                        name
                    };
                    let path = format!("{parent}{shown}");
                    (shown, encode_path(&path, !directory))
                })
        })
        .collect();
    found.sort();
    found.truncate(500);
    found
}

/// Writes a path so that [`path_argument`] reads it back: double quotes when
/// it contains spaces, quotes or backslashes, closed after a file name only so
/// that a directory can still be completed.
fn encode_path(path: &str, closed: bool) -> String {
    let special = |c: char| c.is_whitespace() || c == '"' || c == '\'' || (cfg!(unix) && c == '\\');
    if !path.contains(special) {
        return path.to_owned();
    }
    let mut encoded = String::from('"');
    for c in path.chars() {
        if c == '"' || (cfg!(unix) && c == '\\') {
            encoded.push('\\');
        }
        encoded.push(c);
    }
    if closed {
        encoded.push('"');
    }
    encoded
}

/// A partial input line, preserved across select cancellation.
#[derive(Default)]
struct InputLine {
    bytes: Vec<u8>,
    /// Set by an oversized line until its newline is consumed.
    discarding: bool,
}

/// Read one line, bounded before allocation. An oversized or non-UTF-8 line
/// is discarded and reported without ending the input.
async fn read_input(
    input: &mut (impl AsyncBufRead + Unpin),
    line: &mut InputLine,
) -> Result<Option<Result<String>>> {
    loop {
        let bytes = input.fill_buf().await?;
        let count = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        let done = count == 0 || bytes[count - 1] == b'\n';
        if line.bytes.len() + count > TEXT_LIMIT + 2 {
            line.bytes.clear();
            line.discarding = true;
        }
        if !line.discarding {
            line.bytes.extend_from_slice(&bytes[..count]);
        }
        input.consume(count);
        if !done {
            continue;
        }
        if std::mem::take(&mut line.discarding) {
            return Ok(Some(Err(anyhow::anyhow!(
                "input exceeds 4096 bytes; nothing was sent"
            ))));
        }
        if line.bytes.is_empty() {
            return Ok(None);
        }
        return Ok(Some(
            String::from_utf8(std::mem::take(&mut line.bytes))
                .map(|text| text.trim_end_matches(['\r', '\n']).to_owned())
                .map_err(|_| anyhow::anyhow!("input must be UTF-8; nothing was sent")),
        ));
    }
}

fn receive_arguments(value: &str) -> Result<(PathBuf, files::Replacement, bool)> {
    let mut value = value.trim();
    let mut replacement = None;
    let mut hidden = false;
    while value.starts_with("--") {
        let (flag, rest) = value.split_once(char::is_whitespace).unwrap_or((value, ""));
        value = rest.trim_start();
        if flag == "--" {
            break;
        }
        if flag == "--hidden" {
            hidden = true;
            continue;
        }
        ensure!(
            replacement.is_none(),
            "choose either --force or --no-replace"
        );
        replacement = Some(match flag {
            "--force" => files::Replacement::Force,
            "--no-replace" => files::Replacement::Refuse,
            _ => bail!("{RECEIVE_USAGE}"),
        });
    }
    Ok((
        path_argument(value, RECEIVE_USAGE)?,
        replacement.unwrap_or(files::Replacement::Ask),
        hidden,
    ))
}

/// Parses a path as typed, dropped by a file manager or completed: enclosing
/// quotes are optional and the closing one may be missing.
fn path_argument(value: &str, usage: &str) -> Result<PathBuf> {
    let path = decode_path(value.trim());
    ensure!(!path.is_empty(), "{usage}");
    Ok(PathBuf::from(path))
}

const SEND_USAGE: &str = "usage: /send PATH (a file or a directory)";
const RECEIVE_USAGE: &str =
    "usage: /receive [--force|--no-replace] [--hidden] DESTINATION (for example /receive .)";

/// The command a mistyped one most likely meant: at most two edits away, or
/// the only command it starts.
fn closest_command(typed: &str) -> Option<&'static str> {
    let commands = COMPLETED_COMMANDS.iter().map(|(command, _)| *command);
    let (distance, best) = commands
        .clone()
        .map(|command| (edit_distance(typed, command), command))
        .min()?;
    if distance <= 2 {
        return Some(best);
    }
    let mut starting = commands.filter(|command| command.starts_with(typed));
    match (starting.next(), starting.next()) {
        (Some(command), None) => Some(command),
        _ => None,
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(x != *y))
                .min(row[j] + 1)
                .min(above + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

/// Single quotes keep a path literal. In double quotes, `\` escapes the next
/// character; on Windows, where it separates directories, only before `"`.
/// Unquoted on Unix, `\` escapes the next character as in a shell
/// (`My\ photos`, as dropped by macOS terminals).
fn decode_path(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('\'') {
        return inner.strip_suffix('\'').unwrap_or(inner).to_owned();
    }
    let (inner, escapes) = match value.strip_prefix('"') {
        Some(inner) => {
            let closed = inner.strip_suffix('"').filter(|closed| {
                let escaping = closed.len() - closed.trim_end_matches('\\').len();
                cfg!(windows) || escaping % 2 == 0
            });
            (closed.unwrap_or(inner), true)
        }
        None => (value, cfg!(unix)),
    };
    if !escapes {
        return inner.to_owned();
    }
    let mut path = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some(next)) if cfg!(unix) || next == '"' => {
                path.push(next);
                chars.next();
            }
            _ => path.push(c),
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn input_preserves_partial_lines_when_network_events_interrupt_it() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut input = BufReader::new(reader);
        let mut line = InputLine::default();
        writer.write_all(b"/send holiday ").await.unwrap();
        assert!(
            timeout(Duration::from_millis(10), read_input(&mut input, &mut line))
                .await
                .is_err()
        );
        writer.write_all(b"photos\r\n").await.unwrap();
        assert_eq!(
            read_input(&mut input, &mut line)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "/send holiday photos"
        );
    }

    #[tokio::test]
    async fn input_discards_invalid_lines_and_keeps_reading() {
        let mut bytes = vec![b'x'; TEXT_LIMIT + 3];
        bytes.extend_from_slice(b"\n\xff\nnext\n");
        let mut input = BufReader::with_capacity(16, bytes.as_slice());
        let mut line = InputLine::default();
        for expected in ["exceeds 4096 bytes", "must be UTF-8"] {
            let error = read_input(&mut input, &mut line)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
        assert_eq!(
            read_input(&mut input, &mut line)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "next"
        );
        assert!(read_input(&mut input, &mut line).await.unwrap().is_none());
        let bytes = vec![b'x'; TEXT_LIMIT + 3];
        let mut unterminated = BufReader::new(bytes.as_slice());
        assert!(
            read_input(&mut unterminated, &mut line)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(
            read_input(&mut unterminated, &mut line)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn frames_reject_unknown_versions_fields_and_oversized_messages() {
        assert!(decode(br#"{"version":1,"message":{"type":"hello","received":0}}"#).is_ok());
        for bytes in [
            br#"{"version":2,"message":{"type":"hello","received":0}}"#.as_slice(),
            br#"{"version":1,"message":{"type":"hello","received":0},"extra":"secret"}"#,
            br#"{"version":1,"message":{"type":"arbitrary"}}"#,
            br#"{"version":1,"message":{"type":"text","seq":0,"text":"unnumbered"}}"#,
        ] {
            let error = decode(bytes).err().unwrap();
            assert!(!error.to_string().contains("secret"));
        }
        let frame = serde_json::to_vec(&Frame {
            version: 1,
            message: Message::Text {
                seq: 1,
                text: "x".repeat(TEXT_LIMIT + 1),
            },
        })
        .unwrap();
        assert!(decode(&frame).is_err());
        assert!(decode(&vec![0; FRAME_LIMIT + 1]).is_err());
        assert_eq!(safe_text("a\x1b[2J\u{202e}b"), "a\\u{1b}[2J\\u{202e}b");
    }
    #[test]
    fn paths_accept_quotes_escapes_and_unclosed_quotes() {
        let parse = |value| path_argument(value, SEND_USAGE).unwrap();
        for value in [
            "holiday photos",
            "'holiday photos'",
            "\"holiday photos\"",
            "\"holiday photos",
            " 'holiday photos' ",
        ] {
            assert_eq!(parse(value), PathBuf::from("holiday photos"), "{value}");
        }
        // Single quotes keep backslashes, as a file manager's drop does.
        assert_eq!(parse("'a\\ b'"), PathBuf::from("a\\ b"));
        assert_eq!(parse("\"say \\\"hi\\\"\""), PathBuf::from("say \"hi\""));
        #[cfg(unix)]
        assert_eq!(parse("holiday\\ photos/"), PathBuf::from("holiday photos/"));
        for value in ["", "''", "\"\""] {
            assert!(path_argument(value, SEND_USAGE).is_err(), "{value}");
        }
    }

    #[test]
    fn mistyped_commands_suggest_the_closest_one() {
        assert_eq!(closest_command("/received"), Some("/receive"));
        assert_eq!(closest_command("/receive*"), Some("/receive"));
        assert_eq!(closest_command("/stat"), Some("/status"));
        assert_eq!(closest_command("/conf"), Some("/confirm"));
        assert_eq!(closest_command("/re"), None, "/receive or /reject");
        assert_eq!(closest_command("/xyzzy"), None);
    }

    #[test]
    fn a_missing_path_shows_the_command_usage() {
        let error = receive_arguments("").unwrap_err().to_string();
        assert!(error.starts_with("usage: /receive"), "{error}");
        let error = path_argument("  ", SEND_USAGE).unwrap_err().to_string();
        assert!(error.starts_with("usage: /send"), "{error}");
    }

    #[test]
    fn completion_offers_commands_and_receive_options() {
        let values = |line: &str| {
            complete(line, line.len())
                .into_iter()
                .map(|s| (s.span.start, s.value))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            values("/re"),
            [(0, "/receive".into()), (0, "/reject".into())]
        );
        assert_eq!(values("/receive --f"), [(9, "--force".into())]);
        assert_eq!(
            values("/receive --force --no"),
            [(17, "--no-replace".into())]
        );
        // Options never follow the destination; text and // are not commands.
        for line in ["/receive out --f", "hello /s", "//se", "/status x"] {
            assert!(values(line).is_empty(), "{line}");
        }
    }

    #[test]
    fn completed_paths_are_read_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        // Windows file names cannot contain a double quote.
        let quoted = cfg!(not(windows));
        for name in ["holiday photos", "inbox"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        if quoted {
            std::fs::create_dir(dir.path().join("quote\"d")).unwrap();
        }
        std::fs::write(dir.path().join("holiday notes"), b"").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"").unwrap();
        let root = format!("{}{}", dir.path().display(), std::path::MAIN_SEPARATOR);
        let mut cases = vec![
            ("/send", "holi", 2),
            ("/receive", "holi", 1),
            ("/receive --force", "in", 1),
            ("/send", "", if quoted { 4 } else { 3 }),
        ];
        if quoted {
            cases.push(("/send", "quo", 1));
        }
        for (command, typed, expected) in cases {
            let line = format!(
                "{command} {}",
                encode_path(&format!("{root}{typed}"), false)
            );
            let found = complete(&line, line.len());
            assert_eq!(found.len(), expected, "{line}: {found:?}");
            for suggestion in found {
                let completed = format!("{}{}", &line[..suggestion.span.start], suggestion.value);
                let argument = completed.strip_prefix(command).unwrap();
                let path = receive_arguments(argument).unwrap().0;
                assert!(path.exists(), "{completed} -> {path:?}");
            }
        }
    }

    #[test]
    fn timestamps_flag_overrides_the_environment() {
        let when = |flag, env: Option<&str>| timestamps_when(flag, env.map(str::to_owned));
        assert_eq!(when(None, None).unwrap(), Timestamps::Auto);
        assert_eq!(when(None, Some("")).unwrap(), Timestamps::Auto);
        assert_eq!(when(None, Some("Never")).unwrap(), Timestamps::Never);
        assert_eq!(
            when(Some(Timestamps::Always), Some("never")).unwrap(),
            Timestamps::Always
        );
        let error = when(None, Some("sometimes")).unwrap_err();
        assert!(error.to_string().contains("auto, always or never"));
    }

    #[test]
    fn receive_asks_by_default_and_accepts_one_replacement_flag() {
        let parse = receive_arguments;
        assert_eq!(parse("out").unwrap().1, files::Replacement::Ask);
        assert_eq!(parse("--force out").unwrap().1, files::Replacement::Force);
        assert_eq!(
            parse("--no-replace out").unwrap().1,
            files::Replacement::Refuse
        );
        assert_eq!(
            parse("-- --force").unwrap(),
            (PathBuf::from("--force"), files::Replacement::Ask, false)
        );
        assert!(!parse("out").unwrap().2);
        assert_eq!(
            parse("--hidden --force out").unwrap(),
            (PathBuf::from("out"), files::Replacement::Force, true)
        );
        for value in [
            "--confirm out",
            "--force --no-replace out",
            "--force --force out",
        ] {
            assert!(parse(value).is_err(), "{value}");
        }
    }
}
