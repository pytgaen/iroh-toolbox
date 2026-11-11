use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

#[path = "support/ports_replacement.rs"]
mod ports_replacement;
#[path = "support/public_relay.rs"]
mod public_relay;

#[path = "support/pairing.rs"]
mod pairing;
#[path = "support/peer_chat.rs"]
mod peer_chat;
#[path = "support/peer_files.rs"]
mod peer_files;
#[path = "support/slots.rs"]
mod slots;

/// The official iroh-relay on loopback: code pairing and slots meet through it
/// without any public relay, DNS or mailbox.
struct LocalRelay {
    peer: Peer,
    url: String,
    port: u16,
    directory: tempfile::TempDir,
}

impl LocalRelay {
    fn start(token: Option<&str>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let access = token.map_or(String::new(), |token| {
            format!("access.shared_token = [{token:?}]\n")
        });
        let config = directory.path().join("relay.toml");
        std::fs::write(&config, format!("http_bind_addr = \"127.0.0.1:{port}\"\n{access}enable_quic_addr_discovery = false\nenable_metrics = false\n")).unwrap();
        Self {
            peer: Self::launch(&config, port),
            url: format!("http://127.0.0.1:{port}"),
            port,
            directory,
        }
    }

    fn launch(config: &Path, port: u16) -> Peer {
        let binary = std::env::var_os("IROH_TEST_RELAY_BINARY").filter(|v| !v.is_empty())
            .expect("IROH_TEST_RELAY_BINARY must point to the official iroh-relay; pairing and slots were NOT tested");
        let mut command = Command::new(binary);
        command
            .args(["--dev", "--config-path"])
            .arg(config)
            .env("RUST_LOG", "off");
        let peer = Peer::start(command);
        let start = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "relay did not start: {}",
                peer.errors()
            );
            thread::sleep(Duration::from_millis(20));
        }
        peer
    }

    /// Restarts the relay on the same address: every registration is lost.
    fn restart(&mut self) {
        let _ = self.peer.child.kill();
        let _ = self.peer.child.wait();
        self.peer = Self::launch(&self.directory.path().join("relay.toml"), self.port);
    }

    fn spawn(&self, args: &[&str]) -> Peer {
        let mut all = vec!["--relay", &self.url];
        all.extend_from_slice(args);
        Peer::spawn(&all)
    }
}

struct Peer {
    child: Child,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    out_lines: mpsc::Receiver<String>,
    err_lines: mpsc::Receiver<String>,
    readers: Vec<thread::JoinHandle<()>>,
    /// A private machine identity unless the test chose one explicitly.
    _home: Option<tempfile::TempDir>,
    /// The other peer's stderr, shown when this one times out.
    partner: Option<Arc<Mutex<Vec<u8>>>>,
}

fn capture(
    stream: impl Read + Send + 'static,
) -> (
    Arc<Mutex<Vec<u8>>>,
    mpsc::Receiver<String>,
    thread::JoinHandle<()>,
) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let output = bytes.clone();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line).unwrap_or(0) != 0 {
            output.lock().unwrap().extend_from_slice(&line);
            let _ = tx.send(String::from_utf8_lossy(&line).trim().to_owned());
            line.clear();
        }
    });
    (bytes, rx, reader)
}

