use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use iroh::protocol::ProtocolHandler;
use iroh_blobs::{
    BlobFormat, BlobsProtocol, Hash, HashAndFormat,
    api::{
        Store, TempTag,
        blobs::{AddPathOptions, ImportMode},
        proto::AddProgressItem,
        remote::GetProgressItem,
    },
    format::collection::Collection,
    get::request::get_hash_seq_and_sizes,
    protocol::{ChunkRanges, GetRequest},
    provider::events::{EventMask, EventSender, ProviderMessage, ThrottleMode},
    store::fs::FsStore,
    ticket::BlobTicket,
};
use n0_future::StreamExt;

use crate::publication::{self, Publication};
pub use crate::publication::{ConfirmationChannel, Replacement};
use crate::{
    config::Settings,
    identity, network, pairing,
    partial::Partial,
    progress::{self, Progress},
};

const METADATA_LIMIT: u64 = 32 * 1024 * 1024;

fn validate_name(name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name.len() <= 4096
            && !name.contains('\\')
            && !name.contains(':')
            && !name.starts_with('/')
            && name.split('/').count() <= 64,
        "unsafe collection path"
    );
    for component in name.split('/') {
        ensure!(
            !matches!(component, "" | "." | "..")
                && !component.ends_with(['.', ' '])
                && !component
                    .chars()
                    .any(|c| c.is_control() || "<>\"|?*".contains(c)),
            "unsafe collection path"
        );
        let base = component.split('.').next().unwrap().to_ascii_uppercase();
        ensure!(
            !matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(base.len() == 4
                    && (base.starts_with("COM") || base.starts_with("LPT"))
                    && matches!(base.as_bytes()[3], b'1'..=b'9')),
            "unsafe collection path"
        );
    }
    Ok(name.split('/').collect())
}

fn validate_names<'a>(names: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        validate_name(name)?;
        ensure!(
            seen.insert(name.to_lowercase()),
            "duplicate collection path"
        );
    }
    for name in &seen {
        let mut prefix = String::new();
        for component in name.split('/').take(name.split('/').count() - 1) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            ensure!(
                !seen.contains(&prefix),
                "collection file/directory path conflict"
            );
        }
    }
    Ok(())
}

/// Refuses a top-level name starting with a dot unless `hidden` allows it: a
/// sender could otherwise drop `.bashrc` or `.ssh/` into a home directory.
fn check_hidden<'a>(names: impl Iterator<Item = &'a str>, hidden: bool) -> Result<()> {
    if hidden {
        return Ok(());
    }
    for name in names {
        let top = name.split('/').next().unwrap_or(name);
        ensure!(
            !top.starts_with('.'),
            "{top} starts with a dot; accept such names with --hidden"
        );
    }
    Ok(())
}

async fn import(
    source: &Path,
    store: &Store,
    progress: &mut Progress,
) -> Result<(TempTag, Vec<TempTag>, u64)> {
    let metadata = std::fs::symlink_metadata(source).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("{}: no such file or folder", source.display())
        } else {
            anyhow::Error::new(error).context(format!("cannot read {}", source.display()))
        }
    })?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "{}: source links are forbidden",
        source.display()
    );
    let source = source.canonicalize().context("source does not exist")?;
    ensure!(
        source.is_file() || source.is_dir(),
        "source must be a regular file or directory"
    );
    let parent = source
        .parent()
        .context("source must be below the filesystem root")?;
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(&source).follow_links(false) {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink()
                && (entry.file_type().is_file() || entry.file_type().is_dir()),
            "links and special files are forbidden"
        );
        if entry.file_type().is_file() {
            let name = entry
                .path()
                .strip_prefix(parent)?
                .components()
                .map(|p| p.as_os_str().to_str().context("file names must be UTF-8"))
                .collect::<Result<Vec<_>>>()?
                .join("/");
            files.push((name, entry.path().to_owned()));
        }
    }
    ensure!(
        !files.is_empty(),
        "directory contains no regular files; blob collections do not preserve empty directories"
    );
    files.sort_by(|a, b| a.0.cmp(&b.0));
    validate_names(files.iter().map(|(name, _)| name.as_str()))?;
    let mut tags = vec![];
    let mut total = 0u64;
    let mut collection = Collection::default();
    for (name, path) in files {
        let mut size = std::fs::metadata(&path)?.len();
        progress.phase("preparing: copying file to temporary store", Some(size));
        let mut stream = store
            .blobs()
            .add_path_with_opts(AddPathOptions {
                path,
                format: BlobFormat::Raw,
                mode: ImportMode::Copy,
            })
            .stream()
            .await;
        let mut tag = None;
        while let Some(item) = progress.wait(stream.next()).await {
            match item {
                AddProgressItem::CopyProgress(bytes) | AddProgressItem::OutboardProgress(bytes) => {
                    progress.update(bytes)
                }
                AddProgressItem::Size(bytes) => size = bytes,
                AddProgressItem::CopyDone => {
                    progress.report_bytes(size);
                    progress.phase("preparing: hashing file with BLAKE3", Some(size));
                }
                AddProgressItem::Done(value) => {
                    tag = Some(value);
                    break;
                }
                AddProgressItem::Error(error) => return Err(error.into()),
            }
        }
        let tag = tag.context("incomplete file preparation")?;
        progress.report_bytes(size);
        total = total.checked_add(size).context("source size overflow")?;
        collection.push(name, tag.hash());
        tags.push(tag);
    }
    let root = collection.store(store).await?;
    Ok((root, tags, total))
}

