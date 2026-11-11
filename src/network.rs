use std::{
    io::{IsTerminal, Write},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, SecretKey, TransportAddr,
    Watcher,
    address_lookup::{DnsAddressLookup, PkarrPublisher, PkarrResolver},
    endpoint::{BindOpts, Builder, Connection, NetReportConfig, presets},
};
use iroh_tickets::endpoint::EndpointTicket;
use n0_future::StreamExt;
use serde_json::{Value, json};

use crate::config::Settings;

pub const PING_ALPN: &[u8] = b"iroh-toolbox/ping/1";
pub const BENCH_ALPN: &[u8] = b"iroh-toolbox/bench/1";

fn relay_mode(settings: &Settings) -> RelayMode {
    if settings.direct_only {
        return RelayMode::Disabled;
    }
    if let Some(url) = &settings.relay {
        let mut config = RelayConfig::new(url.clone(), None);
        if let Some(token) = &settings.token {
            config = config.with_auth_token(token.clone());
        }
        let map = RelayMap::empty();
        map.insert(url.clone(), Arc::new(config));
        RelayMode::Custom(map)
    } else {
        RelayMode::Default
    }
}

pub async fn bind(settings: &Settings, alpns: Vec<Vec<u8>>) -> Result<Endpoint> {
    bind_on(settings, alpns, None).await
}

/// Like [`bind`], on a fixed UDP port when `port` is given (`--port`).
pub async fn bind_on(
    settings: &Settings,
    alpns: Vec<Vec<u8>>,
    port: Option<u16>,
) -> Result<Endpoint> {
    bind_with(settings, alpns, relay_mode(settings), port, |builder| {
        builder
    })
    .await
}

/// Whether endpoints use iroh's public relays (and therefore n0 address lookup for pairing).
pub fn public_relays(settings: &Settings) -> bool {
    settings.relay.is_none() && !settings.direct_only
}

/// An endpoint that can reach a pairing rendezvous or a paired machine by its endpoint ID alone. With
/// public relays it resolves addresses through n0's DNS; a custom relay needs no lookup.
pub async fn bind_resolving(settings: &Settings, alpns: Vec<Vec<u8>>) -> Result<Endpoint> {
    let public = public_relays(settings);
    bind_with(settings, alpns, relay_mode(settings), None, |builder| {
        if public {
            builder
                .address_lookup(PkarrResolver::n0_dns())
                .address_lookup(DnsAddressLookup::n0_dns())
        } else {
            builder
        }
    })
    .await
}

/// A receiving endpoint whose key is kept to resume a partial transfer. It
/// needs no address lookup: the sender's ticket carries its addresses.
pub async fn bind_receiver(settings: &Settings, key: SecretKey) -> Result<Endpoint> {
    bind_with(settings, vec![], relay_mode(settings), None, |builder| {
        builder.secret_key(key)
    })
    .await
}

/// The temporary endpoint a joiner pairs from: always relay-only, with the
/// address lookup that reaches a rendezvous by its endpoint ID. The rendezvous
/// key derives from public words, so a squatter may answer in the creator's
/// place; with IP transports iroh would hand it the joiner's direct addresses
/// (NAT traversal candidates) before SPAKE2 fails. The joiner's transfer
/// endpoint keeps its direct transports and is announced only inside the
/// authenticated channel.
pub async fn bind_joiner(settings: &Settings) -> Result<Endpoint> {
    let mut relayed = settings.clone();
    relayed.relay_only = true;
    bind_resolving(&relayed, vec![]).await
}

/// An endpoint with a key derived from a code or a slot address, published
/// through n0's DNS when public relays are used. Its key is not secret: the
/// pairing or slot handshake authenticates the peer.
pub async fn bind_keyed(
    settings: &Settings,
    key: SecretKey,
    alpns: Vec<Vec<u8>>,
) -> Result<Endpoint> {
    bind_keyed_on(settings, key, alpns, None).await
}

/// Like [`bind_keyed`], on a fixed UDP port when `port` is given (`--port`).
pub async fn bind_keyed_on(
    settings: &Settings,
    key: SecretKey,
    alpns: Vec<Vec<u8>>,
    port: Option<u16>,
) -> Result<Endpoint> {
    let public = public_relays(settings);
    bind_with(settings, alpns, relay_mode(settings), port, |builder| {
        let builder = builder.secret_key(key);
        if public {
            builder.address_lookup(PkarrPublisher::n0_dns())
        } else {
            builder
        }
    })
    .await
}

/// The rendezvous endpoint of a pairing code: a keyed endpoint that is always
/// relay-only. Its key derives from public words, so anyone may connect to it
/// before any authentication; with IP transports, iroh would hand that stranger
/// the creator's direct addresses (NAT traversal candidates) and open a direct
/// path. Pairing exchanges a few kilobytes, so the relay costs nothing.
pub async fn bind_rendezvous(
    settings: &Settings,
    key: SecretKey,
    alpns: Vec<Vec<u8>>,
) -> Result<Endpoint> {
    let mut relayed = settings.clone();
    relayed.relay_only = true;
    bind_keyed(&relayed, key, alpns).await
}