impl Peer {
    fn spawn(args: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"));
        // Local scenarios must not depend on or contact public infrastructure.
        if !args.iter().any(|arg| {
            matches!(
                *arg,
                "--relay" | "--relay-only" | "--direct-only" | "--config"
            )
        }) {
            command.arg("--direct-only");
        }
        command.args(args);
        Self::start(command)
    }
    fn start(mut command: Command) -> Self {
        let explicit_home = command
            .get_envs()
            .any(|(key, value)| key == "IROH_TOOLBOX_HOME" && value.is_some());
        for (key, _) in std::env::vars() {
            if key == "IROH_TOOLBOX_HOME" && explicit_home {
                continue;
            }
            if key.starts_with("IROH_")
                || matches!(
                    key.as_str(),
                    "HTTP_PROXY"
                        | "http_proxy"
                        | "HTTPS_PROXY"
                        | "https_proxy"
                        | "ALL_PROXY"
                        | "all_proxy"
                )
            {
                command.env_remove(key);
            }
        }
        // Never read or create the developer's real machine identity.
        let home = (!explicit_home).then(|| tempfile::tempdir().unwrap());
        if let Some(home) = &home {
            command.env("IROH_TOOLBOX_HOME", home.path());
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (stdout, out_lines, out_reader) = capture(child.stdout.take().unwrap());
        let (stderr, err_lines, err_reader) = capture(child.stderr.take().unwrap());
        Self {
            child,
            stdout,
            stderr,
            out_lines,
            err_lines,
            readers: vec![out_reader, err_reader],
            _home: home,
            partner: None,
        }
    }
    /// Shows `other`'s stderr too when a wait of this peer times out: the
    /// cause of a missing line is often on the other side.
    fn watch(&mut self, other: &Peer) {
        self.partner = Some(other.stderr.clone());
    }
    fn report(&self) -> String {
        match &self.partner {
            Some(partner) => format!(
                "{}\n--- other peer's stderr ---\n{}",
                self.errors(),
                String::from_utf8_lossy(&partner.lock().unwrap())
            ),
            None => self.errors(),
        }
    }
    fn line(&self, stderr: bool) -> String {
        let lines = if stderr {
            &self.err_lines
        } else {
            &self.out_lines
        };
        lines
            .recv_timeout(Duration::from_secs(15))
            .unwrap_or_else(|_| panic!("no ticket: {}", self.report()))
    }
    fn errors(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }
    fn until_error_line(&self, expected: &str) -> String {
        let start = Instant::now();
        loop {
            let line = self
                .err_lines
                .recv_timeout(Duration::from_secs(15).saturating_sub(start.elapsed()))
                .unwrap_or_else(|_| panic!("missing {expected}: {}", self.report()));
            if line.contains(expected) {
                return line;
            }
        }
    }
    fn wait(&mut self, success: bool) -> Vec<u8> {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "process timed out: {}",
                self.errors()
            );
            thread::sleep(Duration::from_millis(10));
        };
        for reader in self.readers.drain(..) {
            reader.join().unwrap();
        }
        assert_eq!(status.success(), success, "{}", self.errors());
        self.stdout.lock().unwrap().clone()
    }
    fn input(&mut self, data: &[u8]) {
        self.child.stdin.take().unwrap().write_all(data).unwrap();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

#[test]
fn native_version_matches_compiled_components() {
    let binary = env!("CARGO_BIN_EXE_iroh-toolbox");
    let result = Command::new(binary)
        .args(["--config", "nonexistent-version-test.toml", "--version"])
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.starts_with(concat!("iroh-toolbox v", env!("CARGO_PKG_VERSION"), "\n")));
    assert!(text.contains(concat!("License   ", env!("CARGO_PKG_LICENSE"))));
    let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).unwrap();
    assert!(!text.contains("magic-wormhole") && !text.contains("EUPL"));
    for name in ["iroh", "iroh-blobs", "iroh-tickets"] {
        let row = text
            .lines()
            .find(|line| line.split_whitespace().next() == Some(name));
        let package = lock["package"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| package["name"].as_str() == Some(name))
            .unwrap();
        assert_eq!(
            row.unwrap().split_whitespace().nth(1),
            package["version"].as_str()
        );
    }
    let short = Command::new(binary).arg("-V").output().unwrap();
    assert!(short.status.success());
    assert!(short.stderr.is_empty());
    assert_eq!(
        short.stdout,
        concat!("iroh-toolbox v", env!("CARGO_PKG_VERSION"), "\n").as_bytes()
    );
}

#[test]
fn native_help_and_validation() {
    for args in [
        vec!["--help"],
        vec!["relay", "--help"],
        vec!["ping", "--help"],
        vec!["bench", "--help"],
        vec!["send", "--help"],
        vec!["receive", "--help"],
        vec!["chat", "--help"],
        vec!["chat", "serve", "--help"],
        vec!["chat", "connect", "--help"],
        vec!["pipe", "--help"],
        vec!["tunnel", "tcp", "--help"],
        vec!["ticket", "--help"],
    ] {
        let mut peer = Peer::spawn(&args);
        assert!(!peer.wait(true).is_empty());
    }
    let mut peer = Peer::spawn(&["--timeout", "NaN", "relay", "check"]);
    peer.wait(false);
    let mut peer = Peer::spawn(&["--relay-only", "--direct-only", "ping", "serve"]);
    peer.wait(false);
    // Files and chat have no ticket or `--code` option: a code is the only way in.
    for args in [
        vec!["send", "file", "--code"],
        vec!["chat", "serve", "--code"],
        vec!["receive", "code:a-b", "--code"],
    ] {
        let mut peer = Peer::spawn(&args);
        assert!(peer.wait(false).is_empty());
        assert!(peer.errors().contains("unexpected argument"), "{args:?}");
    }
}

#[test]
fn native_chat_roles_are_explicit() {
    // `chat` alone shows its subcommands and opens nothing.
    let mut peer = Peer::spawn(&["chat"]);
    peer.wait(false);
    assert!(
        peer.errors().contains("serve") && peer.errors().contains("connect"),
        "{}",
        peer.errors()
    );
    assert!(!peer.errors().contains("Code:"), "{}", peer.errors());
    // The forms without a role are refused, naming the subcommand to use.
    for (args, expected) in [
        (
            &["chat", "code:a-b"][..],
            "`iroh-toolbox chat connect code:`",
        ),
        (
            &["chat", "peer:nas"],
            "`iroh-toolbox chat connect peer:NAME`",
        ),
        (
            &["chat", "--peers-only"],
            "`iroh-toolbox chat serve --peers-only`",
        ),
    ] {
        let mut peer = Peer::spawn(args);
        assert!(peer.wait(false).is_empty());
        assert!(
            peer.errors().contains("chat needs a role") && peer.errors().contains(expected),
            "{args:?}: {}",
            peer.errors()
        );
    }
    // `chat connect` needs a target; `chat serve` takes none.
    for args in [&["chat", "connect"][..], &["chat", "serve", "code:a-b"]] {
        let mut peer = Peer::spawn(args);
        assert!(peer.wait(false).is_empty());
        assert!(
            peer.errors().contains("Usage: iroh-toolbox chat"),
            "{args:?}"
        );
    }
}

