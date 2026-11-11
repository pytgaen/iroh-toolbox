//! Slots: named servers of a machine, reached with `peer:NAME/SLOT` (see docs/reference/slots.md).
//!
//! A slot endpoint key is computable by anyone who holds the address, so the
//! TLS identity of a slot proves nothing. On every connection the handshake
//! authenticates both machines with their machine keys, bound to this
//! connection (client endpoint ID and fresh nonces), before any application
//! byte flows:
//!
//! 1. `hello`: the client names the slot and sends a nonce;
//! 2. `proof`: the server signs the client's nonce and endpoint ID, and sends
//!    its own nonce; the client checks the key against its peer book;
//! 3. `client`: peer services, and ping or bench with `--peers-only`; the
//!    client signs the server's nonce and the server checks its signature, then
//!    the peer book and the server's own `--allow`;
//! 4. `ready`.
//!
//! The peer book says who is paired; the slot names a running service; only
//! `--allow` narrows who that server accepts. No per-slot grant exists.
use std::{
    future::Future,
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use iroh::{
    Endpoint, EndpointAddr, PublicKey, Signature,
    endpoint::{Connection, ConnectionError, RecvStream, SendStream},
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::Instrument;

use crate::{
    config::Settings,
    identity::{self, Address, Identity, SlotLock},
    network,
    peers::PeerBook,
    takeover,
};

const VERSION: u32 = 2;
const FRAME_LIMIT: usize = 1024;
const SERVER_DOMAIN: &[u8] = b"iroh-toolbox/slot-auth/v2";
const CLIENT_DOMAIN: &[u8] = b"iroh-toolbox/peer-auth/v1";
/// Close codes explaining a refused slot connection to the client.
///
/// One code for a wrong protocol and a wrong slot: a stranger who tries the
/// protocols or guesses slot names learns nothing from the answer.
const WRONG_SLOT: u32 = 21;
const NOT_PAIRED: u32 = 30;
// 31 refused a slot that was not granted; it is retired, never reused.
const NOT_ALLOWED: u32 = 32;
/// The client's key or signature does not verify, whether the key is paired or
/// not: the server decides it before reading its peer book.
const INVALID_PROOF: u32 = 33;
/// Any other refusal: a malformed message, or the server cannot decide.
const REFUSED: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Tunnel,
    /// `tunnel http serve`: a folder answered over HTTP.
    Http,
    Pipe,
    Ping,
    Bench,
    /// `receive --inbox`: paired machines push files to it.
    Inbox,
    /// `send PATH --peers-only`: paired machines fetch its files.
    Send,
    /// `chat serve`: one paired machine joins the conversation.
    Chat,
}

impl Protocol {
    const ALL: [Self; 8] = [
        Self::Tunnel,
        Self::Http,
        Self::Pipe,
        Self::Ping,
        Self::Bench,
        Self::Inbox,
        Self::Send,
        Self::Chat,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Tunnel => "tunnel-tcp",
            Self::Http => "tunnel-http",
            Self::Pipe => "pipe",
            Self::Ping => "ping",
            Self::Bench => "bench",
            Self::Inbox => "inbox",
            Self::Send => "send",
            Self::Chat => "chat",
        }
    }

    /// The serving command, for a client that reached the wrong kind of server.
    pub fn server_command(self) -> &'static str {
        match self {
            Self::Tunnel => "tunnel tcp serve",
            Self::Http => "tunnel http serve",
            Self::Pipe => "pipe serve",
            Self::Ping => "ping serve",
            Self::Bench => "bench serve",
            Self::Inbox => "receive --inbox",
            Self::Send => "send PATH --peers-only",
            Self::Chat => "chat serve",
        }
    }

    /// Whether the server exposes a local service and must name who it serves.
    fn requires_allow(self) -> bool {
        matches!(self, Self::Tunnel | Self::Http | Self::Pipe)
    }

    /// The slot used when none is named: the command's name.
    pub fn default_slot(self) -> &'static str {
        match self {
            Self::Tunnel => "tunnel",
            Self::Http => "http",
            Self::Pipe => "pipe",
            Self::Ping => "ping",
            Self::Bench => "bench",
            Self::Inbox => "inbox",
            Self::Send => "send",
            Self::Chat => "chat",
        }
    }

    /// Whether a server of this protocol always asks the client to prove its
    /// machine key. The server decides from its own protocol, never from the
    /// client's announcement: peer services give access to data, ping and bench
    /// only to measurements (they ask with `--peers-only`).
    pub fn requires_client(self) -> bool {
        matches!(
            self,
            Self::Tunnel | Self::Http | Self::Pipe | Self::Inbox | Self::Send | Self::Chat
        )
    }

    fn alpn(self) -> Vec<u8> {
        format!("iroh-toolbox/slot/{VERSION}/{}", self.name()).into_bytes()
    }

    fn from_alpn(alpn: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|protocol| protocol.alpn() == alpn)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Frame {
    Hello {
        version: u32,
        slot: String,
        nonce_c: String,
    },
    Proof {
        signature: String,
        nonce_s: String,
        /// Whether the server expects the `client` frame next. Only the server
        /// decides: tunnels and pipes always do, ping and bench with `--peers-only`.
        #[serde(default)]
        client: bool,
    },
    Client {
        machine: String,
        signature: String,
    },
    Ready {},
}

async fn write_frame(send: &mut SendStream, frame: &Frame) -> Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    ensure!(bytes.len() <= FRAME_LIMIT, "slot message exceeds limit");
    send.write_u16(bytes.len() as u16).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read_frame(recv: &mut RecvStream) -> Result<Frame> {
    let size = usize::from(recv.read_u16().await?);
    ensure!(size <= FRAME_LIMIT, "slot message exceeds limit");
    let mut bytes = vec![0; size];
    recv.read_exact(&mut bytes).await?;
    parse_frame(&bytes)
}

fn parse_frame(bytes: &[u8]) -> Result<Frame> {
    serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("invalid slot message"))
}

fn decode<const N: usize>(hex_text: &str) -> Result<[u8; N]> {
    hex::decode(hex_text)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .context("invalid slot message")
}

/// The signed statement: `protocol` serves `slot` at the slot key `endpoint`,
/// for the client endpoint `client` that took part in this exchange with
/// `nonce`. Each side signs under its own domain, so a signature made for one
/// cannot serve as the other. Variable-length fields carry their length.
fn statement(
    domain: &[u8],
    protocol: Protocol,
    slot: &str,
    endpoint: &PublicKey,
    client: &PublicKey,
    nonce: &[u8; 32],
) -> Vec<u8> {
    let mut message = domain.to_vec();
    for part in [protocol.name(), slot] {
        message.push(part.len() as u8);
        message.extend_from_slice(part.as_bytes());
    }
    message.extend_from_slice(endpoint.as_bytes());
    message.extend_from_slice(client.as_bytes());
    message.extend_from_slice(nonce);
    message
}

/// The handshake stream, kept open after `ready`: a pipe carries its data on
/// it, file services their control messages; the others open their own streams.
pub struct Handshake {
    pub send: SendStream,
    pub recv: RecvStream,
    /// On a server that asked for the client proof: the verified machine.
    pub peer: Option<Admitted>,
}

/// A client machine that proved its key and passed the peer book and `--allow`.
#[derive(Clone, Debug)]
pub struct Admitted {
    pub machine: PublicKey,
    /// Its local name in this machine's peer book when it was admitted.
    pub name: String,
}

impl Handshake {
    /// Ends the handshake for protocols that continue on other streams.
    pub fn finish(mut self) -> Result<()> {
        self.send.finish()?;
        Ok(())
    }
}

/// A refusal already shown to the operator: the machine is known, so its
/// refusal is worth a line. Refusals of strangers are only logged with -v.
#[derive(Debug)]
struct Reported(String);

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Reported {}

/// Closes a refused connection and returns the error to report locally.
fn refuse(conn: &Connection, code: u32, reason: &str, local: String) -> anyhow::Error {
    conn.close(code.into(), reason.as_bytes());
    anyhow::anyhow!(local)
}

/// The `--allow` keyword that serves every paired machine.
const ALLOW_ALL: &str = "all";

/// What the server asks of its options when it starts serving a slot.
#[derive(Default, Clone, Copy)]
pub struct Serve<'a> {
    /// The slot; `None` serves an ephemeral ticket (ping and bench only).
    pub slot: Option<&'a str>,
    /// Peer names that `--allow` narrows the server to, or `all`; empty serves every
    /// paired machine, except for tunnels and pipes, which refuse to start with it.
    pub allow: &'a [String],
    /// Fixed UDP port (`--port`).
    pub port: Option<u16>,
    /// Ping and bench: require the client proof and serve the peer book.
    pub peers_only: bool,
}

/// A slot served by this process, reserved until dropped.
pub struct Server {
    identity: Identity,
    slot: String,
    protocol: Protocol,
    /// The client proves its machine key and must be in the peer book.
    require_client: bool,
    /// Machine keys named by `--allow`, resolved once at start.
    allow: Vec<PublicKey>,
    /// Peers served at start, for the announcement.
    serving: Vec<String>,
    /// `--allow all`: every machine of the peer book, read at each connection.
    all: bool,
    /// Reports another holder of the address taking the slot; silent once dropped.
    takeover: OnceLock<Arc<takeover::Watch>>,
    _lock: SlotLock,
}

