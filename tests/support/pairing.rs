//! Code pairing between native clients through a local official relay; no
//! public relay, DNS or mailbox is contacted.
use super::*;

#[path = "pairing_chat.rs"]
pub(crate) mod chat;
#[path = "pairing_files.rs"]
mod files;

fn no_temporary_directories(path: &Path) -> bool {
    std::fs::read_dir(path).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".iroh-")
    })
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_transfer_replacement_reuse_wrong_code_and_expiry() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("folder");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("nested/file"), b"paired payload").unwrap();
    std::fs::write(source.join("empty"), []).unwrap();
    let output = temp.path().join("received");
    for (succeeds, force) in [(true, false), (false, false), (true, true)] {
        if force {
            std::fs::write(output.join("folder/nested/file"), b"old destination").unwrap();
        }
        let mut sender = relay.spawn(&["-v", "send", source.to_str().unwrap(), "--no-verify"]);
        let code = sender.line(false);
        assert_eq!(code.split('-').count(), 6, "{code}");
        // People read the code on stderr; stdout carries it alone for scripts.
        sender.until_error_line(&format!("Code: {code}"));
        let joined = format!("code:{code}");
        let mut receiver_args = vec![
            "receive",
            &joined,
            "--no-verify",
            "--output",
            output.to_str().unwrap(),
        ];
        if force {
            receiver_args.push("--force");
        }
        let mut receiver = relay.spawn(&receiver_args);
        assert!(receiver.wait(succeeds).is_empty());
        if succeeds {
            sender.wait(true);
        } // On publication failure the sender only knows that the connection closed.
        assert_eq!(
            std::fs::read(output.join("folder/nested/file")).unwrap(),
            b"paired payload"
        );
        assert_eq!(
            std::fs::metadata(output.join("folder/empty"))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            &*sender.stdout.lock().unwrap(),
            format!("{code}\n").as_bytes()
        );
        assert!(!sender.errors().contains("iroh-toolbox receive \"blob"));
        // The sender stopped serving the rendezvous: the code cannot be reused.
        let mut reused = relay.spawn(&[
            "receive",
            &format!("code:{code}"),
            "--no-verify",
            "--pairing-timeout",
            "2",
            "--output",
            temp.path().to_str().unwrap(),
        ]);
        reused.wait(false);
        assert!(
            reused.errors().contains("no machine answered"),
            "{}",
            reused.errors()
        );
    }
    // A wrong password fails on both sides without exposing the ticket.
    let mut sender = relay.spawn(&[
        "send",
        source.to_str().unwrap(),
        "--code-words",
        "8",
        "--no-verify",
    ]);
    let code = sender.line(false);
    assert_eq!(code.split('-').count(), 8);
    let rendezvous: Vec<&str> = code.split('-').take(3).collect();
    let wrong = format!(
        "{}-abacus-abacus-abacus-abacus-abacus",
        rendezvous.join("-")
    );
    let mut receiver = relay.spawn(&[
        "receive",
        &format!("code:{wrong}"),
        "--no-verify",
        "--output",
        temp.path().to_str().unwrap(),
    ]);
    receiver.wait(false);
    assert_eq!(sender.wait(false), format!("{code}\n").as_bytes());
    for log in [sender.errors(), receiver.errors()] {
        assert!(log.contains("wrong code"), "{log}");
        assert!(!log.contains("blob"), "{log}");
    }
    // An expired code leaves no published file or temporary directory.
    let mut expired = relay.spawn(&[
        "send",
        source.to_str().unwrap(),
        "--pairing-timeout",
        "1",
        "--no-verify",
    ]);
    let code = expired.line(false);
    expired.wait(false);
    assert!(expired.errors().contains("pairing timed out"));
    let mut receiver = relay.spawn(&[
        "receive",
        &format!("code:{code}"),
        "--no-verify",
        "--pairing-timeout",
        "2",
        "--output",
        temp.path().to_str().unwrap(),
    ]);
    receiver.wait(false);
    assert!(!receiver.errors().contains("BLAKE3 verified"));
    assert!(no_temporary_directories(temp.path()));
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_transfer_through_private_relay_keeps_credentials_local() {
    let relay = LocalRelay::start(Some("paired-relay-secret"));
    let temp = tempfile::tempdir().unwrap();
    let client_config = temp.path().join("client.toml");
    std::fs::write(
        &client_config,
        format!(
            "[relay]\nurl = {:?}\n[access]\nshared_token = \"paired-relay-secret\"\n",
            relay.url
        ),
    )
    .unwrap();
    let run = |args: &[&str]| {
        let mut all = vec!["--config", client_config.to_str().unwrap(), "--relay-only"];
        all.extend_from_slice(args);
        Peer::spawn(&all)
    };
    let source = temp.path().join("file");
    std::fs::write(&source, b"private relay paired transfer").unwrap();
    let output = temp.path().join("received");
    let mut sender = run(&["send", source.to_str().unwrap(), "--no-verify"]);
    let code = sender.line(false);
    let mut receiver = run(&[
        "receive",
        &format!("code:{code}"),
        "--no-verify",
        "--output",
        output.to_str().unwrap(),
    ]);
    receiver.wait(true);
    assert_eq!(sender.wait(true), format!("{code}\n").as_bytes());
    assert!(receiver.errors().contains("BLAKE3 verified"));
    // The copied command carries the relay, never the token.
    assert!(sender.errors().contains(&format!(
        "iroh-toolbox --relay {}/ --relay-only receive code: --no-verify --output ./received",
        relay.url
    )));
    assert_eq!(
        std::fs::read(output.join("file")).unwrap(),
        b"private relay paired transfer"
    );
    for log in [sender.errors(), receiver.errors()] {
        assert!(!log.contains("paired-relay-secret"));
        assert!(!log.contains("iroh-toolbox receive \"blob"));
    }
}

#[cfg(unix)]
fn pty_script(script: &str, relay: &LocalRelay) {
    let result = Command::new("python3")
        .arg(format!(
            "{}/tests/support/{script}",
            env!("CARGO_MANIFEST_DIR")
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
#[ignore = "requires an official iroh-relay binary and Unix PTYs; run mise run test:pairing"]
fn confirmation_number_on_real_terminals() {
    pty_script("verify.py", &LocalRelay::start(None));
}

#[cfg(unix)]
#[test]
#[ignore = "requires an official iroh-relay binary and Unix PTYs; run mise run test:pairing"]
fn machine_pairing_on_real_terminals() {
    pty_script("pair.py", &LocalRelay::start(None));
}