/// A copied, immutable offer kept alive until rejection or transfer completion.
pub struct PreparedSend {
    pub store: FsStore,
    pub name: String,
    pub bytes: u64,
    pub files: usize,
    root: TempTag,
    _tags: Vec<TempTag>,
    _directory: tempfile::TempDir,
}

impl PreparedSend {
    pub async fn new(path: &Path, no_progress: bool) -> Result<Self> {
        // Offers are prepared during a chat, whose input shares the terminal.
        Self::with_progress(path, &mut Progress::lines("send", !no_progress)).await
    }

    pub async fn with_progress(path: &Path, progress: &mut Progress) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let store = FsStore::load(directory.path().join("store")).await?;
        let (root, tags, bytes) = import(path, store.as_ref(), progress).await?;
        let name = path
            .canonicalize()?
            .file_name()
            .context("missing source name")?
            .to_str()
            .context("file names must be UTF-8")?
            .to_owned();
        Ok(Self {
            files: tags.len(),
            store,
            name,
            bytes,
            root,
            _tags: tags,
            _directory: directory,
        })
    }

    pub fn ticket(&self, ep: &iroh::Endpoint) -> BlobTicket {
        BlobTicket::new(ep.addr(), self.root.hash(), BlobFormat::HashSeq)
    }

    /// The collection's root hash: what a paired machine is offered.
    pub fn hash(&self) -> Hash {
        self.root.hash()
    }
}

/// Sends `path` to the one machine that uses the code opened here.
pub async fn send(
    settings: &Settings,
    path: &Path,
    transfer_timeout: f64,
    no_progress: bool,
    words: usize,
    options: &pairing::Options,
) -> Result<()> {
    options.validate(settings)?;
    let mut progress = Progress::new("send", !no_progress);
    progress.phase("preparing files (scanning)", None);
    let temp = tempfile::tempdir()?;
    let store = FsStore::load(temp.path().join("store")).await?;
    let (root, tags, total) = import(path, store.as_ref(), &mut progress).await?;
    let name = path.canonicalize()?.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let sending = match tags.len() {
        1 => format!("sending {name}"),
        files => format!("sending {name} ({files} files)"),
    };
    progress.phase("registering endpoint", None);
    let alpns = vec![iroh_blobs::ALPN.to_vec()];
    // Register the code's rendezvous endpoint at the same time.
    let (ep, rendezvous) = progress
        .wait(async {
            tokio::try_join!(
                network::bind(settings, alpns),
                pairing::open(settings, options, words)
            )
        })
        .await?;
    let result = async {
        let ticket = BlobTicket::new(ep.addr(), root.hash(), BlobFormat::HashSeq);
        let receiver =
            pairing::send_ticket(settings, options, rendezvous, &ticket, &mut progress).await?;
        progress.phase("paired; waiting for the receiver's iroh connection", None);
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(transfer_timeout);
        let conn = tokio::time::timeout_at(deadline, progress.wait(accept_receiver(&ep, receiver))).await
            .context("file transfer timed out waiting for paired receiver; retry with a new code")??;
        // Served bytes include metadata and repeated requests; the percentage is capped.
        progress.step(sending, Some(total));
        tokio::time::timeout_at(deadline, async {
            let mut served = 0;
            let mut conn = conn;
            loop {
                // A restarted receiver can return before this side notices that
                // its previous connection died: accept it meanwhile. Only the
                // paired identity is ever accepted.
                let event = tokio::select! {
                    result = serve_counting(&store, conn.clone(), &mut progress, &mut served) => Ok(result),
                    newer = accept_receiver(&ep, receiver) => Err(newer),
                };
                conn = match event {
                    Ok(result) => {
                        result?;
                        if finished(&conn, served, total) {
                            return Ok::<_, anyhow::Error>(());
                        }
                        note!("connection to the receiver lost; waiting for it to resume");
                        accept_receiver(&ep, receiver).await?
                    }
                    Err(newer) => {
                        let newer = newer?;
                        conn.close(0u32.into(), b"replaced by a newer connection");
                        newer
                    }
                };
                note!("receiver reconnected; resuming");
            }
        })
        .await
        .context("file transfer timed out; check the receiver and retry with a new code; increase --transfer-timeout for large transfers")??;
        progress.finish();
        note!("transfer finished; the receiver verifies integrity");
        Ok(())
    }
    .await;
    ep.close().await;
    store.shutdown().await?;
    result
}

/// Accepts connections until the expected identity connects; others are closed.
/// Handshakes run concurrently with a short budget each, so idle or foreign
/// connections cannot hold the door against the paired receiver. A closed
/// endpoint is an error.
pub(crate) async fn accept_receiver(
    ep: &iroh::Endpoint,
    expected: iroh::EndpointId,
) -> Result<iroh::endpoint::Connection> {
    network::accept_first(
        ep,
        network::HANDSHAKE_DEADLINE,
        "paired",
        move |conn| async move {
            if conn.remote_id() == expected {
                return Ok(conn);
            }
            conn.close(1u32.into(), b"not the paired receiver");
            anyhow::bail!("not the paired receiver")
        },
    )
    .await
}

/// Close of a reception that published its files.
pub(crate) const PUBLISHED: (u32, &[u8]) = (0, b"BLAKE3 verified and published");
/// Close of a reception that stopped for good (declined, refused, limits).
pub(crate) const ABORTED: (u32, &[u8]) = (2, b"reception aborted");

/// Whether the receiver ended the transfer. Any other end (network loss, an
/// interrupted receiver) waits for the same receiver to resume. A receiver
/// other than iroh-toolbox closes without a reason once it has everything.
fn finished(conn: &iroh::endpoint::Connection, served: u64, total: u64) -> bool {
    let Some(iroh::endpoint::ConnectionError::ApplicationClosed(close)) = conn.close_reason()
    else {
        return false;
    };
    let code = u64::from(close.error_code);
    let reason = &close.reason[..];
    (code, reason) == (PUBLISHED.0.into(), PUBLISHED.1)
        || code == u64::from(ABORTED.0)
        || (code == 0 && reason.is_empty() && served >= total)
}