#[cfg(test)]
async fn bind_with_mode(
    settings: &Settings,
    alpns: Vec<Vec<u8>>,
    mode: RelayMode,
) -> Result<Endpoint> {
    bind_with(settings, alpns, mode, None, |builder| builder).await
}

async fn bind_with(
    settings: &Settings,
    alpns: Vec<Vec<u8>>,
    mode: RelayMode,
    port: Option<u16>,
    configure: impl FnOnce(Builder) -> Builder,
) -> Result<Endpoint> {
    // A private relay intentionally redirects HTTP to HTTPS. Its public HTTPS
    // latency probe suffices; avoid the captive portal heuristic over plain HTTP.
    let mut probes = NetReportConfig::default();
    probes.captive_portal_check = false;
    let mut builder = Endpoint::builder(presets::Minimal)
        .clear_address_lookup()
        .net_report_config(probes)
        .relay_mode(mode)
        .alpns(alpns);
    if settings.relay_only {
        builder = builder.clear_ip_transports();
    }
    builder = configure(builder);
    if let Some(port) = port {
        ensure!(
            !settings.relay_only,
            "--port fixes a UDP port, which --relay-only disables"
        );
        // IPv6 is optional; an occupied IPv4 port fails the bind.
        builder = builder
            .bind_addr(format!("0.0.0.0:{port}"))?
            .bind_addr_with_opts(
                format!("[::]:{port}"),
                BindOpts::default().set_is_required(false),
            )?;
    }
    let ep = builder.bind().await.with_context(|| match port {
        Some(port) => format!("cannot bind UDP port {port}; is it already in use?"),
        None => "cannot open iroh endpoint".to_owned(),
    })?;
    if !settings.direct_only
        && let Err(error) = wait_online(&ep, settings).await
    {
        if settings.relay.is_some() || settings.relay_only {
            ep.close().await;
            return Err(error);
        }
        note!(
            "public relays unavailable; continuing with direct addresses only (use --direct-only to skip relay registration)"
        );
    }
    Ok(ep)
}

async fn wait_online(ep: &Endpoint, settings: &Settings) -> Result<()> {
    let mut statuses = ep.home_relay_status().stream();
    let mut last_error = None;
    let deadline = tokio::time::sleep(settings.timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = ep.online() => return Ok(()),
            next = statuses.next() => {
                let Some(relays) = next else { bail!("relay status watcher closed"); };
                for relay in relays {
                    if let Some(reason) = relay.auth_denied_reason() {
                        bail!("relay authentication denied: {}; select a token accepted by this relay (without the Bearer prefix)", settings.redact(reason));
                    }
                    if let Some(error) = relay.last_error() {
                        last_error = Some(settings.redact(format!("{error:#}")));
                    }
                }
            }
            _ = &mut deadline => {
                if let Some(error) = last_error { bail!("relay registration failed: {error}"); }
                bail!("relay registration timed out: no usable relay; check DNS/TLS and HTTP probes /ping and /generate_204 (these probes do not send bearer)");
            }
        }
    }
}

/// Whether a server already serves `max` connections; it notes the refusal
/// once per saturation so the operator sees why clients fail.
pub fn at_capacity(active: usize, max: u64) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if (active as u64) < max {
        REPORTED.store(false, Ordering::Relaxed);
        return false;
    }
    if !REPORTED.swap(true, Ordering::Relaxed) {
        note!("refusing new connections: {max} already open (--max-connections)");
    }
    true
}

pub fn ticket(ep: &Endpoint) -> String {
    EndpointTicket::new(ep.addr()).to_string()
}

/// Values made only of these characters need no quoting in a copied shell command.
pub fn shell_safe(value: &str) -> bool {
    value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ":/._-~%[]".contains(c))
}

/// Global options the other machine needs: a custom relay URL (when shell-safe)
/// and the transport mode. Returns whether the relay URL is included; its
/// access token never is.
pub fn remote_globals(settings: &Settings) -> (String, bool) {
    let mut args = String::new();
    let mut relay_shown = true;
    if let Some(relay) = &settings.relay {
        relay_shown = shell_safe(relay.as_str());
        if relay_shown {
            args.push_str(&format!(" --relay {relay}"));
        }
    }
    args.push_str(transport_options(settings));
    (args, relay_shown)
}