#[test]
fn native_receive_and_chat_refuse_tickets_and_other_targets() {
    let id = iroh::SecretKey::from_bytes(&[9; 32]).public().to_string();
    let machine = iroh::SecretKey::from_bytes(&[9; 32]).public().to_z32();
    let targets = [
        "blobabcdef".to_owned(),
        "endpointabcdef".to_owned(),
        "ticket:endpointabcdef".to_owned(),
        id,
        format!("id:{machine}+ybndrfg8ejkmcpqx"),
        "nas".to_owned(),
    ];
    for target in &targets {
        for command in ["receive", "chat connect"] {
            let start = Instant::now();
            let mut args: Vec<&str> = command.split(' ').collect();
            args.push(target);
            let mut peer = Peer::spawn(&args);
            assert!(peer.wait(false).is_empty());
            assert!(
                peer.errors().contains("files and chat take code:WORDS")
                    && peer.errors().contains("peer:NAME"),
                "{command} {target}: {}",
                peer.errors()
            );
            // No ticket is parsed or dialled, and nothing leaks into the message.
            assert!(!peer.errors().contains("BLAKE3"), "{}", peer.errors());
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }
    // A valid code typed without its prefix is told how to write it.
    let bare = "abacus-abdomen-abdominal-abide-abiding-ability";
    for command in ["receive", "chat connect"] {
        let mut args: Vec<&str> = command.split(' ').collect();
        args.push(bare);
        let mut peer = Peer::spawn(&args);
        assert!(peer.wait(false).is_empty());
        assert!(
            peer.errors()
                .contains(&format!("iroh-toolbox {command} code:<words>")),
            "{}",
            peer.errors()
        );
    }
    // Reading the code from the terminal needs one; the argument form stays.
    for args in [
        &["receive"][..],
        &["receive", "code:"],
        &["chat", "connect", "code:"],
    ] {
        let mut peer = Peer::spawn(args);
        assert!(peer.wait(false).is_empty());
        assert!(
            peer.errors().contains("reads the code from a terminal")
                && peer.errors().contains("code:WORDS"),
            "{args:?}: {}",
            peer.errors()
        );
    }
}

#[test]
fn native_id_and_peers_are_local_commands() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str], success: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"));
        command
            .args(args)
            .env("IROH_TOOLBOX_HOME", home.path())
            .env_remove("IROH_RELAY_TOKEN");
        let mut peer = Peer::start(command);
        let output = String::from_utf8(peer.wait(success)).unwrap();
        (output, peer.errors())
    };
    let (id, _) = run(&["id", "rename", "nas"], true);
    assert!(id.is_empty());
    let (id, _) = run(&["id"], true);
    let lines: Vec<_> = id.lines().collect();
    assert_eq!(lines[0], "Name:     nas (announced when pairing)");
    assert!(
        lines[1].starts_with("Machine:  ") && lines[1].contains('…'),
        "{id}"
    );
    assert_eq!(
        lines[2],
        "Key:      not protected (iroh-toolbox id protect)"
    );
    assert_eq!(lines[3], "Peers:    none");
    assert!(!id.contains("id:"));
    let (json, _) = run(&["id", "--json"], true);
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["name"], "nas");
    assert_eq!(json["peers"], serde_json::json!([]));
    assert_eq!(json["protected"], false);
    let (_, errors) = run(&["id", "rename", "all"], false);
    assert!(errors.contains("reserved"), "{errors}");
    let (_, errors) = run(&["id", "rename", "Bad Name"], false);
    assert!(errors.contains("lowercase"), "{errors}");

    let (list, errors) = run(&["peers"], true);
    assert!(
        list.is_empty() && errors.contains("iroh-toolbox pair"),
        "{errors}"
    );
    let (json, _) = run(&["peers", "--json"], true);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap()["peers"],
        serde_json::json!([])
    );
    for args in [
        &["peers", "rename", "ghost", "x"][..],
        &["peers", "rm", "ghost"],
    ] {
        let (_, errors) = run(args, false);
        assert!(
            errors.contains("ghost") && errors.contains("peers"),
            "{errors}"
        );
    }
    // Per-slot grants are gone: servers narrow with --allow.
    let (_, errors) = run(&["peers", "access", "ghost", "ssh"], false);
    assert!(errors.contains("unrecognized subcommand"), "{errors}");

    // A damaged book is reported with its path, never ignored, and blocks
    // the rotation before it changes anything.
    let book = home.path().join("peers.toml");
    let machine = |text: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix("Machine:  "))
            .unwrap()
            .to_owned()
    };
    let before = machine(&run(&["id"], true).0);
    std::fs::write(&book, "[peers.x]\n").unwrap();
    let (_, errors) = run(&["peers"], false);
    assert!(errors.contains("peers.toml"), "{errors}");
    let (_, errors) = run(&["id", "rotate"], false);
    assert!(errors.contains("peers.toml"), "{errors}");
    std::fs::remove_file(&book).unwrap();
    assert_eq!(machine(&run(&["id"], true).0), before);

    // A hand-written book: listing, rename, removal.
    let key = |seed: u8| iroh::SecretKey::from_bytes(&[seed; 32]).public().to_z32();
    let entry = |name: &str, seed: u8| {
        format!(
            "[peers.{name}]\nmachine = \"{}\"\nsalt = \"yyyyyyyyyyyyyyyy\"\nannounced = \"{name}\"\npaired = \"2026-11-01T10:12:00Z\"\n",
            key(seed)
        )
    };
    let write_book = |text: &str| {
        std::fs::write(&book, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&book, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    };
    // A book from a development build with per-slot access is refused as is.
    let legacy = entry("laptop", 1).replace("paired =", "access = [\"ssh\"]\npaired =");
    write_book(&legacy);
    for args in [&["peers"][..], &["peers", "rm", "laptop"], &["id"]] {
        let (_, errors) = run(args, false);
        assert!(
            errors.contains("peers.toml") && errors.contains("delete it and pair again"),
            "{errors}"
        );
    }
    assert_eq!(std::fs::read_to_string(&book).unwrap(), legacy);
    let text = format!(
        "{}\n{}\n{}",
        entry("laptop", 1),
        entry("phone", 2),
        entry("tv", 3)
    );
    write_book(&text);
    let short = |seed: u8| {
        let key = key(seed);
        format!("{}…{}", &key[..4], &key[key.len() - 4..])
    };
    let (list, _) = run(&["peers"], true);
    let rows: Vec<Vec<&str>> = list
        .lines()
        .map(|line| {
            line.split("  ")
                .filter(|c| !c.is_empty())
                .map(str::trim)
                .collect()
        })
        .collect();
    assert_eq!(rows[0], ["NAME", "MACHINE", "PAIRED"]);
    assert_eq!(rows[1], ["laptop", &short(1), "2026-11-01"]);
    assert_eq!(rows[2], ["phone", &short(2), "2026-11-01"]);
    assert_eq!(rows[3], ["tv", &short(3), "2026-11-01"]);
    let (json, _) = run(&["peers", "--json"], true);
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        json["peers"][0],
        serde_json::json!({
            "name": "laptop",
            "machine": short(1),
            "announced": "laptop",
            "paired": "2026-11-01T10:12:00Z",
        })
    );
    assert!(!json.to_string().contains("yyyyyyyyyyyyyyyy"));
    let (id, _) = run(&["id"], true);
    assert_eq!(id.lines().nth(3).unwrap(), "Peers:    laptop, phone, tv");
    run(&["peers", "rename", "tv", "lounge"], true);
    run(&["peers", "rm", "phone"], true);
    let (_, errors) = run(&["peers", "rename", "lounge", "laptop"], false);
    assert!(errors.contains("taken"), "{errors}");
    let (list, _) = run(&["peers"], true);
    assert!(list.contains("lounge"), "{list}");
    assert!(!list.contains("phone") && !list.contains("tv "), "{list}");
    let (_, errors) = run(&["id", "--json", "rename", "x"], false);
    assert!(errors.contains("--json"), "{errors}");

    // Rotation lists the paired machines and the old short key; a broken
    // name file does not block it.
    std::fs::write(home.path().join("name"), "Not Valid").unwrap();
    let (_, errors) = run(&["id", "rotate", "--address"], true);
    assert!(
        errors.contains("Pair again with: laptop, lounge"),
        "{errors}"
    );
    assert!(errors.contains("Restart running servers"), "{errors}");
    let (_, errors) = run(&["id", "rotate"], true);
    assert!(errors.contains(&before), "{errors}");
    assert!(errors.contains("peers rm NAME"), "{errors}");
    assert!(errors.contains("laptop, lounge"), "{errors}");
    std::fs::remove_file(home.path().join("name")).unwrap();
    assert_ne!(machine(&run(&["id"], true).0), before);
}