impl Server {
    /// Reserves `slot` (or the command's default) with the machine identity.
    ///
    /// Servers that ask for the client proof (peer services, ping and bench with
    /// `peers_only`) serve every machine of the peer book, narrowed by `allow`.
    /// They refuse to start without any paired machine or with an `allow` name
    /// that is not in the book. Open ping and bench servers take no `allow`.
    pub fn open(
        slot: &str,
        protocol: Protocol,
        allow: &[String],
        peers_only: bool,
    ) -> Result<Arc<Self>> {
        let slot = if slot.is_empty() {
            protocol.default_slot()
        } else {
            slot
        };
        identity::check_slot_name(slot)?;
        let all = allow.iter().any(|name| name == ALLOW_ALL);
        ensure!(
            !all || allow.len() == 1,
            "--allow all cannot be combined with machine names; use `--allow all` alone or list the names"
        );
        ensure!(
            !allow.is_empty() || !protocol.requires_allow(),
            "name the machines to serve: --allow NAME (repeatable), or --allow all"
        );
        let requested = allow;
        let allow = if all { &[][..] } else { allow };
        let (mut allowed, mut serving) = (Vec::new(), Vec::new());
        let require_client = protocol.requires_client() || peers_only;
        if require_client {
            let book = PeerBook::load()?;
            ensure!(
                !book.is_empty(),
                "no paired machine; pair one with `iroh-toolbox pair` first"
            );
            for name in allow {
                let peer = book.get(name).with_context(|| {
                    format!(
                        "--allow {name}: no paired machine named {name}; see `iroh-toolbox peers`"
                    )
                })?;
                if !allowed.contains(&peer.machine()) {
                    allowed.push(peer.machine());
                    serving.push(peer.name.clone());
                }
            }
            if all || allow.is_empty() {
                serving = book.iter().map(|peer| peer.name.clone()).collect();
            }
        } else {
            ensure!(
                requested.is_empty(),
                "--allow applies to peer services and --peers-only servers"
            );
        }
        let lock = identity::lock_slot(slot)?;
        let (identity, created) = Identity::load_or_create()?;
        if created {
            note!(
                "created this machine's identity in {}",
                identity::home()?.display()
            );
        }
        Ok(Arc::new(Self {
            identity,
            slot: slot.to_owned(),
            protocol,
            require_client,
            allow: allowed,
            serving,
            all,
            takeover: OnceLock::new(),
            _lock: lock,
        }))
    }

    fn endpoint_key(&self) -> PublicKey {
        self.identity.address().slot_key(&self.slot).public()
    }

    pub async fn bind(&self, settings: &Settings, port: Option<u16>) -> Result<Endpoint> {
        // Every slot ALPN is accepted so that a mismatch gets an explicit reason.
        let mut alpns: Vec<Vec<u8>> = Protocol::ALL.iter().map(|p| p.alpn()).collect();
        // A chat's file offers travel on their own connections, from its participant only.
        if self.protocol == Protocol::Chat {
            alpns.push(iroh_blobs::ALPN.to_vec());
        }
        let key = self.identity.address().slot_key(&self.slot);
        if settings.direct_only {
            // Neither registered at a relay nor published: nothing to take over.
            return network::bind_keyed_on(settings, key, alpns, port).await;
        }
        let watch = takeover::Watch::new(&self.slot);
        let ep = network::bind_keyed_on(settings, key, alpns, port)
            .instrument(watch.span())
            .await?;
        watch.monitor(&ep, network::public_relays(settings));
        let _ = self.takeover.set(watch);
        Ok(ep)
    }

    /// Says what is served and how the other machine reaches it. Never prints
    /// the machine address: only `pair` exchanges it.
    pub fn announce(&self, settings: &Settings, ep: &Endpoint, command: &str, args: &str) {
        // The command's own slot goes unnamed: `peer:NAME` reaches it.
        let default = self.slot == self.protocol.default_slot();
        let serving = if self.all {
            format!("all paired machines (now: {})", self.serving.join(", "))
        } else {
            self.serving.join(", ")
        };
        if self.require_client && default {
            note!("Serving to: {serving}");
        } else if self.require_client {
            note!("Serving slot {} to: {serving}", self.slot);
        } else {
            note!("Serving slot {}", self.slot);
        }
        let mut direct = String::new();
        if settings.direct_only {
            let mut addrs: Vec<SocketAddr> = ep.addr().ip_addrs().copied().collect();
            // A loopback address is of no use to another machine.
            addrs.sort_by_key(|addr| addr.ip().is_loopback());
            let list: Vec<String> = addrs.iter().map(ToString::to_string).collect();
            note!("Direct addresses: {}", list.join(", "));
            if let Some(first) = list.first() {
                direct = format!(" --addr {first}");
            }
        }
        let name = identity::machine_name().unwrap_or_else(|_| "machine".to_owned());
        let target = if default {
            format!("peer:{name}")
        } else {
            format!("peer:{name}/{}", self.slot)
        };
        network::peer_command(settings, command, &target, &format!("{args}{direct}"));
    }

    /// Authenticates a connected client before the protocol runs. Returns the
    /// handshake stream, which carries no application byte yet.
    pub async fn authenticate(&self, conn: &Connection, timeout: Duration) -> Result<Handshake> {
        tokio::time::timeout(timeout, async {
            // A silent connection holds a handshake slot: it gets less time
            // than the whole handshake, counted from the end of the transport
            // handshake and stretched on slow paths.
            let hello = tokio::time::timeout(network::hello_deadline(conn), async {
                let (send, mut recv) = conn.accept_bi().await?;
                Ok::<_, anyhow::Error>((send, read_frame(&mut recv).await?, recv))
            })
            .await
            .context("client sent no hello in time")??;
            let (mut send, hello, mut recv) = hello;
            let Frame::Hello {
                version,
                slot,
                nonce_c,
            } = hello
            else {
                bail!("invalid slot message");
            };
            // The protocol is judged here, with the slot: a wrong protocol and
            // a wrong slot get the same answer, which names neither.
            let protocol = Protocol::from_alpn(conn.alpn());
            if version != VERSION || slot != self.slot || protocol != Some(self.protocol) {
                return Err(refuse(
                    conn,
                    WRONG_SLOT,
                    "unknown slot or protocol",
                    format!(
                        "client asked for slot \"{slot}\" over {}; this server serves slot \"{}\" over {}",
                        protocol.map_or("an unknown protocol", Protocol::name),
                        self.slot,
                        self.protocol.name()
                    ),
                ));
            }
            // From here the handshake counts as past `hello`: capped, and evicted
            // at random (with a close the client retries after) beyond the caps.
            network::mark_hello();
            let nonce_c = decode::<32>(&nonce_c)?;
            let nonce_s = identity::random::<32>()?;
            let endpoint = self.endpoint_key();
            let signature = self.identity.sign(&statement(
                SERVER_DOMAIN,
                self.protocol,
                &self.slot,
                &endpoint,
                &conn.remote_id(),
                &nonce_c,
            ));
            write_frame(
                &mut send,
                &Frame::Proof {
                    signature: hex::encode(signature.to_bytes()),
                    nonce_s: hex::encode(nonce_s),
                    client: self.require_client,
                },
            )
            .await?;
            let peer = if self.require_client {
                // Stalling after a keyless `hello` must cost the attacker little:
                // the client frame follows the proof within a few round trips.
                Some(
                    tokio::time::timeout(
                        network::client_deadline(conn),
                        self.admit(conn, &mut recv, &endpoint, &nonce_s),
                    )
                    .await
                    .context("client sent no proof in time")??,
                )
            } else {
                None
            };
            write_frame(&mut send, &Frame::Ready {}).await?;
            Ok(Handshake { send, recv, peer })
        })
        .await
        .context("slot authentication timed out")?
    }

    /// Step 3: the client proves its machine key, then the peer book decides.
    /// The signature is checked first so that a client holding no key learns
    /// nothing about which keys are paired or whether this server would admit
    /// it. The book is read now, not at start: `peers rm` applies at once.
    async fn admit(
        &self,
        conn: &Connection,
        recv: &mut RecvStream,
        endpoint: &PublicKey,
        nonce_s: &[u8; 32],
    ) -> Result<Admitted> {
        let invalid = || {
            refuse(
                conn,
                INVALID_PROOF,
                "invalid client proof",
                "client could not prove its machine key".into(),
            )
        };
        let Frame::Client { machine, signature } = read_frame(recv).await? else {
            // A protocol error, not evidence about a signature.
            return Err(refuse(
                conn,
                REFUSED,
                "invalid slot message",
                "client sent no machine proof".into(),
            ));
        };
        let signature = decode::<64>(&signature).map_err(|_| invalid())?;
        let machine = PublicKey::from_z32(&machine).map_err(|_| invalid())?;
        let statement = statement(
            CLIENT_DOMAIN,
            self.protocol,
            &self.slot,
            endpoint,
            &conn.remote_id(),
            nonce_s,
        );
        machine
            .verify(&statement, &Signature::from_bytes(&signature))
            .map_err(|_| invalid())?;
        let book = PeerBook::load().map_err(|error| {
            refuse(
                conn,
                REFUSED,
                "the server cannot read its peer book",
                format!("{error:#}"),
            )
        })?;
        // Never log a key the client sent: only local names identify machines.
        let Some(peer) = book.by_machine(&machine) else {
            return Err(refuse(
                conn,
                NOT_PAIRED,
                "not paired",
                "refused a machine that is not paired".into(),
            ));
        };
        if !self.allow.is_empty() && !self.allow.contains(&machine) {
            let message = format!(
                "refused {} on slot {}: not listed by --allow",
                peer.name, self.slot
            );
            note!("{message}");
            conn.close(NOT_ALLOWED.into(), b"not allowed by --allow");
            return Err(Reported(message).into());
        }
        Ok(Admitted {
            machine,
            name: peer.name.clone(),
        })
    }
}