/// What a copied command cannot carry: a relay URL needing quotes, or this
/// machine's access token, which the other machine replaces with its own.
pub fn relay_hint(settings: &Settings, relay_shown: bool) {
    if settings.relay.is_none() {
        return;
    }
    if !relay_shown {
        note!(
            "Configure the same relay and your own access token on the other machine first (see relay check)."
        );
    } else if settings.token.is_some() {
        note!(
            "Configure your own access token for this relay on the other machine first (see relay check)."
        );
    }
}

pub fn transport_options(settings: &Settings) -> &'static str {
    if settings.direct_only {
        " --direct-only"
    } else if settings.relay_only {
        " --relay-only"
    } else {
        ""
    }
}

/// Print a ticket or code alone for scripts (stdout, or stderr when stdout
/// carries data). On a terminal the copyable command carries it instead,
/// unless hints are hidden.
pub fn share(settings: &Settings, value: &str, stderr: bool) -> Result<()> {
    if stderr {
        if settings.no_hints || !std::io::stderr().is_terminal() {
            note!("{value}");
        }
    } else if settings.no_hints || !std::io::stdout().is_terminal() {
        out!("{value}");
        std::io::stdout().flush()?;
    }
    Ok(())
}

/// Tickets generated here contain no shell metacharacters. Do not interpolate
/// local paths, configuration paths or credentials into a remote shell command.
pub fn remote_command(settings: &Settings, command: &str, ticket: &str, args: &str) {
    if settings.no_hints {
        return;
    }
    let (globals, relay_shown) = remote_globals(settings);
    note!("On the other machine, run:");
    note!("  iroh-toolbox{globals} {command} \"{ticket}\"{args}");
    relay_hint(settings, relay_shown);
}

/// The command for the other machine to reach a slot of this one by name. The
/// name is the one the other machine may have chosen differently, so the hint
/// says where to look it up.
pub fn peer_command(settings: &Settings, command: &str, peer: &str, args: &str) {
    if settings.no_hints {
        return;
    }
    let (globals, relay_shown) = remote_globals(settings);
    note!("On the other machine, run:");
    note!("  iroh-toolbox{globals} {command} {peer}{args}");
    note!(
        "Use the name this machine has in that machine's `iroh-toolbox peers` list if it differs."
    );
    relay_hint(settings, relay_shown);
}

pub fn check_addr(addr: &EndpointAddr, settings: &Settings) -> Result<()> {
    if settings.direct_only {
        ensure!(
            addr.ip_addrs().next().is_some(),
            "--direct-only needs a ticket with direct addresses"
        );
        return Ok(());
    }
    let allowed = relay_mode(settings).relay_map();
    for url in addr.relay_urls() {
        ensure!(
            allowed.contains(url),
            "ticket relay differs from the selected relays; configure its URL with --relay"
        );
    }
    if settings.relay_only {
        ensure!(
            addr.relay_urls().next().is_some(),
            "ticket has no relay address"
        );
    }
    Ok(())
}

pub fn target(value: &str, settings: &Settings) -> Result<EndpointAddr> {
    let addr = if value.starts_with("endpoint") {
        value
            .parse::<EndpointTicket>()
            .context("invalid endpoint ticket")?
            .endpoint_addr()
            .clone()
    } else {
        let id: EndpointId = value
            .parse()
            .context("expected an endpoint ticket or endpoint ID")?;
        let mut addr = EndpointAddr::new(id);
        if let Some(relay) = &settings.relay {
            addr = addr.with_relay_url(relay.clone());
        } else if !settings.direct_only {
            for relay in relay_mode(settings).relay_map().urls::<Vec<_>>() {
                addr = addr.with_relay_url(relay);
            }
        }
        addr
    };
    check_addr(&addr, settings)?;
    Ok(addr)
}

pub async fn connect(
    ep: &Endpoint,
    value: &str,
    alpn: &[u8],
    settings: &Settings,
) -> Result<Connection> {
    tokio::time::timeout(settings.timeout, ep.connect(target(value, settings)?, alpn))
        .await
        .context("peer connection timed out; keep the remote command running, copy its current ticket and check relay configuration on both machines")?
        .context("cannot connect to peer; keep the matching remote command running and copy its current ticket")
}

/// Time an unauthenticated connection has to finish its handshake. Short and
/// independent of `--timeout`: whoever knows a slot's endpoint ID can open
/// connections and stay silent, and must not hold the server for long.
pub const HANDSHAKE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Least time a client has to open its first stream and send `hello` once the
/// transport is up; see [`hello_deadline`].
pub const HELLO_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// The time a connected client gets to send `hello`: [`HELLO_DEADLINE`], or
/// four round trips when the path is slower than that.
pub fn hello_deadline(conn: &Connection) -> std::time::Duration {
    let rtt = conn
        .paths()
        .iter()
        .find(|path| path.is_selected())
        .map_or(std::time::Duration::ZERO, |path| path.rtt());
    HELLO_DEADLINE.max(rtt * 4).min(HANDSHAKE_DEADLINE)
}