#[test]
fn native_pairing_validation_before_network_use() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("file");
    std::fs::write(&source, b"data").unwrap();
    // Peer::spawn adds --direct-only: pairing needs a relay to meet.
    for args in [
        vec!["send", source.to_str().unwrap()],
        vec!["chat", "serve"],
    ] {
        let mut peer = Peer::spawn(&args);
        assert!(peer.wait(false).is_empty());
        assert!(
            peer.errors().contains("remove --direct-only"),
            "{}",
            peer.errors()
        );
    }
    // Malformed codes and a non-interactive terminal fail before any connection.
    let unused = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = format!("http://{}", unused.local_addr().unwrap());
    drop(unused);
    for (code, expected) in [
        ("abacus-abdomen-abdominal", "a code has 6 to 32 words"),
        (
            "abacus-abdomen-abdominal-abide-abiding-notaword",
            "not in the word list",
        ),
    ] {
        let joined = format!("code:{code}");
        for command in [
            vec!["receive", joined.as_str()],
            vec!["chat", "connect", joined.as_str()],
        ] {
            let start = Instant::now();
            let mut args = vec!["--relay", relay.as_str()];
            args.extend(command);
            let mut peer = Peer::spawn(&args);
            peer.wait(false);
            assert!(peer.errors().contains(expected), "{}", peer.errors());
            assert!(!peer.errors().contains("notaword"));
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }
    // The confirmation number is the default: without a terminal, every command
    // that makes or uses a code refuses before any network activity.
    for command in [
        vec!["send", source.to_str().unwrap()],
        vec!["chat", "serve"],
        vec![
            "receive",
            "code:abacus-abdomen-abdominal-abide-abiding-ability",
        ],
        vec![
            "chat",
            "connect",
            "code:abacus-abdomen-abdominal-abide-abiding-ability",
        ],
    ] {
        let start = Instant::now();
        let mut args = vec!["--relay", relay.as_str()];
        args.extend(command);
        let mut peer = Peer::spawn(&args);
        peer.wait(false);
        assert!(
            peer.errors().contains("needs an interactive terminal")
                && peer.errors().contains("--no-verify"),
            "{}",
            peer.errors()
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}

#[test]
fn native_chat_timestamps_reject_invalid_values_before_network_use() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"));
    command.env("IROH_TOOLBOX_TIMESTAMPS", "sometimes").args([
        "--direct-only",
        "chat",
        "serve",
        "--no-progress",
    ]);
    let mut invalid = Peer::start(command);
    invalid.wait(false);
    assert!(invalid.errors().contains("auto, always or never"));
}

#[cfg(unix)]
#[test]
#[ignore = "requires an official iroh-relay binary, Python and Unix PTYs; run explicitly for the interactive chat line editor"]
fn native_chat_line_editor_on_real_terminals() {
    // Chats meet by code, so the terminals need a relay.
    let relay = LocalRelay::start(None);
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/chat_editor.py"
        ))
        .arg(env!("CARGO_BIN_EXE_iroh-toolbox"))
        .arg(&relay.url)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires Python and Unix PTYs; run mise run test:pty"]
