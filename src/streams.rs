use std::{future::Future, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail, ensure};
use iroh::{
    Endpoint,
    endpoint::{Connection, RecvStream, SendStream},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

use crate::{
    config::Settings,
    identity::check_slot_name,
    network,
    slot::{self, Protocol, Serve},
    target::{self, Destination},
    web,
};

async fn forward<R, W>(
    mut input: R,
    mut output: W,
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    tokio::try_join!(
        async {
            tokio::io::copy(&mut input, &mut send).await?;
            send.finish()?;
            send.stopped().await?;
            Ok::<_, anyhow::Error>(())
        },
        async {
            tokio::io::copy(&mut recv, &mut output).await?;
            output.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        },
    )?;
    Ok(())
}

/// Which side of a pipe this command runs.
pub enum PipeRole<'a> {
    /// Serve a slot of this machine to paired machines, narrowed by `--allow`.
    Serve(Serve<'a>),
    /// Connect to `peer:NAME[/SLOT]`, with the `--addr` values.
    Connect(&'a str, &'a [String]),
}

pub async fn pipe(settings: &Settings, role: PipeRole<'_>, recv_only: bool) -> Result<()> {
    let mut serving = None;
    let (ep, connected) = match role {
        PipeRole::Serve(serve) => {
            let (ep, server) = slot::bind_server(settings, vec![], Protocol::Pipe, serve).await?;
            let server = server.context("a pipe always runs on a slot")?;
            slot::announce(settings, &ep, Some(&server), "pipe connect", "")?;
            serving = Some(server.clone());
            let connected = accept_pipe(&ep, server).await;
            (ep, connected)
        }
        PipeRole::Connect(target, addrs) => {
            let destination = Destination::parse(target, addrs).await?;
            let remote = destination.target.remote(
                "pipe connect",
                Protocol::Pipe,
                &destination.addrs,
                settings,
            )?;
            let ep = target::bind_client(settings, &destination.target).await?;
            let connected = slot::connect_stream(&ep, &remote, Protocol::Pipe, settings)
                .await
                .map(|(conn, handshake)| (conn, handshake.send, handshake.recv));
            (ep, connected)
        }
    };
    let result = async {
        let (conn, send, recv) = connected?;
        // A pipe serves one client: later ones learn why instead of timing out.
        let _busy = serving.map(|server| network::refuse_while_busy(&ep, Some(server)));
        let info = network::path_info(&conn);
        detail!(
            "QUIC route={} carrier={}",
            info["route"].as_str().unwrap(),
            info["carrier"].as_str().unwrap()
        );
        let forwarded = if recv_only {
            forward(tokio::io::empty(), tokio::io::stdout(), send, recv).await
        } else {
            forward(tokio::io::stdin(), tokio::io::stdout(), send, recv).await
        };
        forwarded.map_err(|error| network::explain_busy(&conn, error))
    }
    .await;
    ep.close().await;
    result
}

/// Accepts the first client that completes the slot handshake: it is paired
/// and, with `--allow`, listed. The handshake stream then carries the data.
async fn accept_pipe(
    ep: &Endpoint,
    server: Arc<slot::Server>,
) -> Result<(Connection, SendStream, RecvStream)> {
    network::accept_first(
        ep,
        network::HANDSHAKE_DEADLINE,
        "pipe",
        move |conn: Connection| {
            let server = server.clone();
            async move {
                let handshake = server
                    .authenticate(&conn, network::HANDSHAKE_DEADLINE)
                    .await?;
                Ok((conn, handshake.send, handshake.recv))
            }
        },
    )
    .await
}

pub fn validate_address(value: &str, allow_zero: bool) -> Result<()> {
    let parts = url::Url::parse(&format!("tcp://{value}"))
        .map_err(|_| anyhow::anyhow!("TCP address must be host:port or [IPv6]:port"))?;
    ensure!(
        parts.host_str().is_some()
            && parts.username().is_empty()
            && parts.password().is_none()
            && matches!(parts.path(), "" | "/")
            && parts.query().is_none()
            && parts.fragment().is_none()
            && parts.port().is_some_and(|p| allow_zero || p > 0),
        "TCP address must be host:port or [IPv6]:port"
    );
    Ok(())
}

/// Services one tunnel slot serves at most.
const MAX_SERVICES: usize = 64;
/// Largest tunnel control message, in bytes.
const MESSAGE_LIMIT: usize = 16 * 1024;
/// Client port announced for a sole unnamed `--to HOST:PORT`: such a service
/// usually listens on a well-known port that a client should not bind.
const SINGLE_PORT: u16 = 8080;

/// A TCP service of a tunnel slot as clients see it: never its destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Service {
    name: String,
    /// The local port a client listens on for it by default.
    port: u16,
}

