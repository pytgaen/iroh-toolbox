//! Native `send` and `receive` paired by code through a local official relay.
use super::*;

/// Directories that `receive` left next to `output`.
fn partial_directories(parent: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".iroh-")
        })
        .collect()
}

/// A sender slowed down so a test can act while it is serving (debug builds).
fn slow_sender(relay: &LocalRelay, source: &Path, extra: &[&str]) -> Peer {
    let mut command = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"));
    command
        .args([
            "--relay",
            &relay.url,
            "-v",
            "send",
            source.to_str().unwrap(),
        ])
        .args(["--no-progress", "--no-verify"])
        .args(extra)
        .env("IROH_TOOLBOX_TEST_SERVE_DELAY_MS", "4");
    Peer::start(command)
}

fn blob_transfer(
    relay: &LocalRelay,
    source: &Path,
    output: &Path,
    max_bytes: Option<&str>,
    success: bool,
) {
    // Raw served byte counts are -vv diagnostics.
    let mut server = relay.spawn(&["send", source.to_str().unwrap(), "-vv", "--no-verify"]);
    let code = server.line(false);
    assert_eq!(code.split('-').count(), 6, "{code}");
    let joined = format!("code:{code}");
    let mut args = vec![
        "receive",
        &joined,
        "--no-verify",
        "--output",
        output.to_str().unwrap(),
    ];
    if let Some(limit) = max_bytes {
        args.extend(["--max-bytes", limit]);
    }
    let mut client = relay.spawn(&args);
    assert!(client.wait(success).is_empty());
    if success {
        assert!(client.errors().contains("BLAKE3 verified"));
        assert_eq!(server.wait(true), format!("{code}\n").as_bytes());
        assert!(server.errors().contains(&format!(
            "iroh-toolbox --relay {}/ receive code: --no-verify --output ./received",
            relay.url
        )));
        assert!(server.errors().contains("waiting for the receiver"));
        assert!(server.errors().contains("send: sending"));
        assert!(server.errors().contains("transfer finished"));
        assert!(!server.errors().contains("BLAKE3 verified"));
        let logs = server.errors();
        let final_count = logs
            .lines()
            .rev()
            .find(|line| line.starts_with("send: sending") && line.contains(" bytes ("))
            .unwrap();
        let served: u64 = final_count
            .split_whitespace()
            .find_map(|word| word.split_once('/'))
            .unwrap()
            .0
            .parse()
            .unwrap();
        assert!(served > 0, "{logs}");
        // A size probe reads a trailing chunk; its offset must not be counted
        // as another complete copy of the large fixture.
        if source.is_dir() {
            assert!(served < 2 * 1048576, "{logs}");
        }
        // Redirected default output lists necessary steps once, without counters.
        assert!(!client.errors().contains("100%"), "{}", client.errors());
        for step in ["connecting to sender", "receiving and verifying BLAKE3"] {
            assert!(client.errors().contains(step), "{}", client.errors());
        }
        for phase in ["registering endpoint", "publishing verified files"] {
            assert!(!client.errors().contains(phase), "{}", client.errors());
        }
    } else {
        let expected = if max_bytes.is_some() {
            "transfer exceeds --max-bytes or metadata limit"
        } else {
            "destination already exists"
        };
        assert!(client.errors().contains(expected), "{}", client.errors());
        assert!(!client.errors().contains("BLAKE3 verified"));
    }
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_blobs_file_directory_limits_and_no_overwrite() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("folder");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    let data: Vec<u8> = (0..1048576).map(|v| (v % 251) as u8).collect();
    std::fs::write(source.join("nested/data.bin"), &data).unwrap();
    std::fs::write(source.join("zero"), []).unwrap();
    let output = temp.path().join("received");
    blob_transfer(&relay, &source, &output, None, true);
    assert_eq!(
        std::fs::read(output.join("folder/nested/data.bin")).unwrap(),
        data
    );
    assert_eq!(
        std::fs::metadata(output.join("folder/zero")).unwrap().len(),
        0
    );
    blob_transfer(&relay, &source, &output, None, false);
    assert_eq!(
        std::fs::read(output.join("folder/nested/data.bin")).unwrap(),
        data
    );
    let limited = temp.path().join("limited");
    blob_transfer(&relay, &source, &limited, Some("1"), false);
    assert!(!limited.exists());
    let file_output = temp.path().join("file-received");
    blob_transfer(&relay, &source.join("zero"), &file_output, None, true);
    assert_eq!(
        std::fs::metadata(file_output.join("zero")).unwrap().len(),
        0
    );
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_receive_resumes_after_an_interrupted_receiver() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("data.bin");
    let data: Vec<u8> = (0..8 * 1048576u32).map(|v| (v % 253) as u8).collect();
    std::fs::write(&source, &data).unwrap();
    let output = temp.path().join("received");
    let mut sender = slow_sender(&relay, &source, &[]);
    let code = sender.line(false);
    let joined = format!("code:{code}");
    let receive = [
        "receive",
        joined.as_str(),
        "--no-verify",
        "--output",
        output.to_str().unwrap(),
    ];
    for interrupt in ["crash", "ctrl-c"] {
        if interrupt == "ctrl-c" && cfg!(not(unix)) {
            continue;
        }
        let mut receiver = relay.spawn(&receive);
        receiver.until_error_line("receiving and verifying BLAKE3");
        thread::sleep(Duration::from_millis(300));
        if interrupt == "crash" {
            receiver.child.kill().unwrap();
            receiver.child.wait().unwrap();
        } else {
            let pid = receiver.child.id().to_string();
            assert!(
                Command::new("kill")
                    .args(["-INT", &pid])
                    .status()
                    .unwrap()
                    .success()
            );
            receiver.until_error_line("partial transfer kept in");
            assert_eq!(receiver.child.wait().unwrap().code(), Some(130));
        }
        // The partial directory survives next to the destination, which stays untouched.
        assert!(!output.exists());
        assert_eq!(partial_directories(temp.path()).len(), 1);
    }
    // The sender stopped serving the rendezvous after pairing: the resumed
    // reception cannot pair the code again, it reuses its saved identity.
    // Resuming does not pair, so it asks for no confirmation number: without
    // --no-verify and without a terminal it still works.
    let resume = [
        "receive",
        joined.as_str(),
        "--output",
        output.to_str().unwrap(),
    ];
    let mut resumed = relay.spawn(&resume);
    resumed.wait(true);
    let errors = resumed.errors();
    assert!(errors.contains("resuming a partial transfer"), "{errors}");
    assert!(errors.contains("BLAKE3 verified"), "{errors}");
    assert_eq!(std::fs::read(output.join("data.bin")).unwrap(), data);
    assert!(partial_directories(temp.path()).is_empty());
    sender.wait(true);
    assert!(
        sender.errors().contains("waiting for it to resume")
            || sender.errors().contains("receiver reconnected")
    );
    // Once the sender has stopped, the same code finds nobody and leaves no partial directory.
    std::fs::remove_dir_all(&output).unwrap();
    let mut late = relay.spawn(&[
        "receive",
        &joined,
        "--no-verify",
        "--pairing-timeout",
        "2",
        "--output",
        output.to_str().unwrap(),
    ]);
    late.wait(false);
    assert!(
        late.errors().contains("no machine answered"),
        "{}",
        late.errors()
    );
    assert!(partial_directories(temp.path()).is_empty());
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_sender_serves_only_the_paired_receiver() {
    use iroh::{
        Endpoint, RelayMode,
        endpoint::{ConnectionError, presets},
    };
    use iroh_blobs::ticket::BlobTicket;

    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("data.bin");
    let data: Vec<u8> = (0..16 * 1048576u32).map(|v| (v % 251) as u8).collect();
    std::fs::write(&source, &data).unwrap();
    let output = temp.path().join("received");
    let mut sender = slow_sender(&relay, &source, &[]);
    let code = sender.line(false);
    let mut receiver = relay.spawn(&[
        "receive",
        &format!("code:{code}"),
        "--no-verify",
        "--output",
        output.to_str().unwrap(),
    ]);
    receiver.until_error_line("receiving and verifying BLAKE3");
    // The sender's address is known to the paired receiver only: read it from
    // its saved session, as a stranger who learned the endpoint ID would use it.
    let session = partial_directories(temp.path())[0].join("session.json");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(session).unwrap()).unwrap();
    let ticket: BlobTicket = saved["ticket"].as_str().unwrap().parse().unwrap();
    let stranger = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let ep = Endpoint::builder(presets::Minimal)
                .clear_address_lookup()
                .relay_mode(RelayMode::Disabled)
                .bind()
                .await
                .unwrap();
            let conn = tokio::time::timeout(
                Duration::from_secs(10),
                ep.connect(ticket.addr().clone(), iroh_blobs::ALPN),
            )
            .await
            .expect("stranger could not reach the sender")
            .unwrap();
            let closed = tokio::time::timeout(Duration::from_secs(10), conn.closed())
                .await
                .expect("stranger kept a connection to the sender");
            ep.close().await;
            closed
        });
    match stranger {
        ConnectionError::ApplicationClosed(close) => {
            assert_eq!(u64::from(close.error_code), 1, "{close}")
        }
        other => panic!("stranger was not refused by the sender: {other}"),
    }
    sender.until_error_line("not the paired receiver");
    // The stranger neither received data nor blocked the paired receiver.
    receiver.wait(true);
    assert_eq!(std::fs::read(output.join("data.bin")).unwrap(), data);
    sender.wait(true);
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_sender_times_out_when_the_paired_receiver_disappears() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("file");
    std::fs::write(&source, vec![5u8; 8 * 1048576]).unwrap();
    let mut sender = slow_sender(&relay, &source, &["--transfer-timeout", "2"]);
    let code = sender.line(false);
    let mut receiver = relay.spawn(&[
        "receive",
        &format!("code:{code}"),
        "--no-verify",
        "--output",
        temp.path().join("received").to_str().unwrap(),
    ]);
    receiver.until_error_line("receiving and verifying BLAKE3");
    receiver.child.kill().unwrap();
    receiver.child.wait().unwrap();
    sender.wait(false);
    assert!(
        sender.errors().contains("file transfer timed out"),
        "{}",
        sender.errors()
    );
    assert_eq!(std::fs::read(&source).unwrap().len(), 8 * 1048576);
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_receive_stalled_never_publishes() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.bin");
    std::fs::write(&source, vec![3u8; 8 * 1048576]).unwrap();
    let output = temp.path().join("received");
    std::fs::create_dir(&output).unwrap();
    std::fs::write(output.join("existing"), b"keep").unwrap();
    let sender = slow_sender(&relay, &source, &[]);
    let code = sender.line(false);
    let mut receiver = relay.spawn(&[
        "receive",
        &format!("code:{code}"),
        "--no-verify",
        "--output",
        output.to_str().unwrap(),
        "--transfer-timeout",
        "1",
    ]);
    receiver.wait(false);
    assert!(
        receiver.errors().contains("file transfer timed out"),
        "{}",
        receiver.errors()
    );
    assert!(!receiver.errors().contains("BLAKE3 verified"));
    // Nothing was published; the verified part stays in a partial directory to resume.
    assert_eq!(std::fs::read(output.join("existing")).unwrap(), b"keep");
    assert_eq!(std::fs::read_dir(&output).unwrap().count(), 2);
    assert_eq!(partial_directories(&output).len(), 1);
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_receive_reconnects_after_the_sender_closes_the_connection() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.bin");
    let data: Vec<u8> = (0..8 * 1048576u32).map(|v| (v % 249) as u8).collect();
    std::fs::write(&source, &data).unwrap();
    let output = temp.path().join("received");
    // The sender closes its first connection midway, with a reason that is not
    // a goodbye: the receiver reconnects with its saved identity, never pairing again.
    let mut command = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"));
    command
        .args(["--relay", &relay.url, "send", source.to_str().unwrap()])
        .args(["--no-progress", "--no-verify"])
        .env("IROH_TOOLBOX_TEST_SERVE_DELAY_MS", "4")
        .env("IROH_TOOLBOX_TEST_SERVE_CUT_BYTES", "2097152");
    let mut sender = Peer::start(command);
    let code = sender.line(false);
    let mut receiver = relay.spawn(&[
        "receive",
        &format!("code:{code}"),
        "--no-verify",
        "--output",
        output.to_str().unwrap(),
    ]);
    receiver.wait(true);
    let errors = receiver.errors();
    assert!(errors.contains("reconnecting to sender"), "{errors}");
    assert!(errors.contains("BLAKE3 verified"), "{errors}");
    assert_eq!(std::fs::read(output.join("source.bin")).unwrap(), data);
    assert!(partial_directories(temp.path()).is_empty());
    sender.wait(true);
    assert!(
        sender.errors().contains("receiver reconnected"),
        "{}",
        sender.errors()
    );
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_receive_refuses_existing_names_before_downloading_contents() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("big-folder");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("data.bin"), vec![7u8; 32 * 1024 * 1024]).unwrap();
    let output = temp.path().join("received");
    std::fs::create_dir_all(output.join("big-folder")).unwrap();
    let mut sender = relay.spawn(&["send", source.to_str().unwrap(), "--no-verify"]);
    let code = sender.line(false);
    let mut receiver = relay.spawn(&[
        "receive",
        &format!("code:{code}"),
        "--no-verify",
        "--output",
        output.to_str().unwrap(),
    ]);
    receiver.wait(false);
    let errors = receiver.errors();
    assert!(errors.contains("destination already exists"), "{errors}");
    assert!(errors.contains("big-folder"), "{errors}");
    // Refused before the payload step: contents were never downloaded.
    assert!(!errors.contains("receiving and verifying"), "{errors}");
    assert!(
        std::fs::read_dir(output.join("big-folder"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(no_temporary_directories(&output));
    // The sender only sees the connection close; it does not claim publication.
    let _ = sender.child.kill();
    let _ = sender.child.wait();
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_blob_quiet_preserves_script_outputs_and_final_result() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("quiet.bin");
    std::fs::write(&source, b"quiet payload").unwrap();
    let mut sender = relay.spawn(&[
        "--no-hints",
        "send",
        source.to_str().unwrap(),
        "--no-progress",
        "--no-verify",
    ]);
    let code = sender.line(false);
    let output = temp.path().join("received");
    let mut receiver = relay.spawn(&[
        "receive",
        &format!("code:{code}"),
        "--no-verify",
        "--output",
        output.to_str().unwrap(),
        "--max-bytes",
        "1KiB",
        "--no-progress",
    ]);
    assert!(receiver.wait(true).is_empty());
    assert_eq!(sender.wait(true), format!("{code}\n").as_bytes());
    assert!(!sender.errors().contains("send:"));
    assert!(!sender.errors().contains("iroh-toolbox receive"));
    assert!(!receiver.errors().contains("receive:"));
    assert!(receiver.errors().contains("BLAKE3 verified"));
    assert_eq!(
        std::fs::read(output.join("quiet.bin")).unwrap(),
        b"quiet payload"
    );
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_receive_forced_replacement_and_noninteractive_confirmation() {
    let relay = LocalRelay::start(None);
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("folder");
    let output = root.path().join("received");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir_all(output.join("folder")).unwrap();
    std::fs::write(source.join("file"), b"verified replacement").unwrap();
    std::fs::write(output.join("folder/file"), b"old").unwrap();
    std::fs::write(output.join("folder/unrelated"), b"keep").unwrap();
    // Without a terminal the default question cannot be asked, even with "yes" on stdin.
    for (flag, max_bytes, success, expected, error) in [
        (
            Some("--force"),
            "1",
            false,
            b"old".as_slice(),
            "--max-bytes",
        ),
        (
            None,
            "10GiB",
            false,
            b"old".as_slice(),
            "interactive terminal",
        ),
        (
            Some("--no-replace"),
            "10GiB",
            false,
            b"old".as_slice(),
            "omit --no-replace",
        ),
        (
            Some("--force"),
            "10GiB",
            true,
            b"verified replacement".as_slice(),
            "",
        ),
    ] {
        let mut sender = relay.spawn(&[
            "send",
            source.to_str().unwrap(),
            "--no-progress",
            "--no-verify",
        ]);
        let code = sender.line(false);
        let joined = format!("code:{code}");
        let mut args = vec![
            "receive",
            &joined,
            "--no-verify",
            "--output",
            output.to_str().unwrap(),
        ];
        args.extend(flag);
        args.extend(["--max-bytes", max_bytes, "--no-progress"]);
        let mut receiver = relay.spawn(&args);
        receiver.input(b"yes\n");
        receiver.wait(success);
        if success {
            sender.wait(true);
        } else {
            assert!(receiver.errors().contains(error), "{}", receiver.errors());
        }
        assert_eq!(std::fs::read(output.join("folder/file")).unwrap(), expected);
        assert_eq!(
            std::fs::read(output.join("folder/unrelated")).unwrap(),
            b"keep"
        );
        assert!(no_temporary_directories(&output));
    }
}
