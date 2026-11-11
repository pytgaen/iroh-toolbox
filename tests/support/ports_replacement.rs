use super::*;

fn exchange(address: &str, payload: &[u8]) -> Vec<u8> {
    let mut conn = TcpStream::connect(address).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    conn.write_all(payload).unwrap();
    conn.shutdown(Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    let _ = conn.read_to_end(&mut bytes);
    bytes
}

/// A port that was free a moment ago.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The local address of a `TCP listening on ADDRESS -> service NAME` line.
fn listening(client: &Peer, service: &str) -> String {
    let line = client.line(true);
    let address = line.strip_prefix("TCP listening on ").unwrap();
    let (address, name) = address.split_once(" -> service ").unwrap();
    assert_eq!(name, service, "{line}");
    address.to_owned()
}

pub(super) fn multiport(pair: &slots::Pair) {
    let mut services = Vec::new();
    let mut echoes = Vec::new();
    for (label, connections) in [(b"one:".as_slice(), 5), (b"two:", 4)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        services.push(listener.local_addr().unwrap().to_string());
        echoes.push(thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            for _ in 0..connections {
                let start = Instant::now();
                let (mut conn, _) = loop {
                    match listener.accept() {
                        Ok(conn) => break conn,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(start.elapsed() < Duration::from_secs(15));
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                // macOS sockets inherit the listener's non-blocking mode.
                conn.set_nonblocking(false).unwrap();
                conn.set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut bytes = Vec::new();
                conn.read_to_end(&mut bytes).unwrap();
                conn.write_all(label).unwrap();
                conn.write_all(&bytes).unwrap();
            }
        }));
    }
    // A numeric name sets the client port: a free one, so the group can use it.
    let numbered = free_port();
    let mut server = pair.serve(&[
        "tunnel",
        "tcp",
        "serve",
        "--allow",
        "all",
        "--to",
        &format!("one={}", services[0]),
        "--to",
        &format!("two={}", services[1]),
        "--to",
        &format!("{numbered}={}", services[0]),
    ]);
    server.until_error_line(&format!(
        "TCP service {numbered} -> {} (client port {numbered})",
        services[0]
    ));
    let mut group = pair.connect(
        &server,
        &[
            "tunnel",
            "tcp",
            "connect",
            "peer:nas",
            "--bind",
            "one=127.0.0.1:0",
            "--bind",
            "two=127.0.0.1:0",
        ],
    );
    let bound = [listening(&group, "one"), listening(&group, "two")];
    assert_eq!(
        listening(&group, &numbered.to_string()),
        format!("127.0.0.1:{numbered}")
    );
    group.until_error_line("tunnel: 3 services of nas/tunnel ready");
    thread::scope(|scope| {
        for n in 0..8 {
            let address = &bound[n % 2];
            scope.spawn(move || {
                let payload = vec![n as u8; 32768];
                let mut expected = if n % 2 == 0 {
                    b"one:".to_vec()
                } else {
                    b"two:".to_vec()
                };
                expected.extend_from_slice(&payload);
                assert_eq!(exchange(address, &payload), expected);
            });
        }
    });
    assert_eq!(
        exchange(&format!("127.0.0.1:{numbered}"), b"third"),
        b"one:third"
    );
    for echo in echoes {
        echo.join().unwrap();
    }
    assert!(server.child.try_wait().unwrap().is_none());
    assert!(group.child.try_wait().unwrap().is_none());
    group.child.kill().unwrap();
    group.wait(false);

    // Lazy clients learn a missing or unnamed service at their first connection.
    let unknown = pair.connect(
        &server,
        &[
            "tunnel",
            "tcp",
            "connect",
            "peer:nas/tunnel/three",
            "--bind",
            "127.0.0.1:0",
        ],
    );
    let address = listening(&unknown, "three");
    assert!(exchange(&address, b"not offered").is_empty());
    unknown.until_error_line("slot tunnel of nas has no service \"three\"; available: one, two, ");
    let unnamed = pair.connect(
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
    let line = unnamed.line(true);
    assert!(exchange(line.strip_prefix("TCP listening on ").unwrap(), b"which").is_empty());
    unnamed.until_error_line("nas/tunnel serves several services (one, two, ");
    server.until_error_line("a peer requested a service this slot does not serve");

    // A listing client refuses an unknown service before binding anything.
    let mut listed = pair.connect(
        &server,
        &["tunnel", "tcp", "connect", "peer:nas/tunnel/three"],
    );
    listed.wait(false);
    assert!(
        listed
            .errors()
            .contains("slot tunnel of nas has no service \"three\"; available: one, two, "),
        "{}",
        listed.errors()
    );
    assert!(!listed.errors().contains("TCP listening"));
    let mut old = pair.connect(
        &server,
        &[
            "tunnel",
            "tcp",
            "connect",
            "peer:nas",
            "--bind",
            "127.0.0.1:0=3000",
        ],
    );
    old.wait(false);
    assert!(
        old.errors().contains("write --bind 3000=127.0.0.1:0"),
        "{}",
        old.errors()
    );

    // Failing to bind the second service releases the first and prints no readiness.
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let free_address = format!("127.0.0.1:{}", free_port());
    let mut failed = pair.connect(
        &server,
        &[
            "tunnel",
            "tcp",
            "connect",
            "peer:nas",
            "--bind",
            &format!("one={free_address}"),
            "--bind",
            &format!("two={}", reserved.local_addr().unwrap()),
        ],
    );
    failed.wait(false);
    assert!(!failed.errors().contains("TCP listening"));
    assert!(
        failed
            .errors()
            .contains("for service two; pass --bind two=127.0.0.1:PORT"),
        "{}",
        failed.errors()
    );
    assert!(TcpListener::bind(free_address).is_ok());
}

#[test]
fn native_tcp_multiple_ports_share_one_slot() {
    multiport(&slots::Pair::direct());
}

#[cfg(unix)]
#[test]
#[ignore = "requires an official iroh-relay binary, Python and Unix PTYs; run explicitly for native file replacement confirmation"]
fn native_replacement_confirmation_on_real_terminals() {
    // The sender's code is the only way in, so the terminals need a relay.
    let relay = LocalRelay::start(None);
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/replacement.py"
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