/// Binds the endpoint of a server: a slot of this machine, or for ping and
/// bench without `--slot` an ephemeral endpoint with a one-time ticket.
pub async fn bind_server(
    settings: &Settings,
    alpns: Vec<Vec<u8>>,
    protocol: Protocol,
    serve: Serve<'_>,
) -> Result<(Endpoint, Option<Arc<Server>>)> {
    match serve.slot {
        None => {
            ensure!(serve.allow.is_empty(), "--allow needs a slot");
            Ok((network::bind_on(settings, alpns, serve.port).await?, None))
        }
        Some(slot) => {
            let server = Server::open(slot, protocol, serve.allow, serve.peers_only)?;
            let ep = server.bind(settings, serve.port).await?;
            Ok((ep, Some(server)))
        }
    }
}

/// Tells how to reach a server: its slot's announcement, or an ephemeral
/// server's ticket on stdout.
pub fn announce(
    settings: &Settings,
    ep: &Endpoint,
    server: Option<&Server>,
    command: &str,
    args: &str,
) -> Result<()> {
    match server {
        Some(server) => server.announce(settings, ep, command, args),
        None => {
            let ticket = network::ticket(ep);
            network::share(settings, &ticket, false)?;
            network::remote_command(settings, command, &ticket, args);
        }
    }
    Ok(())
}

/// Runs the slot handshake on an accepted connection of a server that opens
/// its own streams afterwards; ephemeral servers skip it.
pub async fn authenticate(
    server: Option<&Server>,
    conn: &Connection,
    timeout: Duration,
) -> Result<()> {
    match server {
        Some(server) => server.authenticate(conn, timeout).await?.finish(),
        None => Ok(()),
    }
}

/// Completes the handshake of an incoming connection (transport, `hello` and
/// client proof) within one [`network::HANDSHAKE_DEADLINE`]; ephemeral servers
/// (no `server`) only wait for the connection. It gives up early when a newer
/// connection evicts `handshaking`. A failure is logged with -v and yields
/// `None`: it is usually a stranger, and strangers must not flood the output.
pub async fn accept(
    server: Option<&Server>,
    incoming: iroh::endpoint::Incoming,
    label: &str,
    handshaking: &mut network::Handshaking,
) -> Option<Connection> {
    accept_with(incoming, label, handshaking, |conn| async move {
        authenticate(server, &conn, network::HANDSHAKE_DEADLINE).await
    })
    .await
    .map(|(conn, ())| conn)
}

/// Like [`accept`], keeping the handshake stream and the admitted machine for
/// services that carry their control messages on it.
pub async fn accept_stream(
    server: &Server,
    incoming: iroh::endpoint::Incoming,
    label: &str,
    handshaking: &mut network::Handshaking,
) -> Option<(Connection, Handshake)> {
    accept_with(incoming, label, handshaking, |conn| async move {
        server
            .authenticate(&conn, network::HANDSHAKE_DEADLINE)
            .await
    })
    .await
}

/// Like [`accept`], with the caller's own authentication of the connection.
pub async fn accept_with<T, Fut>(
    incoming: iroh::endpoint::Incoming,
    label: &str,
    handshaking: &mut network::Handshaking,
    authenticate: impl FnOnce(Connection) -> Fut,
) -> Option<(Connection, T)>
where
    Fut: Future<Output = Result<T>>,
{
    let end = tokio::time::Instant::now() + network::HANDSHAKE_DEADLINE;
    let marker = handshaking.marker();
    let admitted = async {
        // Before the transport is up there is nothing to close: just stop.
        let conn = tokio::select! {
            conn = tokio::time::timeout_at(end, incoming) => {
                conn.context("peer handshake timed out")?.context("peer handshake failed")?
            }
            () = handshaking.evicted() => anyhow::bail!("dropped to make room for a newer connection"),
        };
        let handshake = network::with_hello(marker, async {
            tokio::time::timeout_at(end, authenticate(conn.clone()))
                .await
                .context("peer handshake timed out")?
        });
        tokio::select! {
            result = handshake => result.map(|value| (conn.clone(), value)),
            // The client may retry: it is not refused, only crowded out.
            () = handshaking.evicted() => {
                conn.close(network::CROWDED.into(), network::CROWDED_REASON);
                Err(anyhow::anyhow!("dropped to make room for a newer connection"))
            }
        }
    }
    .await;
    match admitted {
        Ok(admitted) => Some(admitted),
        Err(error) => {
            if !error.is::<Reported>() {
                detail!("{label}: refused a connection: {error:#}");
            }
            None
        }
    }
}

/// A slot of a paired machine, as a client reaches it.
pub struct Remote {
    /// The peer's local name, for messages.
    pub peer: String,
    /// Its machine key and salt, from the peer book.
    pub address: Address,
    pub slot: String,
    /// Direct addresses given with `--addr`.
    pub direct: Vec<SocketAddr>,
}

/// Connects to a slot and completes the handshake. The returned stream is
/// finished; use [`connect_stream`] when it carries data.
pub async fn connect(
    ep: &Endpoint,
    remote: &Remote,
    protocol: Protocol,
    settings: &Settings,
) -> Result<Connection> {
    let (conn, handshake) = connect_stream(ep, remote, protocol, settings).await?;
    handshake.finish()?;
    Ok(conn)
}

/// Connects to a slot and completes the handshake, keeping its stream.
pub async fn connect_stream(
    ep: &Endpoint,
    remote: &Remote,
    protocol: Protocol,
    settings: &Settings,
) -> Result<(Connection, Handshake)> {
    let mut addr = EndpointAddr::new(remote.address.slot_key(&remote.slot).public());
    if let Some(relay) = &settings.relay {
        addr = addr.with_relay_url(relay.clone());
    }
    for direct in &remote.direct {
        addr = addr.with_ip_addr(*direct);
    }
    // The server says whether it needs this machine's key; paired machines have one.
    let identity = Some(Identity::load_or_create()?.0);
    connect_addr(
        ep,
        addr,
        remote,
        protocol,
        settings.timeout,
        identity.as_ref(),
    )
    .await
}

/// Handshakes one client process runs at once toward the same slot endpoint.
const HANDSHAKES_PER_SLOT: usize = 4;

/// The gate shared by every client of `endpoint` in this process.
fn gate(endpoint: PublicKey) -> Arc<tokio::sync::Semaphore> {
    static GATES: std::sync::Mutex<Vec<(PublicKey, Arc<tokio::sync::Semaphore>)>> =
        std::sync::Mutex::new(Vec::new());
    let mut gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, gate)) = gates.iter().find(|(key, _)| *key == endpoint) {
        return gate.clone();
    }
    let gate = Arc::new(tokio::sync::Semaphore::new(HANDSHAKES_PER_SLOT));
    gates.push((endpoint, gate.clone()));
    gate
}

/// Attempts at a handshake that a busy or flooded server turned away.
const CONNECT_ATTEMPTS: u32 = 6;

/// Dials a slot at `addr` and runs the client side of the handshake. A server
/// that refused, dropped or never answered the connection is tried again a few
/// times with a short backoff: strangers can crowd out a handshake, and the
/// next try usually finds room. A verdict (a proof that fails, a refusal with a
/// reason) is final.
async fn connect_addr(
    ep: &Endpoint,
    addr: EndpointAddr,
    remote: &Remote,
    protocol: Protocol,
    timeout: Duration,
    identity: Option<&Identity>,
) -> Result<(Connection, Handshake)> {
    // One deadline for all attempts and the pauses between them.
    let deadline = tokio::time::Instant::now() + timeout;
    let gate = gate(addr.id);
    let mut delay = Duration::from_millis(250);
    let mut attempt = 1;
    loop {
        // A burst of local connections (a browser behind a tunnel) would fill the
        // server's budget of handshakes, all from one address: a few at a time.
        let (result, left) = {
            let _turn = tokio::time::timeout_at(deadline, gate.acquire())
                .await
                .context("slot authentication timed out")?
                .context("handshake gate closed")?;
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            (
                connect_once(ep, addr.clone(), remote, protocol, left, identity).await,
                left,
            )
        };
        match result {
            Err(failure) if failure.retry && attempt < CONNECT_ATTEMPTS && left > delay * 2 => {
                detail!(
                    "slot: handshake with {} did not complete ({:#}); retrying",
                    remote.peer,
                    failure.error
                );
                tokio::time::sleep(delay).await;
                delay *= 2;
                attempt += 1;
            }
            result => return result.map_err(|failure| failure.error),
        }
    }
}

struct Failure {
    error: anyhow::Error,
    /// Worth another attempt: the server did not decide anything about us.
    retry: bool,
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            error,
            retry: false,
        }
    }
}

/// Whether the server dropped the handshake to make room for others.
fn crowded(conn: &Connection) -> bool {
    matches!(
        conn.close_reason(),
        Some(ConnectionError::ApplicationClosed(close))
            if u64::from(close.error_code) == u64::from(network::CROWDED)
    )
}

/// Whether the server ended the connection without taking a decision: it
/// dropped the handshake (code 0), or the transport failed. Never a timeout.
fn undecided(conn: &Connection) -> bool {
    match conn.close_reason() {
        Some(ConnectionError::ApplicationClosed(close)) => u64::from(close.error_code) == 0,
        // An idle timeout is a full wait already: never repeat it.
        Some(ConnectionError::LocallyClosed | ConnectionError::TimedOut) | None => false,
        Some(_) => true,
    }
}

/// The slot could not be reached, or gave no usable proof: nothing is known
/// about the server's decision. Callers may name the command to run instead.
#[derive(Debug)]
pub struct Unavailable(String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

fn unavailable(remote: &Remote) -> anyhow::Error {
    let (peer, slot) = (&remote.peer, &remote.slot);
    Unavailable(format!(
        "cannot establish an authenticated connection to slot {slot} on {peer}: check that the server is running, the network and address (--addr), the slot name and compatible versions"
    ))
    .into()
}

/// The server's proof does not verify against the key stored when pairing.
/// Decided locally: nothing the endpoint says afterwards replaces it.
#[derive(Debug)]
struct BadProof;

impl std::fmt::Display for BadProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server proof does not verify")
    }
}