/// Unauthenticated handshakes a server runs at once. They have a budget of
/// their own, so strangers cannot use up the `--max-connections` of
/// authenticated clients. When it is full, sources that have not proven their
/// address are sent a retry (cheap, and spoofed sources go nowhere) and the
/// oldest handshake that has not yet sent a valid `hello` makes room for the
/// newcomer.
///
/// A `hello` costs an attacker nothing (it needs no key), so past-`hello`
/// handshakes are bounded too: at most half of the budget, and
/// [`PAST_HELLO_PER_SOURCE`] per direct source address. Beyond that the oldest
/// past-`hello` handshake gives way, so eviction is always possible. What
/// stays is short: the client frame must follow the proof within a few round
/// trips (see [`client_deadline`]).
pub const PENDING_HANDSHAKES: usize = 128;

/// Past-`hello` handshakes one direct IP address may hold at once. Relayed
/// connections are not capped per source: they may all come from one relay.
pub const PAST_HELLO_PER_SOURCE: usize = 8;

/// The time a client has to send its proof once the server sent its own: a few
/// round trips, at least a second.
pub fn client_deadline(conn: &Connection) -> std::time::Duration {
    let rtt = conn
        .paths()
        .iter()
        .find(|path| path.is_selected())
        .map_or(std::time::Duration::ZERO, |path| path.rtt());
    std::time::Duration::from_secs(1)
        .max(rtt * 3)
        .min(HANDSHAKE_DEADLINE)
}

struct Entry {
    id: u64,
    past_hello: bool,
    source: Option<std::net::IpAddr>,
    evict: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Where a handshake marks its `hello`.
#[derive(Clone)]
pub struct Marker {
    pool: Pending,
    id: u64,
}

tokio::task_local! {
    static HELLO: Marker;
}

/// Runs a handshake whose `hello` is marked through `marker`.
pub async fn with_hello<T>(marker: Marker, handshake: impl Future<Output = T>) -> T {
    HELLO.scope(marker, handshake).await
}

/// Records that the current handshake's client sent a valid `hello`; this may
/// evict older past-`hello` handshakes beyond the caps.
pub fn mark_hello() {
    let _ = HELLO.try_with(|marker| marker.pool.mark(marker.id));
}

/// The unauthenticated handshakes of a server, oldest first.
#[derive(Clone)]
pub struct Pending {
    max: usize,
    queue: Arc<std::sync::Mutex<std::collections::VecDeque<Entry>>>,
    next: Arc<std::sync::atomic::AtomicU64>,
}

impl Default for Pending {
    fn default() -> Self {
        Self::new(PENDING_HANDSHAKES)
    }
}

/// A place in [`Pending`], released on drop.
pub struct Handshaking {
    id: u64,
    pool: Pending,
    evicted: tokio::sync::oneshot::Receiver<()>,
}

impl Pending {
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(2),
            queue: Arc::default(),
            next: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::VecDeque<Entry>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes a place for `incoming`; an unvalidated source gets a retry when
    /// the budget is full.
    pub fn screen(
        &self,
        incoming: iroh::endpoint::Incoming,
    ) -> Option<(iroh::endpoint::Incoming, Handshaking)> {
        if self.lock().len() >= self.max && !incoming.remote_addr_validated() {
            let _ = incoming.retry();
            return None;
        }
        let source = match incoming.remote_addr() {
            iroh::endpoint::IncomingAddr::Ip(addr) => Some(addr.ip()),
            _ => None,
        };
        let handshaking = self.admit(source);
        Some((incoming, handshaking))
    }

    /// Registers a handshake. When full, the oldest handshake without `hello`
    /// makes room, or the oldest one if all are past it.
    pub fn admit(&self, source: Option<std::net::IpAddr>) -> Handshaking {
        let source = source.map(source_key);
        let mut queue = self.lock();
        if queue.len() >= self.max {
            // Entries that never sent `hello` go first, oldest first; among
            // past-`hello` entries the victim is random, so that an attacker
            // cannot time its hellos against one client's few round trips.
            let position = queue
                .iter()
                .position(|entry| !entry.past_hello)
                .or_else(|| random_position(&queue, |_| true));
            if let Some(position) = position {
                Self::evict(&mut queue, position);
            }
        }
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (evict, evicted) = tokio::sync::oneshot::channel();
        queue.push_back(Entry {
            id,
            past_hello: false,
            source,
            evict: Some(evict),
        });
        Handshaking {
            id,
            pool: self.clone(),
            evicted,
        }
    }

    fn evict(queue: &mut std::collections::VecDeque<Entry>, position: usize) {
        if let Some(mut entry) = queue.remove(position)
            && let Some(evict) = entry.evict.take()
        {
            let _ = evict.send(());
        }
    }