pub(crate) async fn serve_blobs(
    store: &Store,
    conn: iroh::endpoint::Connection,
    progress: &mut Progress,
) -> Result<()> {
    serve_counting(store, conn, progress, &mut 0).await
}

/// Serves one connection; `served` accumulates across the connections of a transfer.
pub(crate) async fn serve_counting(
    store: &Store,
    conn: iroh::endpoint::Connection,
    progress: &mut Progress,
    served: &mut u64,
) -> Result<()> {
    // Count even without progress output: the count tells whether a receiver
    // that closes without a reason got everything.
    // The library's progress offsets are positions within blobs, not bytes sent:
    // metadata probes can request just the last chunk. Count actual write lengths
    // through its byte hook instead, acknowledging immediately without a rate limit.
    let (events, mut rx) = EventSender::channel(
        32,
        EventMask {
            throttle: ThrottleMode::Intercept,
            ..EventMask::DEFAULT
        },
    );
    let blobs = BlobsProtocol::new(store, Some(events));
    let transfer = blobs.accept(conn.clone());
    tokio::pin!(transfer);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    // Debug builds only: slows serving so tests can interrupt a transfer midway.
    #[cfg(debug_assertions)]
    let delay = std::env::var("IROH_TOOLBOX_TEST_SERVE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_millis);
    // Debug builds only: closes the first connection of the process once it
    // served this many bytes, as a sender whose connection broke midway. A byte
    // count, unlike a delay, always cuts inside the file contents, however
    // loaded the machine running the tests is.
    #[cfg(debug_assertions)]
    let cut: Option<u64> = {
        static CUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        std::env::var("IROH_TOOLBOX_TEST_SERVE_CUT_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|_| !CUT.swap(true, std::sync::atomic::Ordering::SeqCst))
    };
    #[cfg(not(debug_assertions))]
    let cut: Option<u64> = None;
    loop {
        tokio::select! {
            result = &mut transfer => { result?; break; }
            Some(ProviderMessage::Throttle(msg)) = rx.recv() => {
                *served = served.saturating_add(msg.inner.size);
                if cut.is_some_and(|after| *served >= after) {
                    conn.close(7u32.into(), b"test connection cut");
                    break;
                }
                #[cfg(debug_assertions)]
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                let _ = msg.tx.send(Ok(())).await;
            }
            _ = tick.tick() => progress.report_bytes(*served),
        }
    }
    // This describes locally served data, never remote publication.
    progress.report_bytes(*served);
    Ok(())
}

/// Where a reception gets the sender's blob ticket.
pub enum ReceiveTarget<'a> {
    /// A one-use code: the ticket arrives inside the authenticated pairing channel.
    Code {
        code: &'a str,
        pairing: &'a pairing::Options,
    },
    /// A ticket received inside an authenticated chat session.
    Session(&'a str),
}

pub struct ReceiveOptions {
    pub max_bytes: u64,
    pub transfer_timeout: f64,
    pub no_progress: bool,
    pub replacement: Replacement,
    pub confirmation: Option<ConfirmationChannel>,
    /// Accept top-level names starting with a dot.
    pub hidden: bool,
}

pub async fn receive(
    settings: &Settings,
    target: ReceiveTarget<'_>,
    output: &Path,
    options: ReceiveOptions,
) -> Result<()> {
    receive_using(settings, target, output, options, None).await
}

fn parse_ticket(value: &str, settings: &Settings) -> Result<BlobTicket> {
    let ticket: BlobTicket = value
        .trim()
        .parse()
        .context("expected a blob ticket from send")?;
    ensure!(
        ticket.format() == BlobFormat::HashSeq,
        "expected a collection blob ticket"
    );
    network::check_addr(ticket.addr(), settings)?;
    Ok(ticket)
}

/// Marks a lost connection: verified data stays in the store for another attempt.
#[derive(Debug)]
struct Lost;

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection to sender lost")
    }
}

fn lost(error: impl Into<anyhow::Error>) -> anyhow::Error {
    error.into().context(Lost)
}

/// Marks a sender never reached by this run: stop now, keeping earlier data.
#[derive(Debug)]
struct Unreachable;

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cannot reach the sender")
    }
}

/// Why a reception stopped before any byte was published, in terms a paired
/// sender is told. Attached as context to the local error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Larger than `--max-bytes`, or metadata over its limit.
    TooLarge,
    /// The destination already holds some of the names.
    Exists,
    /// The destination already holds exactly these files.
    Identical,
    /// A name is unsafe or collides with another.
    UnsafeNames,
    /// A top-level name starts with a dot and `--hidden` is not set.
    HiddenNames,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "larger than --max-bytes",
            Self::Exists => "the destination already exists",
            Self::Identical => "identical files are already at the destination",
            Self::UnsafeNames => "unsafe file names",
            Self::HiddenNames => "hidden names at the top level",
        })
    }
}

impl std::error::Error for Refusal {}

/// Where a reception gets its blobs.
enum Source<'a> {
    /// Dials the sender of a ticket, again after each lost connection.
    Ticket(&'a iroh::Endpoint, &'a BlobTicket),
    /// The authenticated connection of a paired machine, used once.
    Connection(&'a iroh::endpoint::Connection),
}