impl std::error::Error for BadProof {}

async fn connect_once(
    ep: &Endpoint,
    addr: EndpointAddr,
    remote: &Remote,
    protocol: Protocol,
    timeout: Duration,
    identity: Option<&Identity>,
) -> std::result::Result<(Connection, Handshake), Failure> {
    let endpoint = addr.id;
    let (peer, slot) = (remote.peer.as_str(), remote.slot.as_str());
    if endpoint != remote.address.slot_key(slot).public() {
        return Err(anyhow::anyhow!("slot address mismatch").into());
    }
    // Transport errors may name endpoints or addresses: they are not shown.
    let conn = match tokio::time::timeout(timeout, ep.connect(addr, &protocol.alpn())).await {
        // A full wait already: never repeated.
        Err(_) => return Err(unavailable(remote).into()),
        Ok(Err(_)) => {
            detail!("slot: cannot reach slot {slot} on {peer} yet");
            return Err(Failure {
                error: unavailable(remote),
                retry: true,
            });
        }
        Ok(Ok(conn)) => conn,
    };
    // Set once the server proved the machine key: only then do its refusals count.
    let proven = std::sync::atomic::AtomicBool::new(false);
    let verified = tokio::time::timeout(timeout, async {
        let (mut send, mut recv) = conn.open_bi().await?;
        let nonce_c = identity::random::<32>()?;
        write_frame(
            &mut send,
            &Frame::Hello {
                version: VERSION,
                slot: slot.into(),
                nonce_c: hex::encode(nonce_c),
            },
        )
        .await?;
        let Frame::Proof {
            signature,
            nonce_s,
            client,
        } = read_frame(&mut recv).await?
        else {
            bail!("invalid slot message");
        };
        let (signature, nonce_s) = (decode::<64>(&signature)?, decode::<32>(&nonce_s)?);
        // Only the key stored when pairing counts; the server does not announce one.
        // On failure nothing more is sent: no machine proof, no service data.
        let expected = statement(SERVER_DOMAIN, protocol, slot, &endpoint, &ep.id(), &nonce_c);
        remote
            .address
            .machine
            .verify(&expected, &Signature::from_bytes(&signature))
            .map_err(|_| BadProof)?;
        proven.store(true, std::sync::atomic::Ordering::SeqCst);
        if client {
            let identity = identity.context("this machine's identity is required")?;
            let signature = identity.sign(&statement(
                CLIENT_DOMAIN,
                protocol,
                slot,
                &endpoint,
                &ep.id(),
                &nonce_s,
            ));
            write_frame(
                &mut send,
                &Frame::Client {
                    machine: identity.address().machine.to_z32(),
                    signature: hex::encode(signature.to_bytes()),
                },
            )
            .await?;
        }
        let Frame::Ready {} = read_frame(&mut recv).await? else {
            bail!("invalid slot message");
        };
        Ok(Handshake {
            send,
            recv,
            peer: None,
        })
    })
    .await;
    let proven = proven.load(std::sync::atomic::Ordering::SeqCst);
    match verified {
        Ok(Ok(handshake)) => Ok((conn, handshake)),
        Ok(Err(error)) => {
            let retry =
                !error.is::<BadProof>() && (crowded(&conn) || (!proven && undecided(&conn)));
            let error = explain(&conn, remote, protocol, &error, proven);
            conn.close(1u32.into(), b"slot authentication failed");
            Err(Failure { error, retry })
        }
        Err(_) => {
            // A server that stays silent is not tried again: that would only
            // multiply the wait.
            conn.close(1u32.into(), b"slot authentication timed out");
            let error = if proven {
                anyhow::anyhow!(
                    "the authenticated handshake with {peer} (slot {slot}) did not complete in time; check the network and try again"
                )
            } else {
                unavailable(remote)
            };
            Err(error.into())
        }
    }
}