    /// Moves a handshake past `hello`, keeping the caps on that state.
    fn mark(&self, id: u64) {
        let mut queue = self.lock();
        let Some(me) = queue.iter().position(|entry| entry.id == id) else {
            return;
        };
        queue[me].past_hello = true;
        let source = queue[me].source;
        // A random other past-hello entry gives way when a cap is exceeded.
        let oldest = |queue: &std::collections::VecDeque<Entry>, same_source: bool| {
            random_position(queue, |entry| {
                entry.past_hello && entry.id != id && (!same_source || entry.source == source)
            })
        };
        if let Some(ip) = source {
            let held = queue
                .iter()
                .filter(|entry| entry.past_hello && entry.source == Some(ip))
                .count();
            if held > PAST_HELLO_PER_SOURCE
                && let Some(position) = oldest(&queue, true)
            {
                Self::evict(&mut queue, position);
            }
        }
        let past = queue.iter().filter(|entry| entry.past_hello).count();
        if past > self.max / 2
            && let Some(position) = oldest(&queue, false)
        {
            Self::evict(&mut queue, position);
        }
    }
}

/// A uniformly random entry satisfying `keep`.
fn random_position(
    queue: &std::collections::VecDeque<Entry>,
    keep: impl Fn(&Entry) -> bool,
) -> Option<usize> {
    let candidates: Vec<usize> = (0..queue.len()).filter(|&i| keep(&queue[i])).collect();
    if candidates.is_empty() {
        return None;
    }
    let pick = crate::identity::random::<8>().map_or(0, |bytes| u64::from_le_bytes(bytes) as usize);
    Some(candidates[pick % candidates.len()])
}

/// The unit a direct source is counted by: a full IPv4 address, but a whole
/// /64 for IPv6, where one host routinely owns billions of addresses.
fn source_key(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => {
            let high = u128::from(v6) & (u128::MAX << 64);
            std::net::IpAddr::V6(high.into())
        }
        v4 => v4,
    }
}

impl Handshaking {
    /// Completes when a newer handshake took this one's place.
    pub async fn evicted(&mut self) {
        let _ = (&mut self.evicted).await;
    }

    /// Where the handshake marks its `hello`.
    pub fn marker(&self) -> Marker {
        Marker {
            pool: self.pool.clone(),
            id: self.id,
        }
    }
}

impl Drop for Handshaking {
    fn drop(&mut self) {
        self.pool.lock().retain(|entry| entry.id != self.id);
    }
}

/// A shared count of live connections or handshakes.
#[derive(Clone, Default)]
pub struct Counter(Arc<std::sync::atomic::AtomicUsize>);

/// One unit of a [`Counter`], released on drop.
pub struct Held(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Counter {
    pub fn get(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Takes a unit unless `max` are already held.
    pub fn try_take(&self, max: u64) -> Option<Held> {
        self.0
            .try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |held| ((held as u64) < max).then_some(held + 1),
            )
            .ok()
            .map(|_| Held(self.0.clone()))
    }
}

/// Accepts connections until `admit` succeeds on one, for servers that serve a
/// single client. Failed attempts (another slot, a stranger, a broken
/// handshake) are reported with -v and do not end the server; they run
/// concurrently so a slow client cannot hold the door, and each is dropped
/// after `deadline`.
pub async fn accept_first<T, F, Fut>(
    ep: &Endpoint,
    deadline: std::time::Duration,
    command: &'static str,
    admit: F,
) -> Result<T>
where
    F: Fn(Connection) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let mut attempts = tokio::task::JoinSet::new();
    let pending = Pending::default();
    loop {
        tokio::select! {
            incoming = ep.accept() => {
                let incoming = incoming.context("endpoint closed")?;
                let Some((incoming, mut handshaking)) = pending.screen(incoming) else {
                    continue;
                };
                let admit = admit.clone();
                attempts.spawn(with_hello(handshaking.marker(), async move {
                    let end = tokio::time::Instant::now() + deadline;
                    let conn = tokio::select! {
                        conn = tokio::time::timeout_at(end, incoming) => {
                            conn.context("peer handshake timed out")?.context("peer handshake failed")?
                        }
                        () = handshaking.evicted() => anyhow::bail!("dropped to make room for a newer connection"),
                    };
                    tokio::select! {
                        result = tokio::time::timeout_at(end, admit(conn.clone())) => {
                            result.context("peer handshake timed out")?
                        }
                        // A newer connection took this one's place.
                        () = handshaking.evicted() => {
                            conn.close(CROWDED.into(), CROWDED_REASON);
                            anyhow::bail!("dropped to make room for a newer connection")
                        }
                    }
                }));
            }
            Some(done) = attempts.join_next() => match done {
                Ok(Ok(admitted)) => return Ok(admitted),
                Ok(Err(error)) => detail!("{command}: ignored a connection: {error:#}"),
                Err(error) => return Err(error.into()),
            },
        }
    }
}