fn native_code_prompt_hides_the_code_on_real_terminals() {
    // The prompt comes before any network use: a relay URL that is never contacted will do.
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/code_prompt.py"
        ))
        .arg(env!("CARGO_BIN_EXE_iroh-toolbox"))
        .arg("http://127.0.0.1:9")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires Python and Unix PTYs; run mise run test:pty"]
fn native_machine_key_passphrase_on_real_terminals() {
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/identity_protect.py"
        ))
        .arg(env!("CARGO_BIN_EXE_iroh-toolbox"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn native_ping_and_offline_ticket() {
    let server = Peer::spawn(&["ping", "serve"]);
    let ticket = server.line(false);
    assert!(ticket.starts_with("endpoint"));
    let mut client = Peer::spawn(&[
        "ping",
        "connect",
        &ticket,
        "--count",
        "2",
        "--interval",
        "0.01",
        "--json",
    ]);
    let bytes = client.wait(true);
    let lines = String::from_utf8(bytes).unwrap();
    let replies: Vec<serde_json::Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 2);
    assert!(
        replies
            .iter()
            .all(|v| v["route"] == "direct" && v["carrier"] == "UDP")
    );
    server.until_error_line(&format!(
        "iroh-toolbox --direct-only ping connect \"{ticket}\" --count 3"
    ));
    let result = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"))
        .env("IROH_TOOLBOX_CONFIG", "/nonexistent/config")
        .env("IROH_TOOLBOX_TOKEN", "invalid secret")
        .args(["ticket", "inspect", &ticket, "--json"])
        .output()
        .unwrap();
    assert!(result.status.success());
    let info: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(!info["direct_addresses"].as_array().unwrap().is_empty());
}

#[test]
fn native_benchmark_both_directions() {
    for quiet in [false, true] {
        let mut serve_args = vec!["bench", "serve"];
        if quiet {
            serve_args.push("--no-progress");
        }
        let mut server = Peer::spawn(&serve_args);
        let ticket = server.line(false);
        let mut connect_args = vec!["bench", "connect", &ticket, "--bytes", "1MiB", "--json"];
        // Percentages are -vv only when redirected; default output lists steps.
        connect_args.push(if quiet { "--no-progress" } else { "-vv" });
        let mut client = Peer::spawn(&connect_args);
        let output = String::from_utf8(client.wait(true)).unwrap();
        let results: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["direction"], "upload");
        assert_eq!(results[1]["direction"], "download");
        assert!(
            results
                .iter()
                .all(|v| v["bytes"] == 1048576 && v["mbps"].as_f64().unwrap() > 0.0)
        );
        assert_eq!(client.errors().contains("bench:"), !quiet);
        if !quiet {
            assert!(client.errors().contains("100%"));
        }
        server.wait(true);
    }
}