/// A client's request; each stream carries one.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    List {},
    /// Forward the stream to `service`; `None` selects the slot's sole service.
    Connect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<String>,
    },
}

/// The server's answer to one request.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Services {
        services: Vec<Service>,
    },
    /// The destination is connected: raw bytes follow on the stream.
    Connected {},
    Refused {
        reason: Refusal,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Refusal {
    UnknownService,
    ServiceRequired,
    Unreachable,
    Unsupported,
}

async fn write_message(send: &mut SendStream, message: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(bytes.len() <= MESSAGE_LIMIT, "tunnel message too large");
    send.write_u32(bytes.len() as u32).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read_message<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let size = recv.read_u32().await? as usize;
    ensure!(size <= MESSAGE_LIMIT, "tunnel message too large");
    let mut bytes = vec![0; size];
    recv.read_exact(&mut bytes).await?;
    parse_message(&bytes)
}

fn parse_message<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| anyhow!("invalid tunnel message"))
}

/// Checks a service name; an all-digit name is a port, returned.
fn check_service_name(name: &str) -> Result<Option<u16>> {
    check_slot_name(name).context("invalid service name")?;
    if !name.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let port = name
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0 && port.to_string() == name)
        .context("a numeric service name is a port from 1 to 65535")?;
    Ok(Some(port))
}

/// A served service and its destination, known to this machine only.
struct Offered {
    service: Service,
    address: String,
}

/// The services of the `--to` values in their order, and whether a sole
/// unnamed `HOST:PORT` was given.
fn services(values: &[String]) -> Result<(Vec<Offered>, bool)> {
    ensure!(!values.is_empty(), "provide at least one --to");
    ensure!(
        values.len() <= MAX_SERVICES,
        "a slot serves at most {MAX_SERVICES} services"
    );
    let single = values.len() == 1 && !values[0].contains('=');
    let mut offered: Vec<Offered> = Vec::new();
    for value in values {
        let (name, address) = match value.split_once('=') {
            Some((name, address)) => (Some(name), address),
            None => (None, value.as_str()),
        };
        validate_address(address, false)?;
        let destination = url::Url::parse(&format!("tcp://{address}"))?
            .port()
            .context("destination needs a port")?;
        let (name, port) = match name {
            Some(name) => (
                name.to_owned(),
                check_service_name(name)?.unwrap_or(destination),
            ),
            None if single => (destination.to_string(), SINGLE_PORT),
            None => (destination.to_string(), destination),
        };
        ensure!(
            !offered.iter().any(|other| other.service.name == name),
            "duplicate service {name}; give each --to its own SERVICE=HOST:PORT"
        );
        if let Some(other) = offered.iter().find(|other| other.service.port == port) {
            bail!(
                "services {} and {name} both use client port {port}; give one a numeric name such as --to 8001=HOST:PORT",
                other.service.name
            );
        }
        offered.push(Offered {
            service: Service { name, port },
            address: address.to_owned(),
        });
    }
    Ok((offered, single))
}

pub async fn tunnel_listen(
    settings: &Settings,
    to: &[String],
    max_connections: u64,
    serve: Serve<'_>,
) -> Result<()> {
    let (offered, single) = services(to)?;
    let offered = Arc::new(offered);
    let (ep, server) = slot::bind_server(settings, vec![], Protocol::Tunnel, serve).await?;
    let server = server.context("a tunnel always runs on a slot")?;
    // A sole service keeps the client lazy: it may start before this server.
    let hint = if single {
        format!(" --bind 127.0.0.1:{SINGLE_PORT}")
    } else {
        String::new()
    };
    slot::announce(settings, &ep, Some(&server), "tunnel tcp connect", &hint)?;
    for offered in offered.iter() {
        note!(
            "TCP service {} -> {} (client port {})",
            offered.service.name,
            offered.address,
            offered.service.port
        );
    }
    let streams = settings.clone();
    accept_tunnels(
        settings,
        &ep,
        server,
        "tunnel",
        max_connections,
        move |conn| {
            let (offered, settings) = (offered.clone(), streams.clone());
            async move {
                let timeout = settings.timeout;
                serve_streams(&conn, &settings, move |send, recv| {
                    serve_service(offered.clone(), send, recv, timeout)
                })
                .await
            }
        },
    )
    .await
}