/// Close code of a handshake dropped to make room for newer ones. Not a verdict
/// on the client: after the server proved itself it is authentic, and the client
/// tries again.
pub const CROWDED: u32 = 23;
pub const CROWDED_REASON: &[u8] = b"too many handshakes at once";

/// Close code of a single-client server that already serves its client.
pub const BUSY: u32 = 22;
pub const BUSY_REASON: &[u8] = b"the server is already serving another client";
/// The busy reason of a chat that already has its participant.
pub const CHAT_BUSY_REASON: &[u8] = b"a chat session is in progress";

/// Turns away clients that arrive while a single-client server is in use, with
/// a reason they show. Dropping the guard stops refusing.
pub struct Busy(tokio::task::JoinHandle<()>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Only a client that passed the server's handshake (the proof, for servers
/// that ask for one) learns that the server is busy; strangers learn nothing.
pub fn refuse_while_busy(ep: &Endpoint, server: Option<Arc<crate::slot::Server>>) -> Busy {
    refuse_while_busy_with(ep, server, BUSY_REASON)
}

/// Like [`refuse_while_busy`], with the reason the clients are given.
pub fn refuse_while_busy_with(
    ep: &Endpoint,
    server: Option<Arc<crate::slot::Server>>,
    reason: &'static [u8],
) -> Busy {
    let ep = ep.clone();
    Busy(tokio::spawn(async move {
        let pending = Pending::default();
        // Dropped with the guard: the handshakes in flight stop with it.
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                incoming = ep.accept() => {
                    let Some(incoming) = incoming else { break };
                    let Some((incoming, mut handshaking)) = pending.screen(incoming) else {
                        continue;
                    };
                    let server = server.clone();
                    tasks.spawn(async move {
                        let admitted = crate::slot::accept(
                            server.as_deref(),
                            incoming,
                            "busy",
                            &mut handshaking,
                        )
                        .await;
                        if let Some(conn) = admitted {
                            conn.close(BUSY.into(), reason);
                        }
                    });
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
    }))
}

/// Reasons a busy server gives, each with the text shown in its place: the
/// reason itself comes from the network and is never printed.
const BUSY_REASONS: [(&[u8], &str); 3] = [
    (
        b"the server is at its connection limit",
        "is at its connection limit; try again later",
    ),
    (
        BUSY_REASON,
        "is already serving another client; try again later",
    ),
    (
        CHAT_BUSY_REASON,
        "is in a chat session with another participant; try again later",
    ),
];

/// What to say about a busy refusal with `reason`, after the subject.
pub fn busy_message(reason: &[u8]) -> &'static str {
    BUSY_REASONS
        .iter()
        .find(|(known, _)| *known == reason)
        .map_or("is busy; try again later", |(_, text)| text)
}

/// Replaces `error` by a local explanation when the server refused a busy connection.
pub fn explain_busy(conn: &Connection, error: anyhow::Error) -> anyhow::Error {
    match conn.close_reason() {
        Some(iroh::endpoint::ConnectionError::ApplicationClosed(close))
            if u64::from(close.error_code) == u64::from(BUSY) =>
        {
            anyhow::anyhow!("the server {}", busy_message(&close.reason))
        }
        _ => error,
    }
}

pub fn path_info(conn: &Connection) -> Value {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected());
    let (route, carrier, remote, rtt) = match selected {
        Some(path) => {
            let (route, carrier) = match path.remote_addr() {
                TransportAddr::Relay(_) => ("relay", "WebSocket/TCP"),
                TransportAddr::Ip(_) => ("direct", "UDP"),
                _ => ("unknown", "unknown"),
            };
            (
                route,
                carrier,
                Some(path.remote_addr().to_string()),
                path.rtt().as_secs_f64() * 1000.0,
            )
        }
        None => ("unknown", "unknown", None, 0.0),
    };
    json!({"protocol":"QUIC", "route":route, "carrier":carrier, "remote_addr":remote,
        "quic_rtt_ms":rtt, "paths":paths.iter().map(|p| json!({
            "selected":p.is_selected(), "relay":matches!(p.remote_addr(), TransportAddr::Relay(_)),
            "ip":matches!(p.remote_addr(), TransportAddr::Ip(_)), "remote_addr":p.remote_addr().to_string(),
            "rtt_ms":p.rtt().as_secs_f64()*1000.0
        })).collect::<Vec<_>>()})
}

