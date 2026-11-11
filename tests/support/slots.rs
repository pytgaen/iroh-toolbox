//! Slots between paired machines: peer authentication and `--allow` filters,
//! direct connections without a relay, and (ignored) the same through a local
//! official relay.
//!
//! Machines are paired by writing both peer books from the test, the way
//! `pair` would have left them: no test-only backdoor in the binary.
use super::*;

/// A machine with its own identity directory and peer book.
pub(super) struct Machine {
    home: tempfile::TempDir,
    pub(super) name: String,
    key: String,
    salt: String,
}

impl Machine {
    pub(super) fn new(name: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        for args in [vec!["id"], vec!["id", "rename", name]] {
            let output = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"))
                .args(&args)
                .env("IROH_TOOLBOX_HOME", home.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "id failed");
        }
        // The machine key and salt come from the identity file, never printed.
        let file = std::fs::read_to_string(home.path().join("identity")).unwrap();
        let identity: toml::Table = toml::from_str(&file).unwrap();
        let seed: [u8; 32] = hex::decode(identity["machine_seed"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        Self {
            key: iroh::SecretKey::from_bytes(&seed).public().to_z32(),
            salt: identity["salt"].as_str().unwrap().to_owned(),
            name: name.to_owned(),
            home,
        }
    }

    /// Another machine holding a copy of this one's identity file and peer book.
    pub(super) fn copy(&self) -> Self {
        let home = tempfile::tempdir().unwrap();
        for file in ["identity", "name", "peers.toml"] {
            std::fs::copy(self.home.path().join(file), home.path().join(file)).unwrap();
        }
        Self {
            home,
            name: self.name.clone(),
            key: self.key.clone(),
            salt: self.salt.clone(),
        }
    }

    /// Adds `other` to this machine's peer book as `local_name`.
    pub(super) fn pair_with(&self, local_name: &str, other: &Machine) {
        let path = self.home.path().join("peers.toml");
        let mut book = std::fs::read_to_string(&path).unwrap_or_default();
        book += &format!(
            "[peers.{local_name}]\nmachine = {:?}\nsalt = {:?}\nannounced = {:?}\npaired = \"2026-11-01T10:12:00Z\"\n",
            other.key, other.salt, other.name
        );
        std::fs::write(&path, book).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    /// Runs the CLI on this machine.
    pub(super) fn spawn(&self, globals: &[String], args: &[&str]) -> Peer {
        self.spawn_with(globals, args, &[])
    }

    /// Runs the CLI on this machine with extra environment variables.
    pub(super) fn spawn_with(
        &self,
        globals: &[String],
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Peer {
        let mut command = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"));
        command
            .args(globals)
            .args(args)
            .env("IROH_TOOLBOX_HOME", self.home.path())
            .envs(env.iter().copied());
        Peer::start(command)
    }

    /// Runs a local command (`peers …`, `id …`) to completion and returns its stderr.
    pub(super) fn run(&self, args: &[&str]) -> String {
        let mut peer = self.spawn(&[], args);
        peer.wait(true);
        peer.errors()
    }
}

/// A free UDP port on loopback, for `--port`.
pub(super) fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A running server and the UDP port it was told to use, if any.
pub(super) struct Served {
    peer: Peer,
    pub(super) port: Option<u16>,
}

impl std::ops::Deref for Served {
    type Target = Peer;
    fn deref(&self) -> &Peer {
        &self.peer
    }
}

impl std::ops::DerefMut for Served {
    fn deref_mut(&mut self) -> &mut Peer {
        &mut self.peer
    }
}

/// `nas` serves, `laptop` connects. Direct pairs run without any relay
/// (`--direct-only --port` on the server, `--addr` on the client); relayed
/// pairs carry the given global options.
pub(super) struct Pair {
    pub(super) nas: Machine,
    pub(super) laptop: Machine,
    pub(super) globals: Vec<String>,
    direct: bool,
}

impl Pair {
    /// Paired without a relay.
    pub(super) fn direct() -> Self {
        Self::new(vec!["--direct-only".into()], true)
    }

    /// Paired, reaching each other through the relay options in `globals`.
    pub(super) fn relayed(globals: Vec<String>) -> Self {
        Self::new(globals, false)
    }

    fn new(globals: Vec<String>, direct: bool) -> Self {
        let (nas, laptop) = (Machine::new("nas"), Machine::new("laptop"));
        nas.pair_with("laptop", &laptop);
        laptop.pair_with("nas", &nas);
        Self {
            nas,
            laptop,
            globals,
            direct,
        }
    }

    /// Starts `nas`'s server once it announced itself.
    pub(super) fn serve(&self, args: &[&str]) -> Served {
        self.serve_from(&self.nas, args)
    }

    pub(super) fn serve_from(&self, machine: &Machine, args: &[&str]) -> Served {
        self.serve_with(machine, args, &[])
    }

    /// Starts a server on `machine` with extra environment variables.
    pub(super) fn serve_with(
        &self,
        machine: &Machine,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Served {
        let port = self.direct.then(free_port);
        let mut all: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        if let Some(port) = port {
            all.extend(["--port".to_owned(), port.to_string()]);
        }
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        let peer = machine.spawn_with(&self.globals, &all, env);
        peer.until_error_line(if self.direct {
            "Direct addresses:"
        } else {
            "Serving "
        });
        Served { peer, port }
    }

    /// Runs a client command as `laptop`.
    pub(super) fn connect(&self, served: &Served, args: &[&str]) -> Peer {
        self.connect_as(&self.laptop, served, args)
    }

    pub(super) fn connect_as(&self, machine: &Machine, served: &Served, args: &[&str]) -> Peer {
        let mut all: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        if let Some(port) = served.port {
            all.extend(["--addr".to_owned(), format!("127.0.0.1:{port}")]);
        }
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        machine.spawn(&self.globals, &all)
    }
}

/// Waits until `peer`'s stderr contains `text`, then returns all of it.
pub(super) fn errors_with(peer: &Peer, text: &str) -> String {
    errors_within(peer, text, Duration::from_secs(15))
}

/// Like [`errors_with`], waiting up to `limit`.
pub(super) fn errors_within(peer: &Peer, text: &str, limit: Duration) -> String {
    let start = Instant::now();
    loop {
        let errors = peer.errors();
        if errors.contains(text) {
            return errors;
        }
        assert!(start.elapsed() < limit, "missing {text}: {errors}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A TCP service that echoes each connection until EOF.
fn echo_service() -> String {
    let service = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = service.local_addr().unwrap().to_string();
    thread::spawn(move || {
        for conn in service.incoming() {
            let mut conn = conn.unwrap();
            thread::spawn(move || {
                let mut bytes = [0u8; 64];
                while let Ok(n) = conn.read(&mut bytes) {
                    if n == 0 || conn.write_all(&bytes[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// Retries until a connection through the local tunnel port echoes, as the
/// slot may still be registering at the relay.
fn echoes_within(bound: &str, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Ok(mut conn) = TcpStream::connect(bound) {
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reply = [0u8; 4];
            if conn.write_all(b"ping").is_ok() && conn.read_exact(&mut reply).is_ok() {
                return &reply == b"ping";
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    false
}

/// Starts a tunnel client and returns it with its local address.
fn tunnel_client(pair: &Pair, who: &Machine, served: &Served, remote: &str) -> (Peer, String) {
    let client = pair.connect_as(
        who,
        served,
        &["tunnel", "tcp", "connect", remote, "--bind", "127.0.0.1:0"],
    );
    let bound = client.line(true);
    let bound = bound.strip_prefix("TCP listening on ").unwrap().to_owned();
    (client, bound)
}

/// The error a tunnel client reports when its first connection is refused.
fn refusal(client: &Peer, bound: &str) -> String {
    let mut conn = TcpStream::connect(bound).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let _ = conn.write_all(b"ping");
    let mut reply = Vec::new();
    let _ = conn.read_to_end(&mut reply);
    assert!(reply.is_empty(), "a refused client must receive nothing");
    client.until_error_line("tunnel error:")
}

/// One HTTP/1.1 request through the local port of `tunnel http connect`,
/// retried while the slot comes up: the response head, lowercased, and body.
fn http(bound: &str, method: &str, path: &str, headers: &str) -> (String, String) {
    http_as(bound, bound, method, path, headers)
}

/// [`http`] with the `Host` header a browser sends for `host`.
fn http_as(bound: &str, host: &str, method: &str, path: &str, headers: &str) -> (String, String) {
    let start = Instant::now();
    loop {
        if let Ok(mut conn) = TcpStream::connect(bound) {
            conn.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{headers}\r\n"
            );
            let mut reply = Vec::new();
            if conn.write_all(request.as_bytes()).is_ok()
                && conn.read_to_end(&mut reply).is_ok()
                && !reply.is_empty()
            {
                let reply = String::from_utf8_lossy(&reply).into_owned();
                let (head, body) = reply.split_once("\r\n\r\n").unwrap();
                return (head.to_lowercase(), body.to_owned());
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "no HTTP answer to {method} {path}"
        );
        thread::sleep(Duration::from_millis(250));
    }
}

/// Starts `tunnel http connect` and returns it with its local address.
fn http_client(pair: &Pair, served: &Served, remote: &str) -> (Peer, String) {
    let client = pair.connect(
        served,
        &["tunnel", "http", "connect", remote, "--bind", "127.0.0.1:0"],
    );
    let line = client.line(true);
    let bound = line
        .strip_prefix("Open http://")
        .and_then(|rest| rest.split_once('/'))
        .unwrap_or_else(|| panic!("{line}"))
        .0
        .to_owned();
    (client, bound)
}

/// A served folder answers a paired machine's browser: listing, files, ranges
/// and redirects, with dotfiles hidden and pages without scripts. `--site`
/// opens index.html and 404.html instead, and lets pages run their scripts.
#[test]
fn tunnel_http_serves_a_folder_or_a_site_to_a_paired_browser() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("hello.txt"), "hello from A").unwrap();
    std::fs::write(folder.path().join("index.html"), "<p>home</p>").unwrap();
    std::fs::write(folder.path().join("404.html"), "lost").unwrap();
    std::fs::write(folder.path().join(".env"), "secret").unwrap();
    std::fs::create_dir(folder.path().join("sub")).unwrap();
    let path = folder.path().to_str().unwrap();
    let pair = Pair::direct();

    let server = pair.serve(&["tunnel", "http", "serve", "--allow", "all", path]);
    let errors = errors_with(&server, "Files: ");
    assert!(
        errors.contains("Serving to: all paired machines (now: laptop)"),
        "{errors}"
    );
    assert!(
        errors.contains("tunnel http connect peer:nas --addr "),
        "{errors}"
    );
    let (client, bound) = http_client(&pair, &server, "peer:nas");
    let (head, body) = http(&bound, "GET", "/", "");
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-security-policy: default-src 'none'"));
    for link in ["./hello.txt\"", "./index.html\"", "./sub/\""] {
        assert!(body.contains(link), "{link}: {body}");
    }
    assert!(!body.contains(".env"), "{body}");
    let (head, body) = http(&bound, "GET", "/hello.txt", "");
    assert!(
        head.contains("content-type: text/plain; charset=utf-8"),
        "{head}"
    );
    assert!(!head.contains("content-security-policy"), "{head}");
    assert_eq!(body, "hello from A");
    let (head, body) = http(&bound, "GET", "/hello.txt", "Range: bytes=0-4\r\n");
    assert!(head.starts_with("http/1.1 206"), "{head}");
    assert!(head.contains("content-range: bytes 0-4/12"), "{head}");
    assert_eq!(body, "hello");
    let (head, body) = http(&bound, "HEAD", "/hello.txt", "");
    assert!(head.starts_with("http/1.1 200") && head.contains("content-length: 12"));
    assert!(body.is_empty());
    let (head, body) = http(&bound, "GET", "/index.html", "");
    assert!(head.contains("content-security-policy: sandbox"), "{head}");
    assert_eq!(body, "<p>home</p>");
    let (head, _) = http(&bound, "GET", "/sub", "");
    assert!(head.starts_with("http/1.1 301") && head.contains("location: /sub/"));
    for (method, path, status) in [
        ("GET", "/.env", "404"),
        ("GET", "/../hello.txt", "404"),
        ("GET", "/missing", "404"),
        ("POST", "/hello.txt", "405"),
    ] {
        let (head, _) = http(&bound, method, path, "");
        assert!(
            head.starts_with(&format!("http/1.1 {status}")),
            "{method} {path}: {head}"
        );
    }
    // A page whose domain was rebound to loopback reads nothing.
    let (head, body) = http_as(&bound, "rebound.example", "GET", "/hello.txt", "");
    assert!(head.starts_with("http/1.1 421"), "{head}");
    assert!(!body.contains("hello from A"), "{body}");
    drop((client, server));

    let site = pair.serve(&[
        "tunnel", "http", "serve", "--allow", "all", path, "--site", "--slot", "web",
    ]);
    errors_with(&site, "Site: ");
    let (_client, bound) = http_client(&pair, &site, "peer:nas/web");
    let (head, body) = http(&bound, "GET", "/", "");
    assert!(head.starts_with("http/1.1 200") && !head.contains("content-security-policy"));
    assert_eq!(body, "<p>home</p>");
    let (head, body) = http(&bound, "GET", "/missing", "");
    assert!(head.starts_with("http/1.1 404"), "{head}");
    assert_eq!(body, "lost");
    // No index.html in sub/: a site lists nothing.
    let (head, body) = http(&bound, "GET", "/sub/", "");
    assert!(head.starts_with("http/1.1 404") && body == "lost");
    // A TCP tunnel client on the HTTP slot learns nothing about it.
    let (tcp, tcp_bound) = tunnel_client(&pair, &pair.laptop, &site, "peer:nas/web");
    assert!(refusal(&tcp, &tcp_bound).contains("unknown slot or protocol"));
}

/// A browser opens many connections at once; each one is a handshake from the
/// same address, which the server caps. The client paces them: all succeed.
#[test]
fn a_burst_of_local_connections_all_get_through_a_direct_tunnel() {
    let pair = Pair::direct();
    let server = pair.serve(&[
        "tunnel",
        "tcp",
        "serve",
        "--allow",
        "all",
        "--to",
        &echo_service(),
        "--slot",
        "ssh",
    ]);
    let (_client, bound) = tunnel_client(&pair, &pair.laptop, &server, "peer:nas/ssh");
    assert!(echoes_within(&bound, Duration::from_secs(20)));
    let failures: Vec<_> = thread::scope(|scope| {
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                scope.spawn(|| {
                    let mut conn = TcpStream::connect(&bound).unwrap();
                    conn.set_read_timeout(Some(Duration::from_secs(20)))
                        .unwrap();
                    let mut reply = [0u8; 4];
                    conn.write_all(b"ping").is_ok()
                        && conn.read_exact(&mut reply).is_ok()
                        && &reply == b"ping"
                })
            })
            .collect();
        tasks
            .into_iter()
            .map(|task| task.join().unwrap())
            .filter(|ok| !ok)
            .collect()
    });
    assert!(
        failures.is_empty(),
        "{} of 20 connections failed",
        failures.len()
    );
}

#[test]
fn tunnels_serve_paired_machines_and_follow_the_book_without_a_restart() {
    let pair = Pair::direct();
    let friend = Machine::new("friend");
    let guest = Machine::new("guest");
    let stranger = Machine::new("stranger");
    // Friend and guest are paired with nas; stranger is unknown to it.
    pair.nas.pair_with("friend", &friend);
    pair.nas.pair_with("guest", &guest);
    for machine in [&friend, &guest, &stranger] {
        machine.pair_with("nas", &pair.nas);
    }
    let service = echo_service();
    let mut server = pair.serve(&[
        "tunnel", "tcp", "serve", "--allow", "all", "--to", &service, "--slot", "ssh",
    ]);
    // The announcement names who is served and how to connect, never an address.
    errors_with(
        &server,
        "Serving slot ssh to: all paired machines (now: friend, guest, laptop)",
    );
    let errors = errors_with(
        &server,
        "--direct-only tunnel tcp connect peer:nas/ssh --bind 127.0.0.1:8080 --addr ",
    );
    assert!(!errors.contains("id:"), "{errors}");
    let remote = "peer:nas/ssh";

    // Paired is enough: there is no separate grant for this slot.
    let (client, bound) = tunnel_client(&pair, &pair.laptop, &server, remote);
    assert!(echoes_within(&bound, Duration::from_secs(10)));
    let (friend_client, friend_bound) = tunnel_client(&pair, &friend, &server, remote);
    assert!(echoes_within(&friend_bound, Duration::from_secs(10)));
    let (_guest_client, guest_bound) = tunnel_client(&pair, &guest, &server, remote);
    assert!(echoes_within(&guest_bound, Duration::from_secs(10)));

    // 30: not in the book. A client announcing ping on a tunnel slot: 21.
    let (stranger_client, stranger_bound) = tunnel_client(&pair, &stranger, &server, remote);
    let error = refusal(&stranger_client, &stranger_bound);
    assert!(
        error.contains("nas does not know this machine; run `iroh-toolbox pair` on both machines"),
        "{error}"
    );
    let mut ping = pair.connect(&server, &["ping", "connect", remote, "--count", "1"]);
    ping.wait(false);
    assert!(
        ping.errors().contains("unknown slot or protocol")
            && ping
                .errors()
                .contains("`iroh-toolbox ping serve --slot ssh`"),
        "{}",
        ping.errors()
    );

    // `peers rm` and a new pairing apply to the running server, on a running client.
    pair.nas.run(&["peers", "rm", "friend"]);
    let error = refusal(&friend_client, &friend_bound);
    assert!(error.contains("does not know this machine"), "{error}");
    pair.nas.pair_with("stranger", &stranger);
    assert!(echoes_within(&stranger_bound, Duration::from_secs(10)));
    // Connections of the other peers continue, and nothing restarted the server.
    assert!(echoes_within(&bound, Duration::from_secs(10)));
    drop(client);
    assert!(server.child.try_wait().unwrap().is_none());
}

#[test]
fn allow_narrows_a_slot_to_the_listed_machines() {
    let pair = Pair::direct();
    let friend = Machine::new("friend");
    pair.nas.pair_with("friend", &friend);
    friend.pair_with("nas", &pair.nas);
    let service = echo_service();
    let server = pair.serve(&[
        "tunnel", "tcp", "serve", "--to", &service, "--slot", "ssh", "--allow", "laptop",
    ]);
    errors_with(&server, "Serving slot ssh to: laptop");
    // Another slot of the same machine has its own filter.
    let web = pair.serve(&[
        "tunnel", "tcp", "serve", "--allow", "all", "--to", &service, "--slot", "web",
    ]);
    errors_with(
        &web,
        "Serving slot web to: all paired machines (now: friend, laptop)",
    );
    let remote = "peer:nas/ssh";
    let (client, bound) = tunnel_client(&pair, &pair.laptop, &server, remote);
    assert!(echoes_within(&bound, Duration::from_secs(10)));
    // 32: paired, but not listed by this server.
    let (friend_client, friend_bound) = tunnel_client(&pair, &friend, &server, remote);
    let error = refusal(&friend_client, &friend_bound);
    assert!(
        error.contains("this nas server (slot ssh) does not allow this machine (--allow)"),
        "{error}"
    );
    let (_web_client, web_bound) = tunnel_client(&pair, &friend, &web, "peer:nas/web");
    assert!(echoes_within(&web_bound, Duration::from_secs(10)));
    // The allow entry was resolved at start: a rename cannot redirect it.
    pair.nas.run(&["peers", "rename", "laptop", "friend2"]);
    pair.nas.run(&["peers", "rename", "friend", "laptop"]);
    let error = refusal(&friend_client, &friend_bound);
    assert!(error.contains("does not allow"), "{error}");
    assert!(echoes_within(&bound, Duration::from_secs(10)));
    drop(client);
}

#[test]
fn servers_refuse_to_start_without_peers_or_with_a_bad_allow() {
    let pair = Pair::direct();
    let lonely = Machine::new("lonely");
    let start = |machine: &Machine, args: &[&str]| {
        let mut server = machine.spawn(&pair.globals, args);
        server.wait(false);
        server.errors()
    };
    for args in [
        &[
            "tunnel",
            "tcp",
            "serve",
            "--to",
            "127.0.0.1:9",
            "--slot",
            "ssh",
            "--allow",
            "all",
        ][..],
        &[
            "tunnel",
            "tcp",
            "serve",
            "--to",
            "127.0.0.1:9",
            "--allow",
            "all",
        ],
        &["tunnel", "http", "serve", ".", "--allow", "all"],
        &["pipe", "serve", "--allow", "all"],
    ] {
        let errors = start(&lonely, args);
        assert!(
            errors.contains("no paired machine; pair one with `iroh-toolbox pair` first"),
            "{errors}"
        );
        assert!(!errors.contains("grant"), "{errors}");
    }
    // Without --allow nothing starts, with or without peers, and `all` stands alone.
    for args in [
        &["tunnel", "tcp", "serve", "--to", "127.0.0.1:9"][..],
        &["tunnel", "http", "serve", "."],
        &["pipe", "serve"],
    ] {
        for machine in [&lonely, &pair.nas] {
            let errors = start(machine, args);
            assert!(
                errors.contains(
                    "name the machines to serve: --allow NAME (repeatable), or --allow all"
                ),
                "{args:?}: {errors}"
            );
        }
    }
    let errors = start(&pair.nas, &["pipe", "serve", "--allow", "all,laptop"]);
    assert!(
        errors.contains("--allow all cannot be combined"),
        "{errors}"
    );
    // A wrong folder is reported before the missing peers.
    let errors = start(
        &lonely,
        &[
            "tunnel",
            "http",
            "serve",
            "127.0.0.1:3000",
            "--allow",
            "all",
        ],
    );
    assert!(
        errors.contains(
            "127.0.0.1:3000 is not a folder; to forward a running web server, use `tunnel tcp serve --to 127.0.0.1:3000`"
        ),
        "{errors}"
    );
    for allow in ["ghost", "laptop,ghost"] {
        let errors = start(
            &pair.nas,
            &["pipe", "serve", "--slot", "web", "--allow", allow],
        );
        assert!(
            errors.contains("no paired machine named ghost; see `iroh-toolbox peers`"),
            "{allow}: {errors}"
        );
    }
}

#[test]
fn tunnel_and_pipe_clients_accept_only_peer_targets() {
    let pair = Pair::direct();
    let machine = pair.laptop.name.clone();
    assert_eq!(machine, "laptop");
    let id = format!("id:{}+ybndrfg8ejkmcpqx/ssh", "y".repeat(52));
    let cases: [(&[&str], &str); 8] = [
        (
            &["tunnel", "tcp", "connect", "endpointabc"],
            "paired machines only",
        ),
        (
            &["tunnel", "tcp", "connect", "ticket:endpointabc"],
            "paired machines only",
        ),
        (
            &["tunnel", "tcp", "connect", "code:a-b-c-d-e-f"],
            "paired machines only",
        ),
        (&["pipe", "connect", "blobabc"], "paired machines only"),
        (
            &["pipe", "connect", "code:a-b-c-d-e-f"],
            "paired machines only",
        ),
        (&["pipe", "connect", &id], "id: targets no longer exist"),
        (
            &["tunnel", "tcp", "connect", "peer:ghost"],
            "no paired machine named ghost; see `iroh-toolbox peers`",
        ),
        (
            &["pipe", "connect", "peer:ghost/ssh"],
            "no paired machine named ghost",
        ),
    ];
    for (args, expected) in cases {
        let mut client = pair.laptop.spawn(&pair.globals, args);
        client.wait(false);
        assert!(
            client.errors().contains(expected),
            "{args:?}: {}",
            client.errors()
        );
        assert!(!client.errors().contains("TCP listening"));
    }
    // The documented message in full.
    let mut client = pair
        .laptop
        .spawn(&pair.globals, &["pipe", "connect", "endpointabc"]);
    client.wait(false);
    assert!(client.errors().contains(
        "tunnel and pipe connect to paired machines only; pair with `iroh-toolbox pair`, then use peer:NAME"
    ));
    // Without a relay, a peer is found only through --addr.
    for args in [
        &["tunnel", "tcp", "connect", "peer:nas/ssh"][..],
        &["pipe", "connect", "peer:nas/ssh"],
        &["ping", "connect", "peer:nas"],
        &["bench", "connect", "peer:nas"],
    ] {
        let mut client = pair.laptop.spawn(&pair.globals, args);
        client.wait(false);
        assert!(
            client.errors().contains(
                "the server cannot be found without a relay; pass --addr HOST:PORT printed by the server"
            ),
            "{args:?}: {}",
            client.errors()
        );
    }
    // --addr belongs to peer: targets.
    let mut client = pair.laptop.spawn(
        &pair.globals,
        &["ping", "connect", "endpointabc", "--addr", "127.0.0.1:1"],
    );
    client.wait(false);
    assert!(
        client
            .errors()
            .contains("--addr applies to peer: targets only")
    );
}

#[test]
fn a_machine_that_rotated_its_identity_is_refused_with_its_old_book_entry() {
    let pair = Pair::direct();
    let service = echo_service();
    let server = pair.serve(&[
        "tunnel", "tcp", "serve", "--allow", "all", "--to", &service, "--slot", "ssh",
    ]);
    let (client, bound) = tunnel_client(&pair, &pair.laptop, &server, "peer:nas/ssh");
    assert!(echoes_within(&bound, Duration::from_secs(10)));
    drop(client);
    // laptop gets a new machine key; nas still lists the old one.
    pair.laptop.run(&["id", "rotate"]);
    let (client, bound) = tunnel_client(&pair, &pair.laptop, &server, "peer:nas/ssh");
    let error = refusal(&client, &bound);
    assert!(
        error.contains("nas does not know this machine; run `iroh-toolbox pair` on both machines"),
        "{error}"
    );
}

#[test]
fn peers_only_ping_serves_paired_machines_and_follows_the_book() {
    let pair = Pair::direct();
    let mut server = pair.serve(&["ping", "serve", "--peers-only"]);
    errors_with(&server, "Serving to: laptop");
    let mut ping = pair.connect(&server, &["ping", "connect", "peer:nas", "--count", "1"]);
    let reply = String::from_utf8(ping.wait(true)).unwrap();
    assert!(reply.starts_with("seq=1"), "{reply}");
    // The book is read on every connection: a removed machine is refused.
    let mut forget = pair.nas.spawn(&pair.globals, &["peers", "rm", "laptop"]);
    forget.wait(true);
    let mut refused = pair.connect(&server, &["ping", "connect", "peer:nas", "--count", "1"]);
    refused.wait(false);
    assert!(
        refused.errors().contains("does not know this machine"),
        "{}",
        refused.errors()
    );
    let _ = server.child.kill();
    // --allow implies --peers-only and refuses an unknown name at start.
    let mut bad = pair
        .nas
        .spawn(&pair.globals, &["bench", "serve", "--allow", "ghost"]);
    bad.wait(false);
    assert!(
        bad.errors().contains("no paired machine"),
        "{}",
        bad.errors()
    );
}

#[test]
fn ping_and_bench_reach_a_slot_by_peer_name() {
    let pair = Pair::direct();
    let mut server = pair.serve(&["ping", "serve", "--slot"]);
    errors_with(&server, "Serving slot ping");
    errors_with(&server, "ping connect peer:nas --count 3");
    let mut ping = pair.connect(&server, &["ping", "connect", "peer:nas", "--count", "1"]);
    let reply = String::from_utf8(ping.wait(true)).unwrap();
    assert!(reply.starts_with("seq=1"), "{reply}");
    // Another slot name is not served; a tunnel client on a ping slot hears why.
    let mut other = pair.connect(
        &server,
        &[
            "--timeout",
            "2",
            "ping",
            "connect",
            "peer:nas/nope",
            "--count",
            "1",
        ],
    );
    other.wait(false);
    assert!(!other.errors().contains("seq="), "{}", other.errors());
    let tunnel = pair.connect(
        &server,
        &[
            "tunnel",
            "tcp",
            "connect",
            "peer:nas/ping",
            "--bind",
            "127.0.0.1:0",
        ],
    );
    let bound = tunnel.line(true);
    let mut conn = TcpStream::connect(bound.strip_prefix("TCP listening on ").unwrap()).unwrap();
    let _ = conn.write_all(b"x");
    tunnel.until_error_line("unknown slot or protocol");
    let _ = server.child.kill();

    let mut bench = pair.serve(&["bench", "serve", "--slot"]);
    errors_with(&bench, "bench connect peer:nas");
    let mut client = pair.connect(
        &bench,
        &["bench", "connect", "peer:nas", "--bytes", "64KiB", "--json"],
    );
    let output = String::from_utf8(client.wait(true)).unwrap();
    assert_eq!(output.lines().count(), 2, "{output}");
    bench.wait(true);
}

#[test]
fn a_fixed_port_is_bound_or_reported() {
    let pair = Pair::direct();
    let occupied = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let port = occupied.local_addr().unwrap().port().to_string();
    for args in [
        vec![
            "pipe", "serve", "--allow", "all", "--slot", "ssh", "--port", &port,
        ],
        vec!["ping", "serve", "--port", &port],
    ] {
        let mut server = pair.nas.spawn(&pair.globals, &args);
        server.wait(false);
        assert!(
            server
                .errors()
                .contains(&format!("cannot bind UDP port {port}")),
            "{}",
            server.errors()
        );
    }
}

#[test]
fn pipes_carry_data_between_paired_machines_only() {
    let pair = Pair::direct();
    let stranger = Machine::new("stranger");
    stranger.pair_with("nas", &pair.nas);
    let mut server = pair.serve(&["-v", "pipe", "serve", "--allow", "all", "--recv-only"]);
    errors_with(&server, "Serving to: all paired machines (now: laptop)");
    // A stranger fails its handshake; the server keeps waiting for a paired machine.
    let mut refused = pair.connect_as(&stranger, &server, &["pipe", "connect", "peer:nas"]);
    refused.wait(false);
    assert!(
        refused
            .errors()
            .contains("nas does not know this machine; run `iroh-toolbox pair` on both machines"),
        "{}",
        refused.errors()
    );
    server.until_error_line("pipe: ignored a connection");
    let mut client = pair.connect(&server, &["pipe", "connect", "peer:nas"]);
    client.input(b"paired payload");
    assert!(client.wait(true).is_empty());
    assert_eq!(server.wait(true), b"paired payload");
}

#[test]
fn a_refused_pipe_client_never_receives_what_the_server_reads_from_stdin() {
    let pair = Pair::direct();
    let stranger = Machine::new("stranger");
    let guest = Machine::new("guest");
    pair.nas.pair_with("guest", &guest);
    for machine in [&stranger, &guest] {
        machine.pair_with("nas", &pair.nas);
    }
    // No --recv-only: the server would send its stdin to the client it serves.
    let mut server = pair.serve(&["pipe", "serve", "--allow", "laptop"]);
    server.input(b"the server's secret");
    // Unpaired (30) and paired outside --allow (32): nothing reaches stdout.
    for (who, expected) in [
        (
            &stranger,
            "nas does not know this machine; run `iroh-toolbox pair` on both machines",
        ),
        (
            &guest,
            "this nas server (slot pipe) does not allow this machine (--allow)",
        ),
    ] {
        let mut refused = pair.connect_as(who, &server, &["pipe", "connect", "peer:nas"]);
        let output = refused.wait(false);
        assert!(output.is_empty(), "a refused client received data");
        assert!(refused.errors().contains(expected), "{}", refused.errors());
    }
    // The listed machine still gets it: the server was waiting.
    let mut client = pair.connect(&server, &["pipe", "connect", "peer:nas"]);
    client.input(b"hello");
    assert_eq!(client.wait(true), b"the server's secret");
    assert_eq!(server.wait(true), b"hello");
    // The refusal of a known peer is visible to the operator; a stranger's is not.
    assert!(
        server
            .errors()
            .contains("refused guest on slot pipe: not listed by --allow"),
        "{}",
        server.errors()
    );
    assert!(!server.errors().contains("stranger"), "{}", server.errors());
}

#[test]
fn rotating_the_address_makes_a_former_peers_target_useless() {
    let pair = Pair::direct();
    let service = echo_service();
    let serve = ["tunnel", "tcp", "serve", "--allow", "all", "--to", &service];
    let mut server = pair.serve(&serve);
    let (client, bound) = tunnel_client(&pair, &pair.laptop, &server, "peer:nas");
    assert!(echoes_within(&bound, Duration::from_secs(10)));
    drop(client);
    server.child.kill().unwrap();
    server.wait(false);
    // The machine key stays, the salt changes, so the slot endpoint ID changes.
    pair.nas.run(&["id", "rotate", "--address"]);
    let restarted = pair.serve(&serve);
    // laptop still holds the old address and the new direct address: unreachable.
    let client = pair.connect(
        &restarted,
        &[
            "--timeout",
            "2",
            "tunnel",
            "tcp",
            "connect",
            "peer:nas",
            "--bind",
            "127.0.0.1:0",
        ],
    );
    let bound = client.line(true);
    let bound = bound.strip_prefix("TCP listening on ").unwrap();
    let error = refusal(&client, bound);
    assert!(error.contains("nas"), "{error}");
    assert!(
        !error.contains("tunnel error: nas does not know"),
        "{error}"
    );
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:slots"]
fn slot_keeps_its_address_across_restarts_and_has_one_holder() {
    let relay = LocalRelay::start(None);
    let pair = Pair::relayed(vec!["--relay".into(), relay.url.clone()]);
    let service = echo_service();
    let serve = [
        "tunnel", "tcp", "serve", "--to", &service, "--slot", "web", "--allow", "all",
    ];
    let server = pair.serve(&serve);
    // `id` shows no address, and neither does the server.
    assert!(!server.errors().contains("id:"), "{}", server.errors());
    // A second process cannot take the same slot.
    let mut second = pair.nas.spawn(&pair.globals, &serve);
    second.wait(false);
    assert!(
        second.errors().contains("already served"),
        "{}",
        second.errors()
    );
    let (client, bound) = tunnel_client(&pair, &pair.laptop, &server, "peer:nas/web");
    assert!(echoes_within(&bound, Duration::from_secs(15)));
    // After a restart the slot has the same address and serves the same client.
    // iroh drops the stale direct path of the previous process after 15 s
    // (PATH_MAX_IDLE_TIMEOUT), so a running client recovers within that delay.
    let mut server = server;
    server.child.kill().unwrap();
    server.wait(false);
    let restarted = pair.serve(&serve);
    let ok = echoes_within(&bound, Duration::from_secs(30));
    assert!(
        ok,
        "client: {}\nserver: {}",
        client.errors(),
        restarted.errors()
    );
    // The restarted process holds the newest registration: nothing to report.
    assert!(
        !restarted.errors().contains("another machine"),
        "{}",
        restarted.errors()
    );
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:slots"]
fn a_slot_taken_over_at_the_relay_is_reported_until_it_is_back() {
    let mut relay = LocalRelay::start(None);
    let pair = Pair::relayed(vec!["--relay".into(), relay.url.clone()]);
    let service = echo_service();
    let serve = [
        "tunnel", "tcp", "serve", "--to", &service, "--slot", "web", "--allow", "all",
    ];
    let server = pair.serve(&serve);
    // A relay restart drops every registration; the server registers again
    // and serves clients without any warning.
    relay.restart();
    let (client, bound) = tunnel_client(&pair, &pair.laptop, &server, "peer:nas/web");
    assert!(
        echoes_within(&bound, Duration::from_secs(30)),
        "client: {}\nserver: {}",
        client.errors(),
        server.errors()
    );
    drop(client);
    assert!(
        !server.errors().contains("another machine"),
        "{}",
        server.errors()
    );
    // A copy of nas's identity serves the same slot: its registration is the
    // newest, and the relay stops delivering to nas.
    let mut twin = pair.serve_from(&pair.nas.copy(), &serve);
    errors_with(&server, "has taken slot web's address at the relay");
    // The warning spans several lines: wait for its last one.
    let errors = errors_with(&server, "then restart this server");
    // The client hint above names the relay; the warning itself names nothing.
    let warning = &errors[errors.find("warning: another machine").unwrap()..];
    assert!(
        warning.contains("`iroh-toolbox id rotate --address`"),
        "{warning}"
    );
    for secret in [&relay.url, &pair.nas.key, &pair.nas.salt] {
        assert!(!warning.contains(secret.as_str()), "{warning}");
    }
    twin.child.kill().unwrap();
    twin.wait(false);
    let errors = errors_with(&server, "slot web is reachable through the relay again");
    assert_eq!(
        errors.matches("warning: another machine").count(),
        1,
        "{errors}"
    );
    assert!(
        !twin.errors().contains("another machine"),
        "{}",
        twin.errors()
    );
    // The note tells the truth: laptop reaches nas through the relay again.
    let (client, bound) = tunnel_client(&pair, &pair.laptop, &server, "peer:nas/web");
    assert!(
        echoes_within(&bound, Duration::from_secs(30)),
        "client: {}\nserver: {}",
        client.errors(),
        server.errors()
    );
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:slots"]
fn slot_targets_use_the_default_slot_and_explain_mismatches() {
    let relay = LocalRelay::start(None);
    let pair = Pair::relayed(vec!["--relay".into(), relay.url.clone()]);
    let server = pair.serve(&["ping", "serve", "--slot"]);
    // Without /SLOT, ping connect selects the "ping" slot.
    let mut ping = pair.connect(&server, &["ping", "connect", "peer:nas", "--count", "1"]);
    let reply = String::from_utf8(ping.wait(true)).unwrap();
    assert!(reply.starts_with("seq=1"), "{reply}");
    // The right slot with the wrong command is refused with its reason.
    let mut pipe = pair.connect(&server, &["pipe", "connect", "peer:nas/ping"]);
    pipe.wait(false);
    assert!(
        pipe.errors().contains("unknown slot or protocol"),
        "{}",
        pipe.errors()
    );
    // An unknown slot cannot be reached.
    let mut ping = pair.connect(
        &server,
        &[
            "--timeout",
            "2",
            "ping",
            "connect",
            "peer:nas/nope",
            "--count",
            "1",
        ],
    );
    ping.wait(false);
    assert!(
        !ping.errors().is_empty() && !ping.errors().contains("seq="),
        "{}",
        ping.errors()
    );
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:slots"]
fn single_client_slots_survive_refused_clients_and_explain_busy() {
    let relay = LocalRelay::start(None);
    let pair = Pair::relayed(vec!["--relay".into(), relay.url.clone()]);
    let stranger = Machine::new("stranger");
    stranger.pair_with("nas", &pair.nas);
    let mut server = pair.serve(&["-v", "pipe", "serve", "--allow", "all", "--recv-only"]);
    // A client of another command is refused, and the slot keeps waiting.
    let mut wrong = pair.connect(
        &server,
        &["ping", "connect", "peer:nas/pipe", "--count", "1"],
    );
    wrong.wait(false);
    assert!(
        wrong.errors().contains("unknown slot or protocol"),
        "{}",
        wrong.errors()
    );
    server.until_error_line("pipe: ignored a connection");
    // So is a machine that is not paired.
    let mut unpaired = pair.connect_as(&stranger, &server, &["pipe", "connect", "peer:nas"]);
    unpaired.wait(false);
    assert!(
        unpaired.errors().contains("does not know this machine"),
        "{}",
        unpaired.errors()
    );
    let mut client = pair.connect(&server, &["pipe", "connect", "peer:nas"]);
    server.until_error_line("QUIC route=");
    let mut second = pair.connect(&server, &["pipe", "connect", "peer:nas", "--recv-only"]);
    second.wait(false);
    assert!(
        second
            .errors()
            .contains("is already serving another client; try again later"),
        "{}",
        second.errors()
    );
    client.input(b"slot payload");
    assert!(client.wait(true).is_empty());
    assert_eq!(server.wait(true), b"slot payload");
    // bench serve, also single-client, survives the same mistake.
    let mut bench = pair.serve(&["bench", "serve", "--slot"]);
    let mut wrong = pair.connect(
        &bench,
        &["ping", "connect", "peer:nas/bench", "--count", "1"],
    );
    wrong.wait(false);
    assert!(
        wrong.errors().contains("unknown slot or protocol"),
        "{}",
        wrong.errors()
    );
    let mut client = pair.connect(
        &bench,
        &["bench", "connect", "peer:nas", "--bytes", "64KiB"],
    );
    client.wait(true);
    bench.wait(true);
}