/// One reception, attempted again after each lost connection.
struct Reception<'a> {
    settings: &'a Settings,
    source: Source<'a>,
    store: &'a FsStore,
    hash: Hash,
    output: &'a Path,
    work: &'a Path,
    options: &'a ReceiveOptions,
    partial: Option<&'a Partial>,
    /// The existing items shown (and approved) before the first download.
    shown: Option<publication::Existing>,
    /// Whether this run reached the sender: only then is a failed connection retried.
    connected: bool,
    /// An existing destination holding exactly the offered files ends the
    /// reception as [`Refusal::Identical`] instead of a collision.
    identical_ends: bool,
    /// When the reception started, for its summary.
    started: std::time::Instant,
}

/// What a reception published, for its final line.
struct Received {
    /// Distinct top-level names, in collection order.
    tops: Vec<String>,
    files: usize,
    bytes: u64,
}

impl Received {
    /// `received photos → /dest/photos — 8.0 MiB, 12 files, 3.2 s, 2.5 MiB/s; BLAKE3 verified`
    fn summary(&self, output: &Path, elapsed: Duration) -> String {
        let (what, path) = match self.tops.as_slice() {
            [top] => (top.clone(), output.join(top)),
            tops => (format!("{} items", tops.len()), output.to_owned()),
        };
        let seconds = elapsed.as_secs_f64();
        let rate = if self.bytes >= 1024 * 1024 && seconds > 0.0 {
            format!(
                ", {}/s",
                progress::size((self.bytes as f64 / seconds) as u64)
            )
        } else {
            String::new()
        };
        format!(
            "received {what} → {} — {}, {}, {}{rate}; BLAKE3 verified",
            path.display(),
            progress::size(self.bytes),
            progress::file_count(self.files),
            progress::duration(elapsed),
        )
    }
}