/// Answers one request of a tunnel client: the service list, or a stream
/// forwarded to a configured destination.
async fn serve_service(
    offered: Arc<Vec<Offered>>,
    mut send: SendStream,
    mut recv: RecvStream,
    timeout: Duration,
) -> Result<()> {
    let service = match requested(&mut recv, timeout).await? {
        Request::List {} => {
            let services = offered.iter().map(|o| o.service.clone()).collect();
            write_message(&mut send, &Reply::Services { services }).await?;
            send.finish()?;
            return Ok(());
        }
        Request::Connect { service } => service,
    };
    // The requested name comes from the peer: it is compared, never printed.
    let found = match &service {
        None if offered.len() == 1 => Ok(&offered[0]),
        None => Err((
            Refusal::ServiceRequired,
            "a peer named no service; this slot serves several",
        )),
        Some(name) => offered
            .iter()
            .find(|offered| &offered.service.name == name)
            .ok_or((
                Refusal::UnknownService,
                "a peer requested a service this slot does not serve",
            )),
    };
    let offered = match found {
        Ok(offered) => offered,
        Err((reason, message)) => {
            refuse(&mut send, reason).await?;
            bail!(message);
        }
    };
    detail!(
        "tunnel: peer connected; connecting to destination TCP service {}",
        offered.service.name
    );
    let socket = async {
        tokio::time::timeout(timeout, TcpStream::connect(&offered.address))
            .await.context("destination TCP connection timed out; check the service and --to on the serving machine")?
            .context("cannot connect to destination TCP service; start the service and check --to on the serving machine")
    }.await;
    let socket = match socket {
        Ok(socket) => socket,
        Err(error) => {
            refuse(&mut send, Refusal::Unreachable).await?;
            return Err(error);
        }
    };
    detail!(
        "tunnel: destination TCP service connected; forwarding (check the response in your application)"
    );
    write_message(&mut send, &Reply::Connected {}).await?;
    let (read, write) = socket.into_split();
    forward(read, write, send, recv).await
}

/// The request a client sends first on a stream.
async fn requested(recv: &mut RecvStream, timeout: Duration) -> Result<Request> {
    tokio::time::timeout(timeout, read_message(recv))
        .await
        .context("TCP service selection timed out")?
}

/// Refuses one request; the connection stays open for the client's others.
async fn refuse(send: &mut SendStream, reason: Refusal) -> Result<()> {
    write_message(send, &Reply::Refused { reason }).await?;
    send.finish()?;
    Ok(())
}

/// Serves `site` on a slot: each tunnel stream carries HTTP/1.1, answered
/// here without any TCP listener on this machine.
pub async fn http_listen(
    settings: &Settings,
    site: web::Site,
    max_connections: u64,
    serve: Serve<'_>,
) -> Result<()> {
    let (ep, server) = slot::bind_server(settings, vec![], Protocol::Http, serve).await?;
    let server = server.context("a served folder always runs on a slot")?;
    slot::announce(settings, &ep, Some(&server), "tunnel http connect", "")?;
    note!("{}", site.describe());
    let site = Arc::new(site);
    let streams = settings.clone();
    accept_tunnels(
        settings,
        &ep,
        server,
        "http",
        max_connections,
        move |conn| {
            let (site, settings) = (site.clone(), streams.clone());
            async move {
                let timeout = settings.timeout;
                serve_streams(&conn, &settings, move |send, recv| {
                    serve_site(site.clone(), send, recv, timeout)
                })
                .await
            }
        },
    )
    .await
}

/// Answers one stream of a `tunnel http` client: a folder has no services.
async fn serve_site(
    site: Arc<web::Site>,
    mut send: SendStream,
    mut recv: RecvStream,
    timeout: Duration,
) -> Result<()> {
    if requested(&mut recv, timeout).await? != (Request::Connect { service: None }) {
        refuse(&mut send, Refusal::Unsupported).await?;
        bail!("a peer sent a request that a served folder does not support");
    }
    detail!("http: peer connected");
    write_message(&mut send, &Reply::Connected {}).await?;
    web::serve(&site, send, recv, timeout).await
}