#[test]
fn native_pipe_raw_payload_and_half_close() {
    let pair = slots::Pair::direct();
    let mut server = pair.serve(&["pipe", "serve", "--allow", "all", "--recv-only"]);
    let mut client = pair.connect(&server, &["pipe", "connect", "peer:nas"]);
    let data: Vec<u8> = (0..262144).map(|v| (v % 251) as u8).collect();
    client.input(&data);
    assert!(client.wait(true).is_empty());
    assert_eq!(server.wait(true), data);
    let errors = server.errors();
    assert!(
        errors.contains("Serving to: all paired machines (now: laptop)"),
        "{errors}"
    );
    assert!(
        errors.contains("iroh-toolbox --direct-only pipe connect peer:nas --addr "),
        "{errors}"
    );
}

#[test]
fn native_pipe_ignores_failed_attempts_and_refuses_a_second_client() {
    let pair = slots::Pair::direct();
    let mut server = pair.serve(&["-v", "pipe", "serve", "--allow", "all", "--recv-only"]);
    // Another protocol fails its handshake without ending the server.
    let mut stranger = pair.connect(
        &server,
        &["ping", "connect", "peer:nas/pipe", "--count", "1"],
    );
    stranger.wait(false);
    server.until_error_line("pipe: ignored a connection");
    let mut client = pair.connect(&server, &["pipe", "connect", "peer:nas"]);
    server.until_error_line("QUIC route=");
    let mut second = pair.connect(&server, &["pipe", "connect", "peer:nas", "--recv-only"]);
    second.wait(false);
    // The busy reason is explained locally, never echoed from the network.
    assert!(
        second
            .errors()
            .contains("is already serving another client; try again later"),
        "{}",
        second.errors()
    );
    client.input(b"first client");
    assert!(client.wait(true).is_empty());
    assert_eq!(server.wait(true), b"first client");
}

#[test]
fn native_pipe_refuses_code_targets() {
    let mut client = Peer::spawn(&["pipe", "connect", "code:a-b-c-d-e-f"]);
    client.wait(false);
    assert!(
        client
            .errors()
            .contains("tunnel and pipe connect to paired machines only"),
        "{}",
        client.errors()
    );
}

#[test]
fn native_pipe_bidirectional_payload() {
    let pair = slots::Pair::direct();
    let mut server = pair.serve(&["pipe", "serve", "--allow", "all"]);
    let mut client = pair.connect(&server, &["pipe", "connect", "peer:nas"]);
    let mut left_input = server.child.stdin.take().unwrap();
    let mut right_input = client.child.stdin.take().unwrap();
    let left = vec![0u8; 500000];
    let right = vec![255u8; 500000];
    let left_writer = thread::spawn(move || left_input.write_all(&vec![0u8; 500000]).unwrap());
    let right_writer = thread::spawn(move || right_input.write_all(&vec![255u8; 500000]).unwrap());
    assert_eq!(client.wait(true), left);
    assert_eq!(server.wait(true), right);
    left_writer.join().unwrap();
    right_writer.join().unwrap();
}

#[test]
fn native_tcp_tunnel_multiple_connections() {
    let service = TcpListener::bind("127.0.0.1:0").unwrap();
    let service_addr = service.local_addr().unwrap().to_string();
    let echo = thread::spawn(move || {
        let mut tasks = vec![];
        for _ in 0..8 {
            let (mut conn, _) = service.accept().unwrap();
            tasks.push(thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut bytes = vec![];
                conn.read_to_end(&mut bytes).unwrap();
                conn.write_all(&bytes).unwrap();
            }));
        }
        for task in tasks {
            task.join().unwrap();
        }
    });
    let pair = slots::Pair::direct();
    let server = pair.serve(&[
        "tunnel",
        "tcp",
        "serve",
        "--allow",
        "all",
        "--to",
        &service_addr,
    ]);
    let client = pair.connect(
        &server,
        &[
            "tunnel",
            "tcp",
            "connect",
            "peer:nas",
            "--bind",
            "127.0.0.1:0",
        ],
    );
    let bound = client.line(true);
    client.until_error_line("peer and destination service unchecked");
    assert!(!client.errors().contains("peer connected"));
    let bound = bound.strip_prefix("TCP listening on ").unwrap();
    thread::scope(|scope| {
        for n in 0..8 {
            scope.spawn(move || {
                let data = vec![n as u8; 262144];
                let mut conn = TcpStream::connect(bound).unwrap();
                conn.set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                conn.write_all(&data).unwrap();
                conn.shutdown(Shutdown::Write).unwrap();
                let mut bytes = vec![];
                conn.read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, data);
            });
        }
    });
    echo.join().unwrap();
}