impl Reception<'_> {
    /// Errors marked [`Lost`] may be retried; any other error ends the reception
    /// and tells the sender to stop.
    async fn attempt(&mut self, progress: &mut Progress) -> Result<()> {
        // After an interruption everything may already be here: no sender needed.
        let complete = self
            .store
            .remote()
            .local(HashAndFormat::hash_seq(self.hash))
            .await?
            .is_complete();
        let conn = if complete {
            None
        } else if let Source::Connection(conn) = self.source {
            self.connected = true;
            Some(conn.clone())
        } else if let Source::Ticket(ep, ticket) = self.source {
            progress.step("connecting to sender", None);
            let connected = self.connected;
            let failed = |error: anyhow::Error| {
                if connected {
                    lost(error)
                } else {
                    error.context(Unreachable)
                }
            };
            let conn = progress
                .wait(tokio::time::timeout(
                    self.settings.timeout,
                    ep.connect(ticket.addr().clone(), iroh_blobs::ALPN),
                ))
                .await
                .context("peer connection timed out; keep send running and check relay configuration on both machines")
                .map_err(failed)?
                .context("cannot connect to peer; keep send running")
                .map_err(failed)?;
            self.connected = true;
            Some(conn)
        } else {
            unreachable!("every source is handled")
        };
        let result = self.transfer(conn.as_ref(), progress).await;
        // A paired machine's connection carries the result in its own messages.
        if let (Some(conn), Source::Ticket(..)) = (&conn, &self.source) {
            match &result {
                Ok(_) => conn.close(PUBLISHED.0.into(), PUBLISHED.1),
                Err(error) if error.downcast_ref::<Lost>().is_none() => {
                    conn.close(ABORTED.0.into(), ABORTED.1)
                }
                Err(_) => {}
            }
        }
        let received = result?;
        done!("{}", received.summary(self.output, self.started.elapsed()));
        Ok(())
    }

    async fn transfer(
        &mut self,
        conn: Option<&iroh::endpoint::Connection>,
        progress: &mut Progress,
    ) -> Result<Received> {
        let hash = self.hash;
        let mut total = 0;
        // File contents only: what the offer announced, without metadata.
        let mut payload = 0;
        if let Some(conn) = conn {
            progress.phase("reading collection metadata", None);
            let (hash_seq, sizes) = progress
                .wait(get_hash_seq_and_sizes(conn, &hash, METADATA_LIMIT, None))
                .await
                .map_err(lost)?;
            let children_size = sizes
                .iter()
                .try_fold(0u64, |sum, size| sum.checked_add(*size))
                .context("collection size overflow")?;
            payload = sizes
                .iter()
                .skip(1)
                .try_fold(0u64, |sum, size| sum.checked_add(*size))
                .context("collection size overflow")?;
            let root_size = (hash_seq.len() as u64)
                .checked_mul(32)
                .context("collection size overflow")?;
            total = children_size
                .checked_add(root_size)
                .context("collection size overflow")?;
            ensure!(hash_seq.len() == sizes.len(), "incomplete collection sizes");
            if payload > self.options.max_bytes || total.saturating_sub(payload) > METADATA_LIMIT {
                return Err(anyhow::anyhow!(
                    "transfer exceeds --max-bytes or metadata limit; check the expected size, then deliberately raise --max-bytes (for example 10GiB); metadata is limited to 32MiB"
                )
                .context(Refusal::TooLarge));
            }
            // Names arrive before contents: refuse or ask about an existing
            // destination now instead of after the whole download.
            progress.phase("checking destination names", None);
            let names = GetRequest::builder()
                .root(ChunkRanges::all())
                .child(0, ChunkRanges::all())
                .build(hash);
            progress
                .wait(async { self.store.remote().execute_get(conn.clone(), names).await })
                .await
                .map_err(lost)?;
        }
        let collection = Collection::load(hash, self.store.as_ref()).await?;
        validate_names(collection.iter().map(|(name, _)| name.as_str()))
            .map_err(|error| error.context(Refusal::UnsafeNames))?;
        check_hidden(
            collection.iter().map(|(name, _)| name.as_str()),
            self.options.hidden,
        )
        .map_err(|error| error.context(Refusal::HiddenNames))?;
        let names = || collection.iter().map(|(name, _)| name.as_str());
        if self.shown.is_none() {
            let existing = publication::existing(self.output, names())?;
            if self.identical_ends
                && !existing.is_empty()
                && publication::identical(
                    self.output,
                    collection.iter().map(|(name, hash)| (name.as_str(), *hash)),
                )?
            {
                return Err(anyhow::Error::new(Refusal::Identical));
            }
            match self.options.replacement {
                Replacement::Refuse => existing
                    .ensure_empty("choose another --output, or omit --no-replace to be asked")
                    .map_err(|error| error.context(Refusal::Exists))?,
                Replacement::Ask => ensure!(
                    publication::confirm(existing.clone(), self.options.confirmation.as_ref())
                        .await?,
                    "file replacement declined; existing files kept"
                ),
                Replacement::Force => {}
            }
            self.shown = Some(existing);
        }
        if let Some(conn) = conn {
            let local = self
                .store
                .remote()
                .local(HashAndFormat::hash_seq(self.hash))
                .await?;
            let base = local.local_bytes();
            let mut stream = std::pin::pin!(
                self.store
                    .remote()
                    .execute_get(conn.clone(), local.missing())
                    .stream()
            );
            if let Some(partial) = self.partial {
                partial.downloading();
            }
            let mut complete = false;
            // Metadata arrives first: shown progress counts file contents only.
            let metadata = total.saturating_sub(payload);
            progress.step("receiving and verifying BLAKE3", Some(payload));
            while let Some(item) = progress.wait(stream.next()).await {
                match item {
                    GetProgressItem::Progress(bytes) => {
                        ensure!(bytes <= total, "transfer exceeds announced size");
                        let held = base.saturating_add(bytes).min(total);
                        if let Some(partial) = self.partial {
                            partial.received(held);
                        }
                        progress.update(held.saturating_sub(metadata));
                    }
                    GetProgressItem::Done(_) => {
                        complete = true;
                        break;
                    }
                    GetProgressItem::Error(error) => return Err(lost(error)),
                }
            }
            if !complete {
                return Err(lost(anyhow::anyhow!("incomplete blob transfer")));
            }
            progress.report_bytes(payload);
        }
        progress.phase("checking collection paths and staging verified files", None);
        let collection = Collection::load(hash, self.store.as_ref()).await?;
        ensure!(!collection.is_empty(), "empty collection");
        validate_names(collection.iter().map(|(name, _)| name.as_str()))?;
        check_hidden(
            collection.iter().map(|(name, _)| name.as_str()),
            self.options.hidden,
        )?;
        let stage = self.work.join("content");
        if stage.exists() {
            std::fs::remove_dir_all(&stage)?;
        }
        std::fs::create_dir(&stage)?;
        let mut received = Received {
            tops: Vec::new(),
            files: 0,
            bytes: 0,
        };
        for (name, hash) in collection.iter() {
            let path = stage.join(validate_name(name)?);
            std::fs::create_dir_all(path.parent().unwrap())?;
            progress
                .wait(self.store.blobs().export(*hash, &path))
                .await?;
            let top = name.split('/').next().unwrap_or(name);
            if !received.tops.iter().any(|seen| seen == top) {
                received.tops.push(top.to_owned());
            }
            received.files += 1;
            received.bytes = received
                .bytes
                .saturating_add(std::fs::metadata(&path)?.len());
        }
        progress.phase("publishing verified files to destination", None);
        let shown = self.shown.as_ref().context("missing destination check")?;
        ensure!(
            publication::existing(
                self.output,
                collection.iter().map(|(name, _)| name.as_str())
            )? == *shown,
            "destination changed while receiving; nothing was published"
        );
        Publication::prepare(&stage, self.output, self.options.replacement)?
            .confirmed(shown)?
            .apply()?;
        Ok(received)
    }
}

/// The absolute destination and the existing directory that holds its
/// working files (the destination itself, or its parent before it exists).
fn destination(output: &Path) -> Result<(PathBuf, PathBuf)> {
    let output = std::path::absolute(output)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&output) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "{}: output must be a directory, not a file or symlink",
            output.display()
        );
    }
    let parent = if output.is_dir() {
        output.clone()
    } else {
        output.parent().context("output needs a parent")?.to_owned()
    };
    ensure!(
        parent.is_dir(),
        "{}: no such folder; create it before using --output",
        parent.display()
    );
    Ok((output, parent))
}

/// Checks `--output` before a peer service starts, so a mistake fails at once.
pub(crate) fn check_destination(output: &Path) -> Result<()> {
    destination(output).map(drop)
}

/// How a reception from a paired machine is told apart from the others.
pub(crate) struct PeerReception<'a> {
    /// The authenticated connection that serves the offer.
    pub conn: &'a iroh::endpoint::Connection,
    /// The machine that proved its key: with the offer and the destination, it
    /// names the partial directory, so a resumed reception never mixes them.
    pub machine: &'a iroh::PublicKey,
    pub hash: Hash,
    /// How to resume an interrupted reception, said when its data is kept.
    pub resume: &'static str,
    /// End with [`Refusal::Identical`] when the destination already holds
    /// exactly these files (inboxes, which never replace anything).
    pub identical_ends: bool,
}