/// Says what a failed handshake proves, in locally written terms: the close
/// text chosen by the remote end is never shown, and before the server proved
/// its key, no claim about its peer book or filter is repeated.
fn explain(
    conn: &Connection,
    remote: &Remote,
    protocol: Protocol,
    error: &anyhow::Error,
    proven: bool,
) -> anyhow::Error {
    let (peer, slot) = (&remote.peer, &remote.slot);
    // A local verification failure is never replaced by what the endpoint says.
    if error.is::<BadProof>() {
        return anyhow::anyhow!(
            "cannot authenticate {peer}: its response does not match the expected identity or handshake context; check the target and whether {peer} rotated its identity"
        );
    }
    let close = match conn.close_reason() {
        Some(ConnectionError::ApplicationClosed(close)) => {
            Some((u64::from(close.error_code), close.reason.clone()))
        }
        _ => None,
    };
    let is = |code: u32| {
        close
            .as_ref()
            .is_some_and(|(got, _)| *got == u64::from(code))
    };
    if !proven {
        // Whoever holds the slot key can close with any code.
        return if is(NOT_PAIRED) || is(NOT_ALLOWED) || is(INVALID_PROOF) || is(network::BUSY) {
            anyhow::anyhow!(
                "cannot authenticate {peer}: the server did not provide a valid proof; its refusal reason is untrusted"
            )
        } else if is(WRONG_SLOT) {
            // Ping and bench serve a ticket unless given a slot.
            let option = if slot != protocol.default_slot() {
                format!(" --slot {slot}")
            } else if matches!(protocol, Protocol::Ping | Protocol::Bench) {
                " --slot".to_owned()
            } else {
                String::new()
            };
            anyhow::anyhow!(
                "{peer} refused the connection: unknown slot or protocol; check that {peer} serves slot {slot} with `iroh-toolbox {}{option}`",
                protocol.server_command()
            )
        } else if is(network::CROWDED) {
            anyhow::anyhow!("{peer} is handling too many connections at once; try again")
        } else {
            unavailable(remote)
        };
    }
    if is(NOT_PAIRED) {
        anyhow::anyhow!(
            "{peer} does not know this machine; run `iroh-toolbox pair` on both machines"
        )
    } else if is(NOT_ALLOWED) {
        anyhow::anyhow!("this {peer} server (slot {slot}) does not allow this machine (--allow)")
    } else if is(INVALID_PROOF) {
        anyhow::anyhow!(
            "{peer} rejected this machine's authentication proof (invalid client proof); check the local identity and protocol implementation"
        )
    } else if is(network::CROWDED) {
        anyhow::anyhow!("{peer} is handling too many connections at once; try again")
    } else if let Some((_, reason)) = close.as_ref().filter(|_| is(network::BUSY)) {
        anyhow::anyhow!("{peer} {}", network::busy_message(reason))
    } else {
        anyhow::anyhow!(
            "the authenticated handshake with {peer} (slot {slot}) did not complete; check the network and {peer}'s output, then try again"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{RelayMode, SecretKey, endpoint::presets};
    use std::future::Future;
    use tokio::sync::mpsc;

    async fn endpoint(key: Option<SecretKey>, alpns: Vec<Vec<u8>>) -> Endpoint {
        let mut builder = Endpoint::builder(presets::Minimal)
            .clear_address_lookup()
            .relay_mode(RelayMode::Disabled)
            .alpns(alpns);
        if let Some(key) = key {
            builder = builder.secret_key(key);
        }
        builder.bind().await.unwrap()
    }

    fn all_alpns() -> Vec<Vec<u8>> {
        Protocol::ALL.iter().map(|p| p.alpn()).collect()
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Runs `test` with an isolated identity directory (and so peer book).
    fn in_home<F: Future>(test: impl FnOnce() -> F) -> F::Output {
        crate::identity::tests::with_home(|_| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(test())
        })
    }

    /// The address of a machine whose secret the test does not hold.
    fn address_of(machine: PublicKey) -> Address {
        Address::from_z32(&machine.to_z32(), "yyyyyyyyyyyyyyyy").unwrap()
    }

    fn other_machine(seed: u8) -> Address {
        address_of(SecretKey::from_bytes(&[seed; 32]).public())
    }

    /// A slot server on loopback, with `laptop` (this test's client) and `phone`
    /// paired.
    struct Rig {
        server: Arc<Server>,
        remote: Remote,
        client: Identity,
        target: EndpointAddr,
        /// The server's verdict on each connection it handled.
        verdicts: mpsc::UnboundedReceiver<std::result::Result<(), String>>,
        _ep: Endpoint,
    }

    impl Rig {
        async fn new(protocol: Protocol, slot: &str, allow: &[&str]) -> Self {
            Self::with(protocol, slot, allow, false).await
        }

        async fn with(protocol: Protocol, slot: &str, allow: &[&str], peers_only: bool) -> Self {
            // The client is the identity created first; the server then takes a new one.
            let (client, _) = Identity::load_or_create().unwrap();
            PeerBook::update(|book| {
                book.add_pairing(client.address(), "laptop", None)?;
                book.add_pairing(other_machine(9), "phone", None)?;
                Ok(())
            })
            .unwrap();
            Identity::rotate(false).unwrap();
            let allow: Vec<String> = allow.iter().map(|name| (*name).to_owned()).collect();
            let server = Server::open(slot, protocol, &allow, peers_only).unwrap();
            let address = server.identity.address();
            let ep = endpoint(Some(address.slot_key(slot)), all_alpns()).await;
            let (verdicts_tx, verdicts) = mpsc::unbounded_channel();
            let (accepting, serving) = (ep.clone(), server.clone());
            tokio::spawn(async move {
                while let Some(incoming) = accepting.accept().await {
                    let (serving, verdicts) = (serving.clone(), verdicts_tx.clone());
                    tokio::spawn(async move {
                        let conn = incoming.await.unwrap();
                        let verdict = serving.authenticate(&conn, Duration::from_secs(2)).await;
                        let _ = verdicts.send(verdict.map(drop).map_err(|e| format!("{e:#}")));
                        // A real server keeps the connection for its protocol.
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    });
                }
            });
            Self {
                remote: Remote {
                    peer: "nas".into(),
                    address,
                    slot: slot.into(),
                    direct: vec![],
                },
                target: ep.addr(),
                server,
                client,
                verdicts,
                _ep: ep,
            }
        }

        /// What a client announcing `alpn_protocol` gets, as the client reports it.
        async fn connect(&self, from: &Endpoint, protocol: Protocol) -> Result<(), String> {
            connect_addr(
                from,
                self.target.clone(),
                &self.remote,
                protocol,
                TIMEOUT,
                Some(&self.client),
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
        }

        async fn verdict(&mut self) -> std::result::Result<(), String> {
            tokio::time::timeout(TIMEOUT, self.verdicts.recv())
                .await
                .unwrap()
                .unwrap()
        }

        /// The client side of steps 1 and 2 by hand, for a client that misbehaves.
        async fn hello(&self, from: &Endpoint) -> Raw {
            let conn = from
                .connect(self.target.clone(), &self.server.protocol.alpn())
                .await
                .unwrap();
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            let hello = Frame::Hello {
                version: VERSION,
                slot: self.remote.slot.clone(),
                nonce_c: hex::encode([7u8; 32]),
            };
            write_frame(&mut send, &hello).await.unwrap();
            let Frame::Proof { nonce_s, .. } = read_frame(&mut recv).await.unwrap() else {
                panic!("expected a proof");
            };
            Raw {
                conn,
                send,
                recv,
                nonce_s: decode(&nonce_s).unwrap(),
            }
        }

        /// A valid client frame for `endpoint` and `nonce`, signed by the paired client.
        fn client_frame(&self, endpoint: &PublicKey, nonce: &[u8; 32]) -> Frame {
            let message = statement(
                CLIENT_DOMAIN,
                self.server.protocol,
                &self.remote.slot,
                &self.server.endpoint_key(),
                endpoint,
                nonce,
            );
            Frame::Client {
                machine: self.client.address().machine.to_z32(),
                signature: hex::encode(self.client.sign(&message).to_bytes()),
            }
        }
    }

    struct Raw {
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
        nonce_s: [u8; 32],
    }

    #[test]
    fn tunnel_slots_admit_paired_machines_and_explain_refusals() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let client = endpoint(None, vec![]).await;
            // Paired is enough: no separate grant for this slot.
            rig.connect(&client, Protocol::Tunnel).await.unwrap();
            rig.verdict().await.unwrap();
            // A client announcing another protocol is refused in general terms.
            let error = rig.connect(&client, Protocol::Pipe).await.unwrap_err();
            assert!(error.contains("unknown slot or protocol"), "{error}");
            assert!(
                error.contains("`iroh-toolbox pipe serve --slot ssh`"),
                "{error}"
            );
            let error = rig.connect(&client, Protocol::Ping).await.unwrap_err();
            assert!(error.contains("unknown slot or protocol"), "{error}");
            // The book is read on every connection: no restart.
            PeerBook::update(|book| book.remove("laptop")).unwrap();
            let error = rig.connect(&client, Protocol::Tunnel).await.unwrap_err();
            assert_eq!(
                error,
                "nas does not know this machine; run `iroh-toolbox pair` on both machines"
            );
            // A new pairing applies at once too.
            PeerBook::update(|book| {
                book.add_pairing(rig.client.address(), "laptop", None)
                    .map(drop)
            })
            .unwrap();
            rig.connect(&client, Protocol::Tunnel).await.unwrap();
            // The operator's log names the cause without a machine key.
            let mut verdicts = Vec::new();
            while let Ok(Some(verdict)) =
                tokio::time::timeout(Duration::from_millis(200), rig.verdicts.recv()).await
            {
                verdicts.push(verdict);
            }
            let log = format!("{verdicts:?}");
            let key = identity::short_key(&rig.client.address().machine);
            assert!(log.contains("not paired") && !log.contains(&key), "{log}");
            client.close().await;
        });
    }

    #[test]
    fn allow_narrows_a_slot_to_the_listed_peers() {
        in_home(|| async {
            let rig = Rig::new(Protocol::Pipe, "backup", &["phone"]).await;
            let client = endpoint(None, vec![]).await;
            let error = rig.connect(&client, Protocol::Pipe).await.unwrap_err();
            assert_eq!(
                error,
                "this nas server (slot backup) does not allow this machine (--allow)"
            );
            assert_eq!(rig.server.serving, ["phone"]);
            client.close().await;
        });
    }

    /// Distinct slots of one machine keep their own filters.
    #[test]
    fn each_server_applies_its_own_allow_list() {
        in_home(|| async {
            let mut open = Rig::new(Protocol::Tunnel, "web", &["all"]).await;
            let narrowed = Server::open("ssh", Protocol::Tunnel, &["phone".into()], false).unwrap();
            assert_eq!(open.server.serving, ["laptop", "phone"]);
            assert_eq!(narrowed.serving, ["phone"]);
            assert_eq!(narrowed.allow, [other_machine(9).machine]);
            let client = endpoint(None, vec![]).await;
            open.connect(&client, Protocol::Tunnel).await.unwrap();
            open.verdict().await.unwrap();
            client.close().await;
        });
    }

    #[test]
    fn servers_refuse_to_start_without_peers_or_with_unknown_allow_names() {
        in_home(|| async {
            let start = |slot: &str, protocol, allow: &[&str], peers_only| {
                let allow: Vec<String> = allow.iter().map(|name| (*name).to_owned()).collect();
                Server::open(slot, protocol, &allow, peers_only)
                    .err()
                    .map(|e| format!("{e:#}"))
            };
            for protocol in [Protocol::Tunnel, Protocol::Http, Protocol::Pipe] {
                assert_eq!(
                    start("ssh", protocol, &["all"], false).unwrap(),
                    "no paired machine; pair one with `iroh-toolbox pair` first"
                );
            }
            assert!(start("diag", Protocol::Ping, &[], true).is_some());
            PeerBook::update(|book| {
                book.add_pairing(other_machine(1), "laptop", None)?;
                book.add_pairing(other_machine(2), "phone", None)?;
                Ok(())
            })
            .unwrap();
            assert!(start("ssh", Protocol::Pipe, &["all"], false).is_none());
            let error = start("web", Protocol::Tunnel, &["ghost"], false).unwrap();
            assert!(error.contains("no paired machine named ghost"), "{error}");
            assert!(!error.contains("grant"), "{error}");
            assert!(start("web", Protocol::Tunnel, &["laptop", "laptop"], false).is_none());
            // Ping and bench serve measurements: no peer is needed, and --allow is refused.
            assert!(start("diag", Protocol::Ping, &[], false).is_none());
            assert!(start("diag2", Protocol::Bench, &["laptop"], false).is_some());
        });
    }

    /// Servers that expose a local service name who they serve, before anything else.
    #[test]
    fn tunnels_and_pipes_refuse_to_start_without_allow() {
        in_home(|| async {
            let start = |protocol, allow: &[&str]| {
                let allow: Vec<String> = allow.iter().map(|name| (*name).to_owned()).collect();
                Server::open("svc", protocol, &allow, false)
                    .err()
                    .map(|e| format!("{e:#}"))
            };
            for protocol in [Protocol::Tunnel, Protocol::Http, Protocol::Pipe] {
                // Refused even with no peer book: the option is checked first.
                assert_eq!(
                    start(protocol, &[]).unwrap(),
                    "name the machines to serve: --allow NAME (repeatable), or --allow all"
                );
            }
            let error = start(Protocol::Tunnel, &["all", "laptop"]).unwrap();
            assert!(error.contains("--allow all cannot be combined"), "{error}");
            let error = start(Protocol::Ping, &["laptop", "all"]).unwrap();
            assert!(error.contains("--allow all cannot be combined"), "{error}");
        });
    }

    /// `--allow all` reads the peer book at each connection and announces the current list.
    #[test]
    fn allow_all_admits_machines_paired_after_the_start() {
        in_home(|| async {
            let rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            assert!(rig.server.all);
            assert!(rig.server.allow.is_empty());
            // The client's pairing is made after the server started.
            PeerBook::update(|book| book.remove("laptop")).unwrap();
            let client = endpoint(None, vec![]).await;
            assert!(rig.connect(&client, Protocol::Tunnel).await.is_err());
            PeerBook::update(|book| {
                book.add_pairing(rig.client.address(), "tablet", None)
                    .map(drop)
            })
            .unwrap();
            rig.connect(&client, Protocol::Tunnel).await.unwrap();
            client.close().await;
        });
    }

    #[test]
    fn a_client_that_skips_or_forges_its_proof_gets_nothing() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let client = endpoint(None, vec![]).await;

            // Skipping step 3: the server never says ready, and a missing proof
            // is not reported as an invalid one.
            let mut skip = rig.hello(&client).await;
            skip.send.finish().unwrap();
            assert!(read_frame(&mut skip.recv).await.is_err());
            assert!(rig.verdict().await.is_err());
            assert_ne!(close_code(&skip.conn).await, Some(u64::from(INVALID_PROOF)));

            // Another frame in place of the proof is a protocol error.
            let mut wrong = rig.hello(&client).await;
            write_frame(&mut wrong.send, &Frame::Ready {})
                .await
                .unwrap();
            assert!(read_frame(&mut wrong.recv).await.is_err());
            assert_eq!(close_code(&wrong.conn).await, Some(u64::from(REFUSED)));
            assert!(
                rig.verdict()
                    .await
                    .unwrap_err()
                    .contains("no machine proof")
            );

            // A signature by another key than the one announced.
            let mut forged = rig.hello(&client).await;
            let Frame::Client { machine, .. } = rig.client_frame(&client.id(), &forged.nonce_s)
            else {
                unreachable!()
            };
            let signature = SecretKey::generate().sign(b"anything").to_bytes();
            let frame = Frame::Client {
                machine,
                signature: hex::encode(signature),
            };
            write_frame(&mut forged.send, &frame).await.unwrap();
            assert!(read_frame(&mut forged.recv).await.is_err());
            let error = rig.verdict().await.unwrap_err();
            assert!(error.contains("could not prove its machine key"), "{error}");

            // A client frame replayed on another connection: other nonce, other endpoint.
            let mut first = rig.hello(&client).await;
            let valid = rig.client_frame(&client.id(), &first.nonce_s);
            let Frame::Client { machine, signature } = &valid else {
                unreachable!()
            };
            let replay = Frame::Client {
                machine: machine.clone(),
                signature: signature.clone(),
            };
            write_frame(&mut first.send, &valid).await.unwrap();
            assert!(matches!(
                read_frame(&mut first.recv).await.unwrap(),
                Frame::Ready {}
            ));
            rig.verdict().await.unwrap();
            let elsewhere = endpoint(None, vec![]).await;
            let mut second = rig.hello(&elsewhere).await;
            write_frame(&mut second.send, &replay).await.unwrap();
            assert!(read_frame(&mut second.recv).await.is_err());
            assert!(rig.verdict().await.is_err());

            // A proof made for another endpoint and relayed from this one, with the right nonce.
            let mut relayed = rig.hello(&elsewhere).await;
            let frame = rig.client_frame(&client.id(), &relayed.nonce_s);
            write_frame(&mut relayed.send, &frame).await.unwrap();
            assert!(read_frame(&mut relayed.recv).await.is_err());
            assert!(rig.verdict().await.is_err());

            // The same exchange from the right endpoint is admitted: the checks above were not vacuous.
            let mut honest = rig.hello(&elsewhere).await;
            let frame = rig.client_frame(&elsewhere.id(), &honest.nonce_s);
            write_frame(&mut honest.send, &frame).await.unwrap();
            assert!(matches!(
                read_frame(&mut honest.recv).await.unwrap(),
                Frame::Ready {}
            ));
            rig.verdict().await.unwrap();
            drop((skip.conn, wrong.conn, forged.conn, first.conn, honest.conn));
            client.close().await;
            elsewhere.close().await;
        });
    }

    /// The close code of a connection the server ended, once it ended.
    async fn close_code(conn: &Connection) -> Option<u64> {
        match tokio::time::timeout(TIMEOUT, conn.closed()).await.ok()? {
            ConnectionError::ApplicationClosed(close) => Some(u64::from(close.error_code)),
            _ => None,
        }
    }

    #[test]
    fn the_signature_is_checked_before_the_peer_book() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let client = endpoint(None, vec![]).await;
            // An unreadable book proves it is never read for an invalid proof.
            let book = identity::home().unwrap().join("peers.toml");
            let saved = std::fs::read_to_string(&book).unwrap();
            std::fs::write(&book, "not toml [").unwrap();
            // Paired key, invalid signature, and an unpaired key with an invalid one
            // get the same refusal: no oracle on the book for a client without the key.
            for machine in [rig.client.address().machine, SecretKey::generate().public()] {
                for signature in [hex::encode([1u8; 64]), "zz".into()] {
                    let mut raw = rig.hello(&client).await;
                    let frame = Frame::Client {
                        machine: machine.to_z32(),
                        signature,
                    };
                    write_frame(&mut raw.send, &frame).await.unwrap();
                    assert!(read_frame(&mut raw.recv).await.is_err(), "no ready");
                    assert_eq!(close_code(&raw.conn).await, Some(u64::from(INVALID_PROOF)));
                    let error = rig.verdict().await.unwrap_err();
                    assert!(error.contains("could not prove"), "{error}");
                }
            }
            // A valid proof reaches the book, whose failure is not an invalid proof.
            let mut raw = rig.hello(&client).await;
            let frame = rig.client_frame(&client.id(), &raw.nonce_s);
            write_frame(&mut raw.send, &frame).await.unwrap();
            assert!(read_frame(&mut raw.recv).await.is_err());
            assert_eq!(close_code(&raw.conn).await, Some(u64::from(REFUSED)));
            assert!(rig.verdict().await.unwrap_err().contains("peers.toml"));
            std::fs::write(&book, saved).unwrap();
            client.close().await;
        });
    }

    /// A new client identity signs validly: the server answers "not paired" (30),
    /// never "invalid proof" (33).
    #[test]
    fn a_rotated_client_identity_is_not_paired_rather_than_invalid() {
        in_home(|| async {
            let rig = Rig::new(Protocol::Pipe, "backup", &["all"]).await;
            let (_, rotated) = Identity::rotate(false).unwrap();
            assert_ne!(rotated.address().machine, rig.client.address().machine);
            let client = endpoint(None, vec![]).await;
            let error = connect_addr(
                &client,
                rig.target.clone(),
                &rig.remote,
                Protocol::Pipe,
                TIMEOUT,
                Some(&rotated),
            )
            .await
            .err()
            .unwrap()
            .to_string();
            assert!(error.contains("does not know this machine"), "{error}");
            client.close().await;
        });
    }

    #[test]
    fn ping_slots_do_not_ask_for_a_client_proof() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Ping, "web", &[]).await;
            // Not paired as far as the server is concerned, and no identity held.
            PeerBook::update(|book| book.remove("laptop")).unwrap();
            let client = endpoint(None, vec![]).await;
            let result = connect_addr(
                &client,
                rig.target.clone(),
                &rig.remote,
                Protocol::Ping,
                TIMEOUT,
                None,
            )
            .await;
            assert!(result.is_ok());
            rig.verdict().await.unwrap();
            client.close().await;
        });
    }

    /// A refusal reason chosen by the remote end never reaches the terminal raw,
    /// and before the server proof it is not repeated at all.
    #[test]
    fn remote_refusal_reasons_are_generic_or_escaped() {
        in_home(|| async {
            let rig = Rig::new(Protocol::Ping, "web", &[]).await;
            for (code, expected) in [
                (WRONG_SLOT, None),
                (network::BUSY, Some("its refusal reason is untrusted")),
            ] {
                let hostile = endpoint(Some(rig.remote.address.slot_key("web")), all_alpns()).await;
                let target = hostile.addr();
                let refusing = tokio::spawn(async move {
                    let conn = hostile.accept().await.unwrap().await.unwrap();
                    conn.close(code.into(), b"refused\x1b[2J\x07 slot \"web\" serves ssh");
                    conn.closed().await;
                    hostile
                });
                let client = endpoint(None, vec![]).await;
                let error =
                    connect_addr(&client, target, &rig.remote, Protocol::Ping, TIMEOUT, None)
                        .await
                        .err()
                        .unwrap()
                        .to_string();
                match expected {
                    Some(expected) => assert!(error.contains(expected), "{error}"),
                    None => assert!(error.contains("unknown slot or protocol"), "{error}"),
                }
                assert!(!error.contains("serves ssh") && !error.contains('\x1b'));
                refusing.await.unwrap().close().await;
                client.close().await;
            }
        });
    }

    /// Replaces the rig's server loop by one like the tunnel's, with a small
    /// handshake budget and the screening of unvalidated sources.
    /// The admissions [`screened`] counted: the server counts one just after
    /// sending `ready`, so this waits up to 2 s for the first.
    async fn admissions(admitted: &std::sync::atomic::AtomicUsize) -> usize {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while admitted.load(std::sync::atomic::Ordering::SeqCst) == 0
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        admitted.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn screened(rig: &mut Rig, budget: usize) -> Arc<std::sync::atomic::AtomicUsize> {
        let serving = rig.server.clone();
        let pending = network::Pending::new(budget);
        let admitted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = admitted.clone();
        // The rig's own accept loop would compete for connections.
        rig._ep.close().await;
        let ep = endpoint(
            Some(rig.remote.address.slot_key(&rig.remote.slot)),
            all_alpns(),
        )
        .await;
        rig.target = ep.addr();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let Some((incoming, mut handshaking)) = pending.screen(incoming) else {
                    continue;
                };
                let (server, count) = (serving.clone(), count.clone());
                tokio::spawn(async move {
                    if let Some(conn) =
                        accept(Some(&server), incoming, "test", &mut handshaking).await
                    {
                        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        drop(handshaking);
                        conn.closed().await;
                    }
                });
            }
        });
        admitted
    }

    /// Mute connections fill the handshake budget; the oldest are evicted for
    /// newcomers, so a paired client that arrives later still gets through.
    #[test]
    fn a_flood_of_mute_connections_does_not_keep_a_paired_client_out() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let admitted = screened(&mut rig, 4).await;
            // Far more silent connections than the budget, kept open.
            let mute = endpoint(None, vec![]).await;
            let mut held = Vec::new();
            for _ in 0..24 {
                if let Ok(conn) = mute
                    .connect(rig.target.clone(), &Protocol::Tunnel.alpn())
                    .await
                {
                    held.push(conn);
                }
            }
            let client = endpoint(None, vec![]).await;
            let started = std::time::Instant::now();
            rig.connect(&client, Protocol::Tunnel).await.unwrap();
            assert!(started.elapsed() < network::HANDSHAKE_DEADLINE);
            assert_eq!(admissions(&admitted).await, 1);
            drop(held);
            client.close().await;
            mute.close().await;
        });
    }

    /// A flood that starts after the paired client sent its `hello` cannot
    /// evict it: past `hello`, a handshake keeps its place.
    #[test]
    fn a_flood_that_starts_mid_handshake_does_not_evict_a_client_past_hello() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let admitted = screened(&mut rig, 4).await;
            let client = endpoint(None, vec![]).await;
            let mut raw = rig.hello(&client).await;
            let mute = endpoint(None, vec![]).await;
            // The flood runs while the client finishes: waiting for all of it
            // could outlast the client's own deadline on a slow machine.
            let mut held = tokio::task::JoinSet::new();
            for _ in 0..40 {
                let (mute, target) = (mute.clone(), rig.target.clone());
                held.spawn(async move {
                    if let Ok(conn) = mute.connect(target, &Protocol::Tunnel.alpn()).await {
                        conn.closed().await;
                    }
                });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            let frame = rig.client_frame(&client.id(), &raw.nonce_s);
            write_frame(&mut raw.send, &frame).await.unwrap();
            assert!(matches!(
                read_frame(&mut raw.recv).await.unwrap(),
                Frame::Ready {}
            ));
            assert_eq!(admissions(&admitted).await, 1);
            drop(held);
            client.close().await;
            mute.close().await;
        });
    }

    /// A keyless attacker sends valid `hello`s and stalls: those handshakes are
    /// capped, the oldest gives way, and a stalled one is dropped within a few
    /// round trips. A paired client still connects while they keep arriving.
    #[test]
    fn stalled_valid_hellos_do_not_pin_the_budget() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let admitted = screened(&mut rig, 4).await;
            let attacker = endpoint(None, vec![]).await;
            let mut stalled = Vec::new();
            for _ in 0..12 {
                stalled.push(rig.hello(&attacker).await);
            }
            let client = endpoint(None, vec![]).await;
            rig.connect(&client, Protocol::Tunnel).await.unwrap();
            assert_eq!(admissions(&admitted).await, 1);
            // The last stalled hello is dropped by the post-proof deadline.
            let last = stalled.pop().unwrap();
            tokio::time::timeout(Duration::from_secs(4), last.conn.closed())
                .await
                .expect("a stalled handshake must be dropped");
            drop(stalled);
            client.close().await;
            attacker.close().await;
        });
    }

    /// A past-`hello` handshake evicted beyond the caps is closed with a code
    /// the client recognizes, not with a bare drop.
    #[test]
    fn an_evicted_handshake_is_closed_as_crowded() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let _admitted = screened(&mut rig, 4).await;
            let from = endpoint(None, vec![]).await;
            let victim = rig.hello(&from).await;
            let mut stalled = Vec::new();
            for _ in 0..60 {
                stalled.push(rig.hello(&from).await);
                if victim.conn.close_reason().is_some() {
                    break;
                }
            }
            let Some(ConnectionError::ApplicationClosed(close)) =
                tokio::time::timeout(Duration::from_secs(2), victim.conn.closed())
                    .await
                    .ok()
            else {
                panic!("the victim was never evicted");
            };
            assert_eq!(u64::from(close.error_code), u64::from(network::CROWDED));
            from.close().await;
        });
    }

    /// Stalled valid hellos keep arriving while the paired client handshakes:
    /// being crowded out is retried, and the client gets through.
    #[test]
    fn a_client_crowded_out_mid_handshake_retries_and_gets_through() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let _admitted = screened(&mut rig, 16).await;
            let attacker = endpoint(None, vec![]).await;
            let (target, remote) = (rig.target.clone(), rig.remote.slot.clone());
            let hostile = tokio::spawn({
                let attacker = attacker.clone();
                async move {
                    let mut held = Vec::new();
                    loop {
                        let alpn = Protocol::Tunnel.alpn();
                        let Ok(conn) = attacker.connect(target.clone(), &alpn).await else {
                            continue;
                        };
                        let Ok((mut send, mut recv)) = conn.open_bi().await else {
                            continue;
                        };
                        let hello = Frame::Hello {
                            version: VERSION,
                            slot: remote.clone(),
                            nonce_c: hex::encode([7u8; 32]),
                        };
                        let _ = write_frame(&mut send, &hello).await;
                        let _ = read_frame(&mut recv).await;
                        held.push((conn, send, recv));
                        if held.len() > 40 {
                            held.remove(0);
                        }
                    }
                }
            });
            let client = endpoint(None, vec![]).await;
            for _ in 0..3 {
                rig.connect(&client, Protocol::Tunnel).await.unwrap();
            }
            hostile.abort();
            client.close().await;
            attacker.close().await;
        });
    }

    /// The server side of a client that disappears after the proof: no `ready`.
    #[test]
    fn a_client_silent_after_the_proof_never_gets_ready() {
        in_home(|| async {
            let mut rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let from = endpoint(None, vec![]).await;
            let mut raw = rig.hello(&from).await;
            let started = std::time::Instant::now();
            let ready = tokio::time::timeout(Duration::from_secs(4), read_frame(&mut raw.recv))
                .await
                .expect("the server must give up");
            assert!(ready.is_err(), "no ready may follow");
            assert!(started.elapsed() < Duration::from_secs(3));
            assert!(rig.verdict().await.is_err());
            // Closing outright is a refusal too.
            let raw = rig.hello(&from).await;
            raw.conn.close(0u32.into(), b"");
            assert!(rig.verdict().await.is_err());
            from.close().await;
        });
    }

    /// A stranger learns neither the slot nor the protocol from a refusal:
    /// every wrong combination gets the same code and reason.
    #[test]
    fn strangers_learn_nothing_from_a_wrong_protocol_or_slot_refusal() {
        in_home(|| async {
            let rig = Rig::new(Protocol::Tunnel, "ssh", &["all"]).await;
            let stranger = endpoint(None, vec![]).await;
            for (protocol, slot) in [
                (Protocol::Pipe, "ssh"),
                (Protocol::Ping, "ssh"),
                (Protocol::Bench, "ssh"),
                (Protocol::Tunnel, "other"),
                (Protocol::Pipe, "other"),
            ] {
                let conn = stranger
                    .connect(rig.target.clone(), &protocol.alpn())
                    .await
                    .unwrap();
                let (mut send, _recv) = conn.open_bi().await.unwrap();
                let hello = Frame::Hello {
                    version: VERSION,
                    slot: slot.into(),
                    nonce_c: hex::encode([1u8; 32]),
                };
                write_frame(&mut send, &hello).await.unwrap();
                let Some(ConnectionError::ApplicationClosed(close)) = conn.closed().await.into()
                else {
                    panic!("expected an application close");
                };
                assert_eq!(u64::from(close.error_code), u64::from(WRONG_SLOT));
                assert_eq!(close.reason.as_ref(), b"unknown slot or protocol");
            }
            stranger.close().await;
        });
    }

    #[test]
    fn peers_only_ping_serves_the_peer_book_and_nothing_else() {
        in_home(|| async {
            let mut rig = Rig::with(Protocol::Ping, "web", &[], true).await;
            let client = endpoint(None, vec![]).await;
            // Any paired machine is served.
            rig.connect(&client, Protocol::Ping).await.unwrap();
            rig.verdict().await.unwrap();
            // A removed machine is refused with the usual reason.
            PeerBook::update(|book| book.remove("laptop")).unwrap();
            let error = rig.connect(&client, Protocol::Ping).await.unwrap_err();
            assert_eq!(
                error,
                "nas does not know this machine; run `iroh-toolbox pair` on both machines"
            );
            // A client without identity cannot be served either.
            let result = connect_addr(
                &client,
                rig.target.clone(),
                &rig.remote,
                Protocol::Ping,
                TIMEOUT,
                None,
            )
            .await;
            assert!(result.is_err());
            client.close().await;
        });
    }

    #[test]
    fn allow_narrows_a_peers_only_ping_server() {
        in_home(|| async {
            let rig = Rig::with(Protocol::Bench, "bench", &["phone"], true).await;
            assert_eq!(rig.server.serving, ["phone"]);
            let client = endpoint(None, vec![]).await;
            let error = rig.connect(&client, Protocol::Bench).await.unwrap_err();
            assert_eq!(
                error,
                "this nas server (slot bench) does not allow this machine (--allow)"
            );
            client.close().await;
            // Open servers take no --allow; peers-only ones need a known name.
            assert!(Server::open("web", Protocol::Ping, &["phone".into()], false).is_err());
            // `all` is no exception: an open server has no one to name.
            let error = Server::open("web", Protocol::Ping, &["all".into()], false)
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("--allow applies to peer services"));
            let error = Server::open("web", Protocol::Ping, &["ghost".into()], true)
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("no paired machine named ghost"));
        });
    }

    /// An impostor that computes the slot key from the address cannot pass as
    /// the machine: it lacks the key, relays a proof made for another
    /// connection, or answers with a key other than the one in the peer book.
    #[test]
    fn an_impostor_holding_the_slot_key_cannot_pass_as_the_server() {
        in_home(|| async {
            let rig = Rig::new(Protocol::Ping, "web", &[]).await;
            let slot_key = rig.remote.address.slot_key("web");
            let real = rig.target.clone();
            // 1. Relays the real server's proof, which names the impostor's upstream endpoint.
            let relaying = endpoint(Some(slot_key.clone()), all_alpns()).await;
            let relaying_addr = relaying.addr();
            let upstream = endpoint(None, vec![]).await;
            // The impostors answer every attempt: the client may retry one
            // that a slow network cut short.
            tokio::spawn(async move {
                while let Some(incoming) = relaying.accept().await {
                    let (upstream, real) = (upstream.clone(), real.clone());
                    tokio::spawn(async move {
                        let conn = incoming.await.unwrap();
                        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
                        let hello = read_frame(&mut recv).await.unwrap();
                        let real_conn = upstream
                            .connect(real, &Protocol::Ping.alpn())
                            .await
                            .unwrap();
                        let (mut up_send, mut up_recv) = real_conn.open_bi().await.unwrap();
                        write_frame(&mut up_send, &hello).await.unwrap();
                        let proof = read_frame(&mut up_recv).await.unwrap();
                        write_frame(&mut send, &proof).await.unwrap();
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    });
                }
            });
            // 2. Signs with its own machine key.
            let own = SecretKey::generate();
            let signing = endpoint(Some(slot_key), all_alpns()).await;
            let signing_addr = signing.addr();
            let (key, id) = (own.clone(), signing.id());
            tokio::spawn(async move {
                while let Some(incoming) = signing.accept().await {
                    let key = key.clone();
                    tokio::spawn(async move {
                        let conn = incoming.await.unwrap();
                        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
                        let Frame::Hello { nonce_c, .. } = read_frame(&mut recv).await.unwrap()
                        else {
                            panic!("expected hello");
                        };
                        let message = statement(
                            SERVER_DOMAIN,
                            Protocol::Ping,
                            "web",
                            &id,
                            &conn.remote_id(),
                            &decode(&nonce_c).unwrap(),
                        );
                        let proof = Frame::Proof {
                            signature: hex::encode(key.sign(&message).to_bytes()),
                            nonce_s: hex::encode([3u8; 32]),
                            client: false,
                        };
                        write_frame(&mut send, &proof).await.unwrap();
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    });
                }
            });
            // 3. Closes with a peer-book verdict without proving anything: the
            // client must not repeat a claim about the book or the filter.
            let client = endpoint(None, vec![]).await;
            for code in [NOT_PAIRED, NOT_ALLOWED, INVALID_PROOF] {
                let closing = endpoint(Some(rig.remote.address.slot_key("web")), all_alpns()).await;
                let closing_addr = closing.addr();
                // Closed below: every impostor shares the slot key, so the
                // same endpoint ID must not stay reachable for the next one.
                let accepting = closing.clone();
                tokio::spawn(async move {
                    while let Some(incoming) = accepting.accept().await {
                        tokio::spawn(async move {
                            let conn = incoming.await.unwrap();
                            let _ = conn.accept_bi().await.unwrap();
                            conn.close(code.into(), b"not paired");
                            conn.closed().await;
                        });
                    }
                });
                let error = connect_addr(
                    &client,
                    closing_addr,
                    &rig.remote,
                    Protocol::Ping,
                    TIMEOUT,
                    None,
                )
                .await
                .err()
                .unwrap()
                .to_string();
                assert!(error.contains("its refusal reason is untrusted"), "{error}");
                assert!(!error.contains("does not know") && !error.contains("--allow"));
                closing.close().await;
            }
            // A proof that does not verify is a local verdict, whatever close follows.
            for target in [relaying_addr, signing_addr] {
                let error =
                    connect_addr(&client, target, &rig.remote, Protocol::Ping, TIMEOUT, None)
                        .await
                        .err()
                        .unwrap()
                        .to_string();
                assert!(
                    error.starts_with("cannot authenticate nas: its response does not match"),
                    "{error}"
                );
                assert!(!error.contains("different machine key"), "{error}");
            }
            client.close().await;
        });
    }

    /// After a verified proof, 33 is reported as the server's rejection of this
    /// machine's proof; the close text it sends is never shown.
    #[test]
    fn a_verified_invalid_proof_refusal_is_explained_locally() {
        in_home(|| async {
            let rig = Rig::new(Protocol::Pipe, "backup", &["all"]).await;
            let server = rig.server.clone();
            let ep = endpoint(Some(rig.remote.address.slot_key("backup")), all_alpns()).await;
            let target = ep.addr();
            tokio::spawn(async move {
                let conn = ep.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = conn.accept_bi().await.unwrap();
                let Frame::Hello { nonce_c, .. } = read_frame(&mut recv).await.unwrap() else {
                    panic!("expected hello");
                };
                let message = statement(
                    SERVER_DOMAIN,
                    Protocol::Pipe,
                    "backup",
                    &ep.id(),
                    &conn.remote_id(),
                    &decode(&nonce_c).unwrap(),
                );
                let proof = Frame::Proof {
                    signature: hex::encode(server.identity.sign(&message).to_bytes()),
                    nonce_s: hex::encode([3u8; 32]),
                    client: true,
                };
                write_frame(&mut send, &proof).await.unwrap();
                let _ = read_frame(&mut recv).await;
                conn.close(INVALID_PROOF.into(), b"\x1b[2Jforged words");
                conn.closed().await;
                ep.close().await;
            });
            let client = endpoint(None, vec![]).await;
            let error = connect_addr(
                &client,
                target,
                &rig.remote,
                Protocol::Pipe,
                TIMEOUT,
                Some(&rig.client),
            )
            .await
            .err()
            .unwrap()
            .to_string();
            assert_eq!(
                error,
                "nas rejected this machine's authentication proof (invalid client proof); check the local identity and protocol implementation"
            );
            client.close().await;
        });
    }

    #[test]
    fn statements_bind_the_role_protocol_slot_endpoints_and_nonce() {
        let key = |seed: u8| SecretKey::from_bytes(&[seed; 32]).public();
        let base = |domain: &[u8], protocol, slot: &str, a, b, nonce: [u8; 32]| {
            statement(domain, protocol, slot, &key(a), &key(b), &nonce)
        };
        let reference = base(SERVER_DOMAIN, Protocol::Tunnel, "ssh", 1, 2, [0; 32]);
        for other in [
            base(CLIENT_DOMAIN, Protocol::Tunnel, "ssh", 1, 2, [0; 32]),
            base(SERVER_DOMAIN, Protocol::Pipe, "ssh", 1, 2, [0; 32]),
            base(SERVER_DOMAIN, Protocol::Tunnel, "ss", 1, 2, [0; 32]),
            base(SERVER_DOMAIN, Protocol::Tunnel, "ssh", 3, 2, [0; 32]),
            base(SERVER_DOMAIN, Protocol::Tunnel, "ssh", 1, 3, [0; 32]),
            base(SERVER_DOMAIN, Protocol::Tunnel, "ssh", 1, 2, [1; 32]),
        ] {
            assert_ne!(reference, other);
        }
    }

    #[test]
    fn slot_alpns_are_distinct_and_versioned() {
        let alpns = all_alpns();
        assert_eq!(alpns.len(), 8);
        assert!(alpns.iter().all(|a| a.starts_with(b"iroh-toolbox/slot/2/")));
        for protocol in Protocol::ALL {
            assert_eq!(Protocol::from_alpn(&protocol.alpn()), Some(protocol));
        }
        assert!(Protocol::from_alpn(b"iroh-toolbox/slot/1/ping").is_none());
        assert!(Protocol::Tunnel.requires_client() && Protocol::Pipe.requires_client());
        assert!(Protocol::Inbox.requires_client() && Protocol::Send.requires_client());
        assert!(Protocol::Chat.requires_client());
        assert!(!Protocol::Ping.requires_client() && !Protocol::Bench.requires_client());
    }

    /// Frozen vector: a change to an expected value is a wire-format change
    /// (a client and a server of different builds could no longer authenticate).
    #[test]
    fn slot_statements_match_the_frozen_bytes() {
        let key = |seed: u8| SecretKey::from_bytes(&[seed; 32]).public();
        for (domain, expected) in [
            (
                SERVER_DOMAIN,
                "69726f682d746f6f6c626f782f736c6f742d617574682f76320a74756e6e656c2d746370037373688a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b3940707070707070707070707070707070707070707070707070707070707070707",
            ),
            (
                CLIENT_DOMAIN,
                "69726f682d746f6f6c626f782f706565722d617574682f76310a74756e6e656c2d746370037373688a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b3940707070707070707070707070707070707070707070707070707070707070707",
            ),
        ] {
            let bytes = statement(domain, Protocol::Tunnel, "ssh", &key(1), &key(2), &[7; 32]);
            assert_eq!(hex::encode(bytes), expected);
        }
    }

    #[test]
    fn slot_frame_parser_survives_mutated_and_random_input() {
        crate::fuzz::check_decoder(
            &[
                r#"{"type":"hello","version":2,"slot":"ssh","nonce_c":"00"}"#,
                r#"{"type":"proof","signature":"00","nonce_s":"11","client":true}"#,
                r#"{"type":"proof","signature":"00","nonce_s":"11"}"#,
                r#"{"type":"client","machine":"ab","signature":"cd"}"#,
                r#"{"type":"ready"}"#,
                r#"{"type":"ready"}"#,
            ],
            &[
                ("hello", &["version", "slot", "nonce_c"]),
                ("proof", &["signature", "nonce_s", "client"]),
                ("client", &["machine", "signature"]),
                ("ready", &[]),
            ],
            parse_frame,
        );
    }

    #[test]
    fn slot_hex_fields_accept_only_their_exact_length() {
        let mut rng = crate::fuzz::Rng::new(7);
        let alphabet: Vec<char> = "0123456789abcdefABCDEFxyz -\u{e9}".chars().collect();
        let (mut accepted, mut total) = (0, 0);
        for _ in 0..4000 {
            let length = rng.below(80);
            let text: String = (0..length).map(|_| *rng.pick(&alphabet)).collect();
            total += 1;
            if let Ok(bytes) = decode::<32>(&text) {
                accepted += 1;
                assert_eq!(text.len(), 64);
                assert_eq!(hex::encode(bytes), text.to_lowercase());
            }
        }
        assert!(decode::<32>(&"ab".repeat(32)).is_ok());
        assert!(accepted < total);
    }
}