#[test]
fn native_tcp_tunnel_refuses_connections_beyond_the_limit() {
    let service = TcpListener::bind("127.0.0.1:0").unwrap();
    let service_addr = service.local_addr().unwrap().to_string();
    // Echo service: each accepted connection echoes until EOF.
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
    let pair = slots::Pair::direct();
    let server = pair.serve(&[
        "tunnel",
        "tcp",
        "serve",
        "--allow",
        "all",
        "--to",
        &service_addr,
        "--max-connections",
        "1",
    ]);
    let client = pair.connect(
        &server,
        &[
            "tunnel",
            "tcp",
            "connect",
            "peer:nas",
            "--bind",
            "127.0.0.1:0",
        ],
    );
    let bound = client.line(true);
    let bound = bound.strip_prefix("TCP listening on ").unwrap().to_owned();
    let echo = |conn: &mut TcpStream| {
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        conn.write_all(b"ping").unwrap();
        let mut reply = [0u8; 4];
        conn.read_exact(&mut reply).map(|()| reply)
    };
    // The first connection holds the only slot.
    let mut first = TcpStream::connect(&bound).unwrap();
    assert_eq!(&echo(&mut first).unwrap(), b"ping");
    // A second concurrent connection is refused before any data flows.
    let mut second = TcpStream::connect(&bound).unwrap();
    assert!(echo(&mut second).is_err());
    server.until_error_line("refusing new connections: 1 already open");
    // Once the first closes, the slot is available again.
    first.shutdown(Shutdown::Both).unwrap();
    drop(first);
    let start = Instant::now();
    loop {
        let mut third = TcpStream::connect(&bound).unwrap();
        if echo(&mut third).is_ok() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "slot never freed"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn native_tcp_tunnel_http_response() {
    tcp_http_response(&slots::Pair::direct());
}

fn tcp_http_response(pair: &slots::Pair) {
    let service = TcpListener::bind("127.0.0.1:0").unwrap();
    let service_addr = service.local_addr().unwrap().to_string();
    let http = thread::spawn(move || {
        service.set_nonblocking(true).unwrap();
        let started = Instant::now();
        let mut conn = loop {
            match service.accept() {
                Ok((conn, _)) => break conn,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(started.elapsed() < Duration::from_secs(15));
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("HTTP accept failed: {error}"),
            }
        };
        // macOS sockets inherit the listener's non-blocking mode.
        conn.set_nonblocking(false).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        let mut reader = BufReader::new(&mut conn);
        while !request.ends_with(b"\r\n\r\n") {
            assert!(reader.read_until(b'\n', &mut request).unwrap() > 0);
            assert!(request.len() <= 8192);
        }
        assert_eq!(request, b"GET /p0 HTTP/1.0\r\n\r\n");
        conn.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 6\r\n\r\np0-ok\n")
            .unwrap();
    });
    let server = pair.serve(&[
        "tunnel",
        "tcp",
        "serve",
        "--allow",
        "all",
        "--to",
        &service_addr,
        "--verbose",
    ]);
    server.until_error_line("tunnel tcp connect peer:nas ");
    let client = pair.connect(
        &server,
        &[
            "tunnel",
            "tcp",
            "connect",
            "peer:nas",
            "--bind",
            "127.0.0.1:0",
        ],
    );
    let bound = client.line(true);
    let mut conn = TcpStream::connect(bound.strip_prefix("TCP listening on ").unwrap()).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    conn.write_all(b"GET /p0 HTTP/1.0\r\n\r\n").unwrap();
    conn.shutdown(Shutdown::Write).unwrap();
    let mut response = String::new();
    conn.read_to_string(&mut response).unwrap();
    assert_eq!(
        response,
        "HTTP/1.0 200 OK\r\nContent-Length: 6\r\n\r\np0-ok\n"
    );
    http.join().unwrap();
    server.until_error_line("destination TCP service connected");
}

#[test]
fn native_unreachable_peer_by_name_has_a_bounded_diagnostic() {
    let pair = slots::Pair::direct();
    let mut server = pair.serve(&["ping", "serve", "--slot"]);
    server.child.kill().unwrap();
    server.wait(false);
    let mut client = pair.connect(
        &server,
        &[
            "--timeout",
            "0.2",
            "ping",
            "connect",
            "peer:nas",
            "--count",
            "1",
        ],
    );
    assert!(client.wait(false).is_empty());
    assert!(
        client
            .errors()
            .contains("cannot establish an authenticated connection to slot ping on nas"),
        "{}",
        client.errors()
    );
}

#[test]
fn native_unreachable_peer_has_a_bounded_diagnostic() {
    let mut server = Peer::spawn(&["ping", "serve"]);
    let ticket = server.line(false);
    server.child.kill().unwrap();
    server.wait(false);
    let mut client = Peer::spawn(&[
        "--timeout",
        "0.2",
        "ping",
        "connect",
        &ticket,
        "--count",
        "1",
    ]);
    assert!(client.wait(false).is_empty());
    assert!(
        client.errors().contains("peer connection timed out")
            || client.errors().contains("cannot connect to peer"),
        "{}",
        client.errors()
    );
}

#[test]
fn native_tcp_tunnel_reports_unavailable_destination() {
    let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = unavailable.local_addr().unwrap().to_string();
    drop(unavailable);
    let pair = slots::Pair::direct();
    let mut server = pair.serve(&["tunnel", "tcp", "serve", "--allow", "all", "--to", &address]);
    let client = pair.connect(
        &server,
        &[
            "tunnel",
            "tcp",
            "connect",
            "peer:nas",
            "--bind",
            "127.0.0.1:0",
        ],
    );
    let bound = client.line(true);
    let mut conn = TcpStream::connect(bound.strip_prefix("TCP listening on ").unwrap()).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    conn.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut response = Vec::new();
    let _ = conn.read_to_end(&mut response);
    assert!(response.is_empty());
    assert!(
        server
            .until_error_line("cannot connect to destination TCP service")
            .contains("cannot connect to destination TCP service"),
        "{}",
        server.errors()
    );
    client.until_error_line("destination TCP service unavailable");
    assert!(
        !server
            .errors()
            .contains("destination TCP service connected")
    );
    assert!(server.child.try_wait().unwrap().is_none());
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:relay with IROH_TEST_RELAY_BINARY set"]
fn official_private_relay_registration_ping_blobs_and_denials() {
    let binary = std::env::var_os("IROH_TEST_RELAY_BINARY")
        .filter(|value| !value.is_empty())
        .expect("IROH_TEST_RELAY_BINARY must point to an official iroh-relay binary; private relay was NOT tested");
    let temp = tempfile::tempdir().unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}");
    let server_config = temp.path().join("relay.toml");
    std::fs::write(&server_config, format!("http_bind_addr = \"127.0.0.1:{port}\"\naccess.shared_token = [\"test-relay-credential\"]\nenable_quic_addr_discovery = false\nenable_metrics = false\n")).unwrap();
    let mut command = Command::new(binary);
    command
        .args(["--dev", "--config-path"])
        .arg(&server_config)
        .env("RUST_LOG", "off");
    let relay = Peer::start(command);
    let start = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "relay did not start: {}",
            relay.errors()
        );
        thread::sleep(Duration::from_millis(25));
    }
    let client_config = temp.path().join("client.toml");
    std::fs::write(
        &client_config,
        format!("[relay]\nurl = {url:?}\n[access]\nshared_token = \"test-relay-credential\"\n"),
    )
    .unwrap();
    let config = client_config.to_str().unwrap();
    let run = |args: &[&str]| {
        let mut all = vec!["--config", config, "--timeout", "3", "--relay-only"];
        all.extend_from_slice(args);
        Peer::spawn(&all)
    };
    let pair = slots::Pair::relayed(
        ["--config", config, "--timeout", "3", "--relay-only"]
            .map(String::from)
            .to_vec(),
    );
    let mut check = run(&["relay", "check", "--json"]);
    let online: serde_json::Value = serde_json::from_slice(&check.wait(true)).unwrap();
    assert_eq!(online["status"], "online");
    let listener = run(&["ping", "serve"]);
    let ticket = listener.line(false);
    let mut ping = run(&["ping", "connect", &ticket, "--count", "1", "--json"]);
    let reply: serde_json::Value = serde_json::from_slice(&ping.wait(true)).unwrap();
    assert_eq!(reply["route"], "relay");
    assert_eq!(reply["carrier"], "WebSocket/TCP");
    tcp_http_response(&pair);
    ports_replacement::multiport(&pair);
    pairing::chat::conversation(run, temp.path());
    let source = temp.path().join("private-folder");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    let data = vec![42u8; 1048576];
    std::fs::write(source.join("nested/private.bin"), &data).unwrap();
    std::fs::write(source.join("zero"), []).unwrap();
    let mut sender = run(&["send", source.to_str().unwrap(), "--no-verify"]);
    let code = sender.line(false);
    assert!(!code.contains("test-relay-credential"));
    let output = temp.path().join("received");
    let mut receiver = run(&[
        "receive",
        &format!("code:{code}"),
        "--no-verify",
        "--output",
        output.to_str().unwrap(),
    ]);
    receiver.wait(true);
    assert!(receiver.errors().contains("BLAKE3 verified"));
    sender.wait(true);
    // The copied command carries the relay URL, never this machine's token.
    assert!(sender.errors().contains(&format!(
        "iroh-toolbox --relay {url}/ --relay-only receive code:"
    )));
    assert!(
        sender
            .errors()
            .contains("Configure your own access token for this relay")
    );
    assert!(!sender.errors().contains("test-relay-credential"));
    assert_eq!(
        std::fs::read(output.join("private-folder/nested/private.bin")).unwrap(),
        data
    );
    assert_eq!(
        std::fs::metadata(output.join("private-folder/zero"))
            .unwrap()
            .len(),
        0
    );
    for token in [None, Some("invalid-test-credential")] {
        let bad_config = temp.path().join("denied.toml");
        let mut source = format!("[relay]\nurl = {url:?}\n");
        if let Some(token) = token {
            source += &format!("[access]\nshared_token = {token:?}\n");
        }
        std::fs::write(&bad_config, source).unwrap();
        let mut denied = Peer::spawn(&[
            "--config",
            bad_config.to_str().unwrap(),
            "--timeout",
            "3",
            "--relay-only",
            "relay",
            "check",
        ]);
        denied.wait(false);
        assert!(
            denied.errors().contains("authentication denied"),
            "{}",
            denied.errors()
        );
        assert!(!denied.errors().contains("invalid-test-credential"));
    }
}