/// Receives the collection a paired machine offers on its connection: one
/// attempt; a lost connection keeps verified data for the same offer.
pub(crate) async fn receive_offer(
    settings: &Settings,
    peer: PeerReception<'_>,
    output: &Path,
    options: &ReceiveOptions,
    progress: &mut Progress,
) -> Result<()> {
    let (output, parent) = destination(output)?;
    let partial = Partial::open_peer(
        &parent,
        peer.machine,
        &peer.hash.to_hex(),
        &output,
        peer.resume,
    )?;
    let work = partial.path().to_owned();
    let store = FsStore::load(work.join("store")).await?;
    let mut interrupted = false;
    let result = async {
        let local = store
            .remote()
            .local(HashAndFormat::hash_seq(peer.hash))
            .await?;
        if local.local_bytes() > 0 {
            partial.received(local.local_bytes());
            note!(
                "resuming a partial transfer: {} already verified",
                progress::mib(local.local_bytes())
            );
        }
        let mut reception = Reception {
            settings,
            source: Source::Connection(peer.conn),
            store: &store,
            hash: peer.hash,
            output: &output,
            work: &work,
            options,
            partial: Some(&partial),
            shown: None,
            connected: false,
            identical_ends: peer.identical_ends,
            started: std::time::Instant::now(),
        };
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs_f64(options.transfer_timeout);
        match tokio::time::timeout_at(deadline, reception.attempt(progress)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                interrupted = error.downcast_ref::<Lost>().is_some();
                Err(error)
            }
            Err(_) => {
                interrupted = true;
                anyhow::bail!(
                    "file transfer timed out; increase --transfer-timeout for large transfers"
                )
            }
        }
    }
    .await;
    store.shutdown().await?;
    if !interrupted {
        partial.discard();
    }
    result
}

/// Whether a reception from a paired machine stopped on a lost connection.
pub(crate) fn is_lost(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Lost>().is_some()
}