pub async fn relay_check(settings: &Settings, as_json: bool) -> Result<()> {
    ensure!(
        !settings.direct_only,
        "relay check cannot run with --direct-only"
    );
    let start = Instant::now();
    // Registration is the requested operation, so do not fall back to direct mode here.
    let mut registration = settings.clone();
    registration.relay_only = true;
    let ep = bind(&registration, vec![]).await?;
    let addr = ep.addr();
    let relay = addr
        .relay_urls()
        .next()
        .context("relay registration completed without a relay address")?;
    let result = json!({"relay":relay.as_str(), "status":"online", "mode":if settings.relay.is_some() { "custom" } else { "public" },
        "carrier":"WebSocket/TCP", "registration_ms":start.elapsed().as_secs_f64()*1000.0});
    if as_json {
        out!("{result}");
    } else {
        out!(
            "relay={} status=online carrier=WebSocket/TCP registration={:.3}ms",
            result["relay"].as_str().unwrap(),
            result["registration_ms"].as_f64().unwrap()
        );
    }
    ep.close().await;
    Ok(())
}

pub fn inspect_ticket(value: &str, as_json: bool) -> Result<()> {
    let (addr, blob) = if value.starts_with("blob") {
        let ticket: iroh_blobs::ticket::BlobTicket =
            value.parse().context("invalid blob ticket")?;
        (
            ticket.addr().clone(),
            Some(
                json!({"hash":ticket.hash().to_string(), "format":format!("{:?}",ticket.format())}),
            ),
        )
    } else {
        (
            value
                .parse::<EndpointTicket>()
                .context("invalid endpoint ticket")?
                .endpoint_addr()
                .clone(),
            None,
        )
    };
    let mut result = json!({"endpoint_id":addr.id.to_string(), "relays":addr.relay_urls().map(ToString::to_string).collect::<Vec<_>>(),
        "direct_addresses":addr.ip_addrs().map(ToString::to_string).collect::<Vec<_>>()});
    result["relay"] = addr
        .relay_urls()
        .next()
        .map(|v| json!(v.to_string()))
        .unwrap_or(Value::Null);
    if let Some(blob) = blob {
        result["blob"] = blob;
    }
    if as_json {
        out!("{result}");
    } else {
        out!("endpoint_id: {}", addr.id);
        out!("relay: {}", result["relay"].as_str().unwrap_or("none"));
        out!("direct_addresses:");
        for addr in addr.ip_addrs() {
            out!("  {addr}");
        }
        if let Some(blob) = result.get("blob") {
            out!("blob: {blob}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    /// Marks `count` handshakes past `hello` from `source` and returns how many were evicted.
    async fn evicted_after_hellos(
        pending: &Pending,
        source: impl Fn(usize) -> Option<std::net::IpAddr>,
        count: usize,
    ) -> usize {
        let mut held = Vec::new();
        for n in 0..count {
            let handshaking = pending.admit(source(n));
            with_hello(handshaking.marker(), async { mark_hello() }).await;
            held.push(handshaking);
        }
        let mut evicted = 0;
        for handshaking in &mut held {
            evicted += usize::from(handshaking.evicted.try_recv().is_ok());
        }
        evicted
    }

    #[tokio::test]
    async fn past_hello_handshakes_are_capped_per_source_and_by_half_the_budget() {
        let pending = Pending::new(64);
        let v4 = |_| Some("192.0.2.7".parse().unwrap());
        // 12 hellos from one IPv4 address: only 8 stay.
        assert_eq!(evicted_after_hellos(&pending, v4, 12).await, 4);
        // An IPv6 host cannot multiply itself: one /64 counts as one source.
        let pending = Pending::new(64);
        let v6 = |n: usize| Some(format!("2001:db8:1:2::{n:x}").parse().unwrap());
        assert_eq!(evicted_after_hellos(&pending, v6, 12).await, 4);
        // Another /64 is another source.
        let other = |n: usize| Some(format!("2001:db8:1:{}::1", n % 3 + 10).parse().unwrap());
        let pending = Pending::new(64);
        assert_eq!(evicted_after_hellos(&pending, other, 12).await, 0);
        // Relayed connections have no source: only half of the budget caps them.
        let pending = Pending::new(64);
        assert_eq!(evicted_after_hellos(&pending, |_| None, 40).await, 8);
    }

    /// Entries that never sent `hello` are evicted first, oldest first.
    #[tokio::test]
    async fn silent_entries_make_room_before_any_hello_does() {
        let pending = Pending::new(4);
        let mut silent: Vec<_> = (0..2).map(|_| pending.admit(None)).collect();
        let mut hello: Vec<_> = Vec::new();
        for _ in 0..2 {
            let handshaking = pending.admit(None);
            with_hello(handshaking.marker(), async { mark_hello() }).await;
            hello.push(handshaking);
        }
        let _newcomer = pending.admit(None);
        assert!(silent[0].evicted.try_recv().is_ok());
        assert!(silent[1].evicted.try_recv().is_err());
        assert!(hello.iter_mut().all(|h| h.evicted.try_recv().is_err()));
    }
    use super::*;
    use std::time::Duration;

    fn settings() -> Settings {
        Settings {
            relay: None,
            token: None,
            timeout: Duration::from_millis(100),
            relay_only: false,
            direct_only: false,
            no_hints: true,
        }
    }

    #[test]
    fn public_relays_are_default_custom_is_exclusive_and_direct_disables_relays() {
        let mut config = settings();
        assert!(matches!(relay_mode(&config), RelayMode::Default));
        let public = RelayMode::Default.relay_map().urls::<Vec<_>>();
        assert!(!public.is_empty());
        let peer = iroh::SecretKey::generate().public();
        assert!(
            check_addr(
                &EndpointAddr::new(peer).with_relay_url(public[0].clone()),
                &config
            )
            .is_ok()
        );
        assert_eq!(
            target(&peer.to_string(), &config)
                .unwrap()
                .relay_urls()
                .count(),
            public.len()
        );
        let private: iroh::RelayUrl = "https://private.example".parse().unwrap();
        assert!(
            check_addr(
                &EndpointAddr::new(peer).with_relay_url(private.clone()),
                &config
            )
            .is_err()
        );
        config.relay = Some(private.clone());
        let map = relay_mode(&config).relay_map();
        assert_eq!(map.len(), 1);
        assert!(map.contains(&private));
        assert!(
            check_addr(
                &EndpointAddr::new(peer).with_relay_url(public[0].clone()),
                &config
            )
            .is_err()
        );
        config.direct_only = true;
        assert!(matches!(relay_mode(&config), RelayMode::Disabled));
        let ticket = EndpointAddr::new(peer)
            .with_relay_url(public[0].clone())
            .with_ip_addr("127.0.0.1:12345".parse().unwrap());
        assert!(check_addr(&ticket, &config).is_ok());
        assert!(check_addr(&EndpointAddr::new(peer), &config).is_err());
    }

    #[test]
    fn counters_hold_units_until_released() {
        let counter = Counter::default();
        let first = counter.try_take(2).unwrap();
        let _second = counter.try_take(2).unwrap();
        assert!(counter.try_take(2).is_none());
        drop(first);
        assert_eq!(counter.get(), 1);
        assert!(counter.try_take(2).is_some());
    }

    /// Strangers that open connections and stay silent use up the handshake
    /// budget, but only until the deadline: a client that follows is admitted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_handshakes_do_not_keep_a_paired_client_out() {
        const ALPN: &[u8] = b"iroh-toolbox/test/1";
        let build = |alpns: Vec<Vec<u8>>| {
            Endpoint::builder(presets::Minimal)
                .clear_address_lookup()
                .relay_mode(RelayMode::Disabled)
                .alpns(alpns)
                .bind()
        };
        let server = build(vec![ALPN.to_vec()]).await.unwrap();
        let addr = server.addr();
        let accepting = server.clone();
        let admitted = tokio::spawn(async move {
            accept_first(
                &accepting,
                Duration::from_secs(1),
                "test",
                |conn: Connection| async move {
                    // The handshake: the client opens a stream and writes a byte.
                    let (_send, mut recv) = conn.accept_bi().await?;
                    let mut byte = [0];
                    recv.read_exact(&mut byte).await?;
                    Ok(conn)
                },
            )
            .await
        });
        let idle = build(vec![]).await.unwrap();
        let mut held = Vec::new();
        for _ in 0..PENDING_HANDSHAKES + 20 {
            if let Ok(conn) = idle.connect(addr.clone(), ALPN).await {
                held.push(conn);
            }
        }
        let client = build(vec![]).await.unwrap();
        let started = Instant::now();
        let _open = loop {
            let attempt = async {
                let conn = client.connect(addr.clone(), ALPN).await?;
                let (mut send, _recv) = conn.open_bi().await?;
                send.write_all(&[1]).await?;
                Ok::<_, anyhow::Error>(conn)
            };
            if let Ok(Ok(conn)) = tokio::time::timeout(Duration::from_millis(500), attempt).await {
                break conn;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "never admitted");
        };
        tokio::time::timeout(Duration::from_secs(3), admitted)
            .await
            .expect("accept_first must return")
            .unwrap()
            .unwrap();
        drop(held);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unavailable_public_relays_preserve_direct_access_but_custom_relays_fail() {
        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let relay: iroh::RelayUrl = format!("http://{}", unused.local_addr().unwrap())
            .parse()
            .unwrap();
        drop(unused);
        let mut config = settings();
        let offline = RelayMode::custom([relay.clone()]);
        let ep = bind_with_mode(&config, vec![], offline.clone())
            .await
            .unwrap();
        assert!(ep.addr().ip_addrs().next().is_some());
        ep.close().await;
        config.relay_only = true;
        assert!(
            bind_with_mode(&config, vec![], offline.clone())
                .await
                .is_err()
        );
        config.relay_only = false;
        config.relay = Some(relay);
        assert!(bind_with_mode(&config, vec![], offline).await.is_err());
    }
}