/// Serves the streams of an admitted connection, one request each, until the
/// client closes it or leaves it without streams for `--timeout`.
async fn serve_streams<F, Fut>(conn: &Connection, settings: &Settings, serve: F) -> Result<()>
where
    F: Fn(SendStream, RecvStream) -> Fut,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let mut tasks = JoinSet::new();
    loop {
        let idle = tasks.is_empty();
        let timeout = settings.timeout;
        let idle = async move {
            if idle {
                tokio::time::sleep(timeout).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            accepted = conn.accept_bi() => match accepted {
                Ok((send, recv)) => {
                    tasks.spawn(serve(send, recv));
                }
                // The client closed the connection.
                Err(_) => break,
            },
            () = idle => {
                conn.close(0u32.into(), b"idle");
                break;
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => report_task(result, settings),
        }
    }
    while let Some(result) = tasks.join_next().await {
        report_task(result, settings);
    }
    Ok(())
}

/// Accepts the paired clients of a tunnel slot until the endpoint closes, and
/// runs `serve` on each admitted connection.
async fn accept_tunnels<F, Fut>(
    settings: &Settings,
    ep: &Endpoint,
    server: Arc<slot::Server>,
    label: &'static str,
    max_connections: u64,
    serve: F,
) -> Result<()>
where
    F: Fn(Connection) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let mut tasks = JoinSet::new();
    // Strangers' handshakes and authenticated connections have separate budgets.
    let (pending, active) = (network::Pending::default(), network::Counter::default());
    loop {
        tokio::select! {
            incoming = ep.accept() => {
                let incoming = incoming.context("endpoint closed")?;
                // A full budget retries unvalidated sources and evicts the oldest
                // handshake that has not sent its hello.
                let Some((incoming, mut handshaking)) = pending.screen(incoming) else {
                    continue;
                };
                let server = server.clone();
                let active = active.clone();
                let serve = serve.clone();
                tasks.spawn(async move {
                    let Some(conn) = slot::accept(Some(&server), incoming, label, &mut handshaking).await else {
                        return Ok(());
                    };
                    drop(handshaking);
                    // Only authenticated clients count against --max-connections.
                    let full = network::at_capacity(active.get(), max_connections);
                    let Some(_serving) = (!full).then(|| active.try_take(max_connections)).flatten() else {
                        conn.close(network::BUSY.into(), b"the server is at its connection limit");
                        return Ok(());
                    };
                    serve(conn).await
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => report_task(result, settings),
        }
    }
}

/// What `--bind` asks of `tunnel tcp connect`.
#[derive(Debug, PartialEq, Eq)]
enum Binds {
    /// One `HOST:PORT` listener: the client starts without listing services.
    Single(String),
    /// `SERVICE=HOST:PORT` overrides, possibly none, of the listed services.
    Services(Vec<(String, String)>),
}

fn binds(values: &[String]) -> Result<Binds> {
    let mut single = None;
    let mut services: Vec<(String, String)> = Vec::new();
    for value in values {
        match value.split_once('=') {
            Some((left, right)) => {
                if validate_address(left, true).is_ok() && right.parse::<u16>().is_ok() {
                    bail!("--bind {value} uses the old order; write --bind {right}={left}");
                }
                check_service_name(left)?;
                validate_address(right, true)?;
                ensure!(
                    !services.iter().any(|(name, _)| name == left),
                    "--bind {left}=… is given twice"
                );
                services.push((left.to_owned(), right.to_owned()));
            }
            None => {
                validate_address(value, true)?;
                ensure!(
                    single.is_none(),
                    "give --bind HOST:PORT once, or repeat --bind SERVICE=HOST:PORT for several services"
                );
                single = Some(value.clone());
            }
        }
    }
    match single {
        Some(address) => {
            ensure!(
                services.is_empty(),
                "--bind HOST:PORT cannot be combined with --bind SERVICE=HOST:PORT"
            );
            Ok(Binds::Single(address))
        }
        None => Ok(Binds::Services(services)),
    }
}

pub async fn tunnel_connect(
    settings: &Settings,
    target: &str,
    bind: &[String],
    addrs: &[String],
) -> Result<()> {
    let binds = binds(bind)?;
    let (destination, service) = Destination::parse_service(target, addrs).await?;
    if let (Some(_), Binds::Services(overrides)) = (&service, &binds) {
        ensure!(
            overrides.is_empty(),
            "the target already names a service; use --bind HOST:PORT for its listener"
        );
    }
    let mut client = Client::open(
        settings,
        Protocol::Tunnel,
        "tunnel tcp connect",
        &destination,
    )
    .await?;
    let lazy = match binds {
        Binds::Single(address) => {
            client.listen(&address, service).await.context(
                "cannot bind TCP listener; choose a free local port with --bind 127.0.0.1:8081",
            )?;
            true
        }
        Binds::Services(overrides) => {
            let services = client.list(settings).await?;
            for (address, name) in chosen(
                (&client.remote.peer, &client.remote.slot),
                &services,
                service.as_deref(),
                &overrides,
            )? {
                client.listen(&address, Some(name.to_owned())).await.with_context(|| {
                    format!("cannot bind {address} for service {name}; pass --bind {name}=127.0.0.1:PORT with a free port")
                })?;
            }
            false
        }
    };
    for (listener, service) in &client.listeners {
        match service {
            Some(service) => note!(
                "TCP listening on {} -> service {service}",
                listener.local_addr()?
            ),
            None => note!("TCP listening on {}", listener.local_addr()?),
        }
    }
    if lazy {
        note!(
            "tunnel: local listener ready; peer and destination service unchecked until a local connection; open your application on this port"
        );
    } else {
        let count = client.listeners.len();
        note!(
            "tunnel: {count} service{} of {}/{} ready; destinations are checked at each local connection",
            if count == 1 { "" } else { "s" },
            client.remote.peer,
            client.remote.slot
        );
    }
    client.run(settings).await
}

/// The listeners for the listed `services`, as `(address, service)`: the one
/// the target names, or the whole group with the `--bind SERVICE=` overrides.
fn chosen<'a>(
    (peer, slot): (&str, &str),
    services: &'a [Service],
    service: Option<&str>,
    overrides: &[(String, String)],
) -> Result<Vec<(String, &'a str)>> {
    let missing = |name: &str| {
        anyhow!(
            "slot {slot} of {peer} has no service \"{name}\"; available: {}",
            names(services)
        )
    };
    let default = |service: &Service| format!("127.0.0.1:{}", service.port);
    if let Some(name) = service {
        let service = services
            .iter()
            .find(|service| service.name == name)
            .ok_or_else(|| missing(name))?;
        return Ok(vec![(default(service), &service.name)]);
    }
    if let Some((name, _)) = overrides
        .iter()
        .find(|(name, _)| !services.iter().any(|service| &service.name == name))
    {
        return Err(missing(name));
    }
    Ok(services
        .iter()
        .map(|service| {
            let address = overrides
                .iter()
                .find(|(name, _)| *name == service.name)
                .map_or_else(|| default(service), |(_, address)| address.clone());
            (address, service.name.as_str())
        })
        .collect())
}

fn names(services: &[Service]) -> String {
    services
        .iter()
        .map(|service| service.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Opens a paired machine's served folder on a local port for the browser.
pub async fn http_connect(
    settings: &Settings,
    target: &str,
    bind: &str,
    addrs: &[String],
) -> Result<()> {
    validate_address(bind, true)?;
    let destination = Destination::parse(target, addrs).await?;
    let mut client = Client::open(
        settings,
        Protocol::Http,
        "tunnel http connect",
        &destination,
    )
    .await?;
    client
        .listen(bind, None)
        .await
        .context("cannot bind TCP listener; choose a free local port with --bind 127.0.0.1:8081")?;
    let local = client.listeners[0].0.local_addr()?;
    note!(
        "Open {} ({} is contacted when the page loads)",
        web::url(local),
        client.remote.peer
    );
    client.run(settings).await
}

/// The local listeners of a tunnel client, all bound before readiness: an
/// error drops the ones already bound.
struct Client {
    ep: Endpoint,
    remote: Arc<slot::Remote>,
    protocol: Protocol,
    /// Each listener with the service it asks for; `None` asks for the sole one.
    listeners: Vec<(TcpListener, Option<String>)>,
}

impl Client {
    async fn open(
        settings: &Settings,
        protocol: Protocol,
        command: &str,
        destination: &Destination,
    ) -> Result<Self> {
        let remote = Arc::new(destination.target.remote(
            command,
            protocol,
            &destination.addrs,
            settings,
        )?);
        let ep = target::bind_client(settings, &destination.target).await?;
        Ok(Self {
            ep,
            remote,
            protocol,
            listeners: Vec::new(),
        })
    }

    async fn listen(&mut self, address: &str, service: Option<String>) -> Result<()> {
        self.listeners
            .push((TcpListener::bind(address).await?, service));
        Ok(())
    }

    /// The services of the slot, on a connection of their own.
    async fn list(&self, settings: &Settings) -> Result<Vec<Service>> {
        let (peer, slot) = (&self.remote.peer, &self.remote.slot);
        let conn = slot::connect(&self.ep, &self.remote, self.protocol, settings)
            .await
            .map_err(|error| {
                if !error.is::<slot::Unavailable>() {
                    return error;
                }
                // An unknown slot cannot be told apart from a stopped server.
                let default = Protocol::Tunnel.default_slot();
                let other = if slot == default {
                    String::new()
                } else {
                    format!("; if {slot} is a service of the default slot, use peer:{peer}/{default}/{slot}")
                };
                anyhow!(
                    "{error}; to start before the server, pass peer:{peer}/{slot}/SERVICE --bind HOST:PORT{other}"
                )
            })?;
        let services = request_list(&conn, (peer, slot), settings.timeout).await;
        conn.close(0u32.into(), b"listed");
        services
    }

    async fn run(self, settings: &Settings) -> Result<()> {
        let mut tasks = JoinSet::new();
        for (listener, service) in self.listeners {
            tasks.spawn(tunnel_connections(
                self.ep.clone(),
                self.remote.clone(),
                self.protocol,
                listener,
                service,
                settings.clone(),
            ));
        }
        while let Some(result) = tasks.join_next().await {
            result??;
        }
        Ok(())
    }
}

/// Sends one request on a new stream of `conn` and reads the reply.
async fn request(
    conn: &Connection,
    request: &Request,
    timeout: Duration,
) -> Result<(SendStream, RecvStream, Reply)> {
    tokio::time::timeout(timeout, async {
        let (mut send, mut recv) = conn.open_bi().await?;
        write_message(&mut send, request).await?;
        let reply = read_message(&mut recv).await?;
        Ok((send, recv, reply))
    })
    .await
    .context("TCP service selection timed out")?
}

/// The services a slot lists, checked before any of them is printed.
async fn request_list(
    conn: &Connection,
    (peer, slot): (&str, &str),
    timeout: Duration,
) -> Result<Vec<Service>> {
    let (_, _, reply) = request(conn, &Request::List {}, timeout).await?;
    let invalid = || {
        anyhow!(
            "{peer} sent an invalid service list for slot {slot}; check that both machines run compatible versions"
        )
    };
    let Reply::Services { services } = reply else {
        return Err(invalid());
    };
    let valid = (1..=MAX_SERVICES).contains(&services.len())
        && services.iter().enumerate().all(|(n, service)| {
            check_service_name(&service.name).is_ok()
                && service.port > 0
                && services[..n]
                    .iter()
                    .all(|other| other.name != service.name && other.port != service.port)
        });
    ensure!(valid, invalid());
    Ok(services)
}

/// Asks the server to forward a new stream of `conn` to `service`.
async fn open_service(
    conn: &Connection,
    (peer, slot): (&str, &str),
    service: Option<&str>,
    timeout: Duration,
) -> Result<(SendStream, RecvStream)> {
    let connect = Request::Connect {
        service: service.map(str::to_owned),
    };
    let (send, recv, reply) = request(conn, &connect, timeout).await?;
    let reason = match reply {
        Reply::Connected {} => return Ok((send, recv)),
        Reply::Refused { reason } => reason,
        Reply::Services { .. } => bail!(
            "{peer} sent an unexpected reply; check that both machines run compatible versions"
        ),
    };
    Err(match (reason, service) {
        (Refusal::Unreachable, _) => anyhow!(
            "{peer}: destination TCP service unavailable; start the service and check --to on the serving machine"
        ),
        (Refusal::UnknownService, Some(name)) => {
            // The same connection lists the services for the message.
            let available = request_list(conn, (peer, slot), timeout).await?;
            anyhow!(
                "slot {slot} of {peer} has no service \"{name}\"; available: {}",
                names(&available)
            )
        }
        (Refusal::ServiceRequired, _) => {
            let available = request_list(conn, (peer, slot), timeout).await?;
            anyhow!(
                "{peer}/{slot} serves several services ({}); name one with peer:{peer}/{slot}/SERVICE or --bind SERVICE=HOST:PORT",
                names(&available)
            )
        }
        _ => anyhow!(
            "{peer} (slot {slot}) refused this request; check that both machines run compatible versions"
        ),
    })
}

async fn tunnel_connections(
    ep: Endpoint,
    remote: Arc<slot::Remote>,
    protocol: Protocol,
    listener: TcpListener,
    service: Option<String>,
    settings: Settings,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let service = Arc::new(service);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                let ep: Endpoint = ep.clone();
                let remote = remote.clone();
                let service = service.clone();
                let settings = settings.clone();
                tasks.spawn(async move {
                    detail!("tunnel: local connection accepted; connecting to peer");
                    let conn = slot::connect(&ep, &remote, protocol, &settings).await?;
                    let (send, recv) =
                        open_service(&conn, (&remote.peer, &remote.slot), service.as_deref(), settings.timeout).await?;
                    detail!("tunnel: peer connected; destination service is being requested; confirm access with your application's response");
                    let (read, write) = socket.into_split();
                    forward(read, write, send, recv).await
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => report_task(result, &settings),
        }
    }
}

fn report_task(result: Result<Result<()>, tokio::task::JoinError>, settings: &Settings) {
    match result {
        Ok(Err(error)) => note!("tunnel error: {}", settings.redact(format!("{error:#}"))),
        Err(_) => note!("tunnel task failed"),
        _ => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn announced(values: &[&str]) -> Vec<(String, u16, String)> {
        let (offered, _) = services(&strings(values)).unwrap();
        offered
            .into_iter()
            .map(|o| (o.service.name, o.service.port, o.address))
            .collect()
    }

    #[test]
    fn services_take_names_ports_and_the_single_default() {
        let (_, single) = services(&strings(&["127.0.0.1:3000"])).unwrap();
        assert!(single);
        assert_eq!(
            announced(&["127.0.0.1:3000"]),
            [("3000".into(), 8080, "127.0.0.1:3000".into())]
        );
        assert_eq!(
            announced(&["web=127.0.0.1:3000"]),
            [("web".into(), 3000, "127.0.0.1:3000".into())]
        );
        assert_eq!(
            announced(&["web=127.0.0.1:3000", "[::1]:5432", "8001=192.168.1.20:80"]),
            [
                ("web".into(), 3000, "127.0.0.1:3000".into()),
                ("5432".into(), 5432, "[::1]:5432".into()),
                ("8001".into(), 8001, "192.168.1.20:80".into()),
            ]
        );
        for values in [
            vec![],
            vec!["0=127.0.0.1:3000"],
            vec!["080=127.0.0.1:3000"],
            vec!["65536=127.0.0.1:3000"],
            vec!["Web=127.0.0.1:3000"],
            vec!["all=127.0.0.1:3000"],
            vec!["web=127.0.0.1"],
            vec!["web=127.0.0.1:3000", "web=127.0.0.1:4000"],
            vec!["127.0.0.1:3000", "3000=localhost:5000"],
            vec!["a=127.0.0.1:80", "b=127.0.0.2:80"],
        ] {
            assert!(services(&strings(&values)).is_err(), "{values:?}");
        }
        let many: Vec<String> = (1..=65).map(|n| format!("127.0.0.1:{n}")).collect();
        assert!(services(&many).is_err());
        assert!(services(&many[..64]).is_ok());
    }

    #[test]
    fn binds_select_one_listener_or_override_services() {
        assert_eq!(binds(&[]).unwrap(), Binds::Services(vec![]));
        assert_eq!(
            binds(&strings(&["127.0.0.1:0"])).unwrap(),
            Binds::Single("127.0.0.1:0".into())
        );
        assert_eq!(
            binds(&strings(&["db=127.0.0.1:15432", "3000=[::1]:0"])).unwrap(),
            Binds::Services(vec![
                ("db".into(), "127.0.0.1:15432".into()),
                ("3000".into(), "[::1]:0".into())
            ])
        );
        let error = binds(&strings(&["127.0.0.1:8080=3000"]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("write --bind 3000=127.0.0.1:8080"),
            "{error}"
        );
        for values in [
            vec!["127.0.0.1:0", "127.0.0.1:1"],
            vec!["127.0.0.1:0", "db=127.0.0.1:1"],
            vec!["db=127.0.0.1:0", "db=127.0.0.1:1"],
            vec!["Db=127.0.0.1:0"],
            vec!["db=127.0.0.1"],
        ] {
            assert!(binds(&strings(&values)).is_err(), "{values:?}");
        }
    }

    #[test]
    fn group_listeners_follow_the_list_and_the_overrides() {
        let remote = ("nas", "dev");
        let services = [
            Service {
                name: "web".into(),
                port: 3000,
            },
            Service {
                name: "db".into(),
                port: 5432,
            },
        ];
        assert_eq!(
            chosen(remote, &services, None, &[]).unwrap(),
            [
                ("127.0.0.1:3000".into(), "web"),
                ("127.0.0.1:5432".into(), "db")
            ]
        );
        assert_eq!(
            chosen(
                remote,
                &services,
                None,
                &[("db".into(), "127.0.0.1:15432".into())]
            )
            .unwrap(),
            [
                ("127.0.0.1:3000".into(), "web"),
                ("127.0.0.1:15432".into(), "db")
            ]
        );
        assert_eq!(
            chosen(remote, &services, Some("db"), &[]).unwrap(),
            [("127.0.0.1:5432".into(), "db")]
        );
        for error in [
            chosen(remote, &services, Some("cache"), &[]).unwrap_err(),
            chosen(
                remote,
                &services,
                None,
                &[("cache".into(), "127.0.0.1:0".into())],
            )
            .unwrap_err(),
        ] {
            assert_eq!(
                error.to_string(),
                "slot dev of nas has no service \"cache\"; available: web, db"
            );
        }
    }

    #[test]
    fn tunnel_messages_are_closed_tagged_json() {
        let cases = [
            (r#"{"type":"list"}"#, Request::List {}),
            (
                r#"{"type":"connect","service":"web"}"#,
                Request::Connect {
                    service: Some("web".into()),
                },
            ),
            (r#"{"type":"connect"}"#, Request::Connect { service: None }),
        ];
        for (text, request) in cases {
            assert_eq!(serde_json::to_string(&request).unwrap(), text);
            assert_eq!(serde_json::from_str::<Request>(text).unwrap(), request);
        }
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"type":"connect","service":null}"#).unwrap(),
            Request::Connect { service: None }
        );
        for bad in [
            r#"{"type":"list","x":1}"#,
            r#"{"type":"connect","port":3000}"#,
            r#"{"type":"open"}"#,
            r#"{}"#,
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
        let replies = [
            (r#"{"type":"connected"}"#, Reply::Connected {}),
            (
                r#"{"type":"refused","reason":"service_required"}"#,
                Reply::Refused {
                    reason: Refusal::ServiceRequired,
                },
            ),
            (
                r#"{"type":"services","services":[{"name":"web","port":3000}]}"#,
                Reply::Services {
                    services: vec![Service {
                        name: "web".into(),
                        port: 3000,
                    }],
                },
            ),
        ];
        for (text, reply) in replies {
            assert_eq!(serde_json::to_string(&reply).unwrap(), text);
            assert_eq!(serde_json::from_str::<Reply>(text).unwrap(), reply);
        }
        for bad in [
            r#"{"type":"connected","x":1}"#,
            r#"{"type":"refused","reason":"busy"}"#,
            r#"{"type":"services","services":[{"name":"web","port":3000,"host":"10.0.0.1"}]}"#,
        ] {
            assert!(serde_json::from_str::<Reply>(bad).is_err(), "{bad}");
        }
    }

    /// A local TCP service that answers each connection with `label` and
    /// what it received, until EOF.
    async fn labelled_echo(label: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    socket.read_to_end(&mut bytes).await.unwrap();
                    socket.write_all(label).await.unwrap();
                    socket.write_all(&bytes).await.unwrap();
                });
            }
        });
        address
    }

    async fn exchange(conn: &Connection, service: Option<&str>, payload: &[u8]) -> Result<Vec<u8>> {
        let (mut send, mut recv) =
            open_service(conn, ("nas", "dev"), service, Duration::from_secs(5)).await?;
        send.write_all(payload).await?;
        send.finish()?;
        Ok(recv.read_to_end(1 << 20).await?)
    }

    #[test]
    fn one_connection_carries_several_requests() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            use iroh::{RelayMode, endpoint::presets};
            const ALPN: &[u8] = b"iroh-toolbox/test/tunnel";
            let endpoint = |alpns: Vec<Vec<u8>>| async move {
                Endpoint::builder(presets::Minimal)
                    .clear_address_lookup()
                    .relay_mode(RelayMode::Disabled)
                    .alpns(alpns)
                    .bind()
                    .await
                    .unwrap()
            };
            let settings = Settings {
                relay: None,
                token: None,
                timeout: Duration::from_secs(5),
                relay_only: false,
                direct_only: true,
                no_hints: true,
            };
            let to = vec![
                format!("web={}", labelled_echo(b"web:").await),
                format!("db={}", labelled_echo(b"db:").await),
            ];
            let offered = Arc::new(services(&to).unwrap().0);
            let server = endpoint(vec![ALPN.to_vec()]).await;
            let client = endpoint(vec![]).await;
            let target = server.addr();
            let serving = tokio::spawn(async move {
                let conn = server.accept().await.unwrap().await.unwrap();
                serve_streams(&conn, &settings, move |send, recv| {
                    serve_service(offered.clone(), send, recv, Duration::from_secs(5))
                })
                .await
                .unwrap();
                server
            });
            let conn = client.connect(target, ALPN).await.unwrap();
            let listed = request_list(&conn, ("nas", "dev"), Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(names(&listed), "web, db");
            // A refusal leaves the connection usable for the next requests.
            let error = exchange(&conn, None, b"x").await.unwrap_err().to_string();
            assert_eq!(
                error,
                "nas/dev serves several services (web, db); name one with peer:nas/dev/SERVICE or --bind SERVICE=HOST:PORT"
            );
            let error = exchange(&conn, Some("cache"), b"x").await.unwrap_err().to_string();
            assert_eq!(error, "slot dev of nas has no service \"cache\"; available: web, db");
            let (web, db) = tokio::join!(
                exchange(&conn, Some("web"), b"one"),
                exchange(&conn, Some("db"), b"two")
            );
            assert_eq!(web.unwrap(), b"web:one");
            assert_eq!(db.unwrap(), b"db:two");
            conn.close(0u32.into(), b"done");
            serving.await.unwrap().close().await;
            client.close().await;
        });
    }

    #[test]
    fn tunnel_message_parsers_survive_mutated_and_random_input() {
        crate::fuzz::check_decoder(
            &[
                r#"{"type":"list"}"#,
                r#"{"type":"connect"}"#,
                r#"{"type":"connect","service":"web"}"#,
            ],
            &[("list", &[]), ("connect", &["service"])],
            parse_message::<Request>,
        );
        crate::fuzz::check_decoder(
            &[
                r#"{"type":"services","services":[{"name":"web","port":80}]}"#,
                r#"{"type":"services","services":[]}"#,
                r#"{"type":"connected"}"#,
                r#"{"type":"refused","reason":"unreachable"}"#,
            ],
            &[
                ("services", &["services"]),
                ("connected", &[]),
                ("refused", &["reason"]),
            ],
            parse_message::<Reply>,
        );
    }
}