pub(crate) async fn receive_using(
    settings: &Settings,
    target: ReceiveTarget<'_>,
    output: &Path,
    options: ReceiveOptions,
    endpoint: Option<&iroh::Endpoint>,
) -> Result<()> {
    // A borrowed endpoint means a chat reception, whose input shares the terminal.
    let mut progress = if endpoint.is_some() {
        Progress::lines("receive", !options.no_progress)
    } else {
        Progress::new("receive", !options.no_progress)
    };
    // A code is paired once per reception; a session ticket needs the chat's endpoint.
    let session_ticket = match &target {
        ReceiveTarget::Code { code, .. } => {
            ensure!(endpoint.is_none(), "a code reception owns its endpoint");
            pairing::check_code(code)?;
            None
        }
        ReceiveTarget::Session(value) => {
            ensure!(
                endpoint.is_some(),
                "a session ticket needs the chat endpoint"
            );
            Some(parse_ticket(value, settings)?)
        }
    };
    let (output, parent) = destination(output)?;
    let parent = parent.as_path();
    // A standalone reception keeps verified data to resume; a chat reception
    // ends with its chat.
    let (partial, _temp) = if let ReceiveTarget::Code { code, .. } = &target {
        let id = format!("code:{}", pairing::normalize_code(code)?);
        (Some(Partial::open(parent, &id)?), None)
    } else {
        let temp = tempfile::Builder::new()
            .prefix(".iroh-")
            .tempdir_in(parent)?;
        (None, Some(temp))
    };
    let work = partial.as_ref().map_or_else(
        || {
            _temp
                .as_ref()
                .expect("chat reception directory")
                .path()
                .to_owned()
        },
        |partial| partial.path().to_owned(),
    );
    let session = match &partial {
        Some(partial) => partial.session()?,
        None => None,
    };
    // Resuming does not pair: only a new pairing needs a terminal for the
    // confirmation number.
    if let ReceiveTarget::Code { pairing, .. } = &target {
        if session.is_some() {
            pairing.validate_resuming(settings)?;
        } else {
            pairing.validate(settings)?;
        }
    }
    let store = FsStore::load(work.join("store")).await?;
    progress.phase("registering endpoint", None);
    // The temporary relay-only pairing endpoint is bound alongside the transfer
    // endpoint, so the two registrations take one relay round trip together.
    let mut joiner = None;
    let owned_ep = if endpoint.is_none() {
        // The same key lets a later run resume as the identity the sender accepts;
        // a first run announces this key's public half inside the pairing.
        let key = match &session {
            Some(session) => session.key.clone(),
            None => iroh::SecretKey::from_bytes(&identity::random()?),
        };
        // The transfer endpoint needs no address lookup: the sender's ticket
        // carries its addresses. Only a new pairing needs the joiner endpoint,
        // which finds the code's rendezvous by its derived ID.
        let (ep, paired) = progress
            .wait(async {
                tokio::try_join!(network::bind_receiver(settings, key), async {
                    if session.is_none() {
                        pairing::prepare_join(settings).await.map(Some)
                    } else {
                        Ok(None)
                    }
                })
            })
            .await?;
        joiner = paired;
        Some(ep)
    } else {
        None
    };
    let ep = endpoint
        .or(owned_ep.as_ref())
        .context("missing receive endpoint")?;
    let mut interrupted = false;
    let result = async {
        let ticket = match (&session, &target) {
            (Some(session), _) => parse_ticket(&session.ticket, settings)?,
            (None, ReceiveTarget::Code { code, pairing }) => {
                let joiner = joiner.take().context("missing pairing endpoint")?;
                let ticket = pairing::receive_ticket(settings, pairing, code, joiner, ep, &mut progress).await?;
                progress.phase("paired; connecting to the sender over iroh", None);
                ticket
            }
            (None, ReceiveTarget::Session(_)) => {
                session_ticket.context("missing blob ticket")?
            }
        };
        if let Some(partial) = &partial
            && session.is_none()
        {
            partial.save(&ticket.to_string(), ep.secret_key())?;
        }
        let local = store.remote().local(ticket.hash_and_format()).await?;
        if session.is_some() {
            if let Some(partial) = &partial {
                partial.received(local.local_bytes());
            }
            note!(
                "resuming a partial transfer: {} already verified",
                progress::mib(local.local_bytes())
            );
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(options.transfer_timeout);
        let mut reception = Reception {
            settings,
            source: Source::Ticket(ep, &ticket),
            store: &store,
            hash: ticket.hash(),
            output: &output,
            work: &work,
            options: &options,
            partial: partial.as_ref(),
            shown: None,
            connected: false,
            identical_ends: false,
            started: std::time::Instant::now(),
        };
        loop {
            let error = match tokio::time::timeout_at(deadline, reception.attempt(&mut progress)).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => error,
                Err(_) => {
                    interrupted = true;
                    anyhow::bail!(
                        "file transfer timed out; check the sender and disk space, then retry; increase --transfer-timeout for large transfers"
                    )
                }
            };
            if error.downcast_ref::<Unreachable>().is_some() {
                interrupted = true;
                return Err(error);
            }
            // Without a partial directory (chat), or for a deliberate stop, end now.
            if reception.partial.is_none() || error.downcast_ref::<Lost>().is_none() {
                return Err(error);
            }
            note!("{error:#}; reconnecting to sender");
            tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_secs(1))).await;
        }
    }
    .await;
    if let Some(ep) = owned_ep {
        ep.close().await;
    }
    store.shutdown().await?;
    // Success and deliberate stops remove the partial directory; an
    // interruption keeps it (see `Partial`'s drop, also run on Ctrl-C).
    if let Some(partial) = partial
        && !interrupted
    {
        partial.discard();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reception_summary_names_the_published_path() {
        let one = Received {
            tops: vec!["abbr.sav".into()],
            files: 1,
            bytes: 336,
        };
        let path = Path::new("dest").join("abbr.sav");
        assert_eq!(
            one.summary(Path::new("dest"), Duration::from_millis(300)),
            format!(
                "received abbr.sav → {} — 336 B, 1 file, 0.3 s; BLAKE3 verified",
                path.display()
            )
        );
        let many = Received {
            tops: vec!["a".into(), "b".into()],
            files: 12,
            bytes: 8 * 1024 * 1024,
        };
        assert_eq!(
            many.summary(Path::new("dest"), Duration::from_secs(4)),
            "received 2 items → dest — 8.0 MiB, 12 files, 4.0 s, 2.0 MiB/s; BLAKE3 verified"
        );
    }

    #[test]
    fn rejects_path_traversal_and_portable_collisions() {
        for name in [
            "../escape",
            "/absolute",
            "a/../b",
            "a\\b",
            "C:secret",
            "a//b",
            "NUL",
            "a/CON.txt",
            "a/.",
            "a/trailing.",
        ] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        assert!(validate_names(["a", "a/b"].into_iter()).is_err());
        assert!(validate_names(["a", "A"].into_iter()).is_err());
        assert!(validate_names(["dir/file", "dir/other"].into_iter()).is_ok());
    }

    #[test]
    fn top_level_hidden_names_need_hidden() {
        for name in [
            ".bashrc",
            ".ssh/authorized_keys",
            ".config/autostart/x.desktop",
        ] {
            assert!(check_hidden([name].into_iter(), false).is_err(), "{name}");
            assert!(check_hidden([name].into_iter(), true).is_ok(), "{name}");
        }
        assert!(check_hidden(["project/.git/HEAD", "project/.env"].into_iter(), false).is_ok());
    }

    #[test]
    fn publication_never_overwrites_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join("stage");
        let output = temp.path().join("out");
        std::fs::create_dir(&stage).unwrap();
        std::fs::create_dir(&output).unwrap();
        std::fs::write(stage.join("file"), b"new").unwrap();
        std::fs::write(output.join("file"), b"existing").unwrap();
        assert!(Publication::prepare(&stage, &output, Replacement::Refuse).is_err());
        assert_eq!(std::fs::read(output.join("file")).unwrap(), b"existing");
    }

    async fn local_endpoint(alpns: Vec<Vec<u8>>) -> iroh::Endpoint {
        iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .clear_address_lookup()
            .relay_mode(iroh::RelayMode::Disabled)
            .alpns(alpns)
            .bind()
            .await
            .unwrap()
    }

    /// The gate of the sender and of both chat sides: strangers are closed with
    /// code 1 on any protocol, in numbers, before and after a connection drop,
    /// and never keep the paired machine out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_the_paired_identity_is_accepted_in_any_state() {
        use iroh::endpoint::ConnectionError;

        const CHAT: &[u8] = b"iroh-toolbox/chat/1";
        let server = local_endpoint(vec![CHAT.to_vec(), iroh_blobs::ALPN.to_vec()]).await;
        let paired = local_endpoint(vec![]).await;
        let (tx, mut accepted) = tokio::sync::mpsc::channel(8);
        let (gate, expected) = (server.clone(), paired.id());
        let gate = tokio::spawn(async move {
            // As the chat does: every admitted connection is handed over.
            loop {
                let conn = accept_receiver(&gate, expected).await?;
                tx.send(conn).await?;
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        });
        let addr = server.addr();
        let refused = |alpn: &'static [u8], count: usize| {
            let addr = addr.clone();
            async move {
                let mut strangers = tokio::task::JoinSet::new();
                for _ in 0..count {
                    let addr = addr.clone();
                    strangers.spawn(async move {
                        let ep = local_endpoint(vec![]).await;
                        let conn = ep.connect(addr, alpn).await.unwrap();
                        let closed = tokio::time::timeout(Duration::from_secs(10), conn.closed())
                            .await
                            .expect("stranger kept a connection");
                        ep.close().await;
                        closed
                    });
                }
                while let Some(closed) = strangers.join_next().await {
                    match closed.unwrap() {
                        ConnectionError::ApplicationClosed(close) => {
                            assert_eq!(u64::from(close.error_code), 1, "{close}")
                        }
                        other => panic!("stranger was not refused: {other}"),
                    }
                }
            }
        };
        // Many strangers on the chat protocol first: the paired machine still gets in.
        refused(CHAT, 16).await;
        let conn = paired.connect(addr.clone(), CHAT).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), accepted.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((first.remote_id(), first.alpn()), (paired.id(), CHAT));
        // During an accepted offer the file protocol is closed to strangers too.
        refused(iroh_blobs::ALPN, 4).await;
        // After a drop of the chat connection, strangers are refused again and
        // the paired machine reconnects.
        conn.close(3u32.into(), b"test drop");
        refused(CHAT, 4).await;
        refused(iroh_blobs::ALPN, 4).await;
        let again = paired.connect(addr, CHAT).await.unwrap();
        let second = tokio::time::timeout(Duration::from_secs(5), accepted.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.remote_id(), paired.id());
        again.close(0u32.into(), b"done");
        gate.abort();
        server.close().await;
    }

    /// The names a sender controls: whatever is accepted must be a relative
    /// path that stays inside the destination on every platform.
    fn assert_safe(name: &str, path: &Path) {
        assert!(!name.is_empty() && name.len() <= 4096 && path.is_relative());
        assert!(name.split('/').count() <= 64);
        assert_eq!(path, name.split('/').collect::<PathBuf>());
        for component in name.split('/') {
            assert!(!matches!(component, "" | "." | ".."), "{name:?}");
            assert!(!component.ends_with(['.', ' ']), "{name:?}");
            assert!(
                !component
                    .chars()
                    .any(|c| c.is_control() || "\\:<>\"|?*".contains(c)),
                "{name:?}"
            );
        }
        assert!(
            path.components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        );
    }

    #[test]
    fn validate_name_survives_random_and_mutated_names() {
        use crate::fuzz::Rng;
        const PIECES: &[&str] = &[
            "a",
            "b.txt",
            "..",
            ".",
            "",
            "CON",
            "com1",
            "Nul.txt",
            "lpt9",
            "COM0",
            "\u{e9}",
            "\u{202e}",
            "\u{1F600}",
            "x ",
            "y.",
            "a:b",
            "a\\b",
            "*",
            "q?",
            "t\tb",
            "\0",
            "dir",
            "Dir",
            "~",
            "-rf",
        ];
        let mut rng = Rng::new(0x2026_1110);
        let (mut accepted, mut refused) = (0, 0);
        for round in 0..6000 {
            let name = match round % 3 {
                0 => {
                    let parts: Vec<&str> =
                        (0..1 + rng.below(6)).map(|_| *rng.pick(PIECES)).collect();
                    parts.join("/")
                }
                1 => {
                    let length = rng.below(40);
                    (0..length)
                        .map(|_| char::from_u32(rng.below(0x3000) as u32).unwrap_or('?'))
                        .collect()
                }
                _ => {
                    let mut bytes = b"photos/2026/holiday.jpg".to_vec();
                    for _ in 0..1 + rng.below(3) {
                        let index = rng.below(bytes.len());
                        bytes[index] = *rng.pick(b"/.\\: \0*\xc3");
                    }
                    String::from_utf8_lossy(&bytes).into_owned()
                }
            };
            match validate_name(&name) {
                Ok(path) => {
                    accepted += 1;
                    assert_safe(&name, &path);
                }
                Err(_) => refused += 1,
            }
        }
        for name in ["a".repeat(4097), "a/".repeat(64) + "a", "/".into()] {
            assert!(validate_name(&name).is_err(), "{name:.20}");
        }
        assert!(validate_name(&format!("{}a", "a/".repeat(63))).is_ok());
        assert!(accepted > 200 && refused > 200, "{accepted} {refused}");
    }

    #[test]
    fn validate_names_survives_random_collections() {
        use crate::fuzz::Rng;
        const POOL: &[&str] = &[
            "a", "A", "a/b", "A/B", "a/b/c", "b", "a.txt", "a.TXT", "dir/a", "dir", "Dir/x",
            "x/..", "../x", "con", "e\u{301}", "\u{e9}", "",
        ];
        let mut rng = Rng::new(0x1110_2026);
        let (mut accepted, mut refused) = (0, 0);
        for _ in 0..4000 {
            let names: Vec<&str> = (0..rng.below(7)).map(|_| *rng.pick(POOL)).collect();
            if validate_names(names.iter().copied()).is_err() {
                refused += 1;
                continue;
            }
            accepted += 1;
            let lower: Vec<String> = names.iter().map(|name| name.to_lowercase()).collect();
            let unique: BTreeSet<&String> = lower.iter().collect();
            assert_eq!(unique.len(), lower.len(), "{names:?}");
            for name in &names {
                assert!(validate_name(name).is_ok(), "{name:?}");
            }
            for name in &lower {
                for (index, _) in name.match_indices('/') {
                    assert!(!unique.contains(&name[..index].to_owned()), "{names:?}");
                }
            }
        }
        assert!(validate_names(std::iter::empty()).is_ok());
        assert!(accepted > 200 && refused > 200, "{accepted} {refused}");
    }
}
