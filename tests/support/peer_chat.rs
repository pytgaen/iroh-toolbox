//! Chats between paired machines without a code: `chat serve --peers-only` and
//! `chat connect peer:NAME`, directly between loopback endpoints (no relay).
use super::*;
use slots::{Machine, Pair, errors_with};

fn say(peer: &mut Peer, line: &str) {
    writeln!(peer.child.stdin.as_mut().unwrap(), "{line}").unwrap();
}

#[test]
fn a_peers_only_chat_admits_one_paired_machine() {
    let pair = Pair::direct();
    let (guest, friend) = (Machine::new("guest"), Machine::new("friend"));
    for machine in [&guest, &friend] {
        pair.nas.pair_with(&machine.name, machine);
        machine.pair_with("nas", &pair.nas);
    }
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("chat file");
    std::fs::write(&source, b"chat over a slot").unwrap();
    let (output, reverse) = (temp.path().join("received"), temp.path().join("reverse"));
    // --allow implies --peers-only: no relay and no code.
    let mut server = pair.serve(&[
        "-v",
        "chat",
        "serve",
        "--allow",
        "laptop,guest",
        "--no-progress",
    ]);
    errors_with(&server, "Serving to: laptop, guest");
    errors_with(&server, "chat connect peer:nas --addr ");
    // No code exists, so none is shown and no code path bypasses --allow.
    assert!(!server.errors().contains("Code:"), "{}", server.errors());
    let mut outsider = pair.connect_as(
        &friend,
        &server,
        &["chat", "connect", "peer:nas", "--no-progress"],
    );
    outsider.wait(false);
    assert!(
        outsider
            .errors()
            .contains("this nas server (slot chat) does not allow this machine (--allow)"),
        "{}",
        outsider.errors()
    );
    let mut client = pair.connect(
        &server,
        &["-v", "chat", "connect", "peer:nas", "--no-progress"],
    );
    server.watch(&client);
    client.watch(&server);
    // Each side names the machine whose key it verified.
    server.until_error_line("Chat ready; admission: paired machine laptop.");
    client.until_error_line("Chat ready; admission: paired machine nas.");
    // The session has its participant: another allowed machine hears so.
    let mut third = pair.connect_as(
        &guest,
        &server,
        &["chat", "connect", "peer:nas", "--no-progress"],
    );
    third.wait(false);
    assert!(
        third
            .errors()
            .contains("nas is in a chat session with another participant; try again later"),
        "{}",
        third.errors()
    );
    say(&mut client, "hello over the slot");
    assert_eq!(server.line(false), "laptop> hello over the slot");
    // File offers in both directions, served only to the participant.
    say(&mut client, &format!("/send \"{}\"", source.display()));
    server.until_error_line("laptop offers");
    say(&mut server, &format!("/receive \"{}\"", output.display()));
    server.until_error_line("BLAKE3 verified");
    client.until_error_line("nas confirms:");
    assert_eq!(
        std::fs::read(output.join("chat file")).unwrap(),
        b"chat over a slot"
    );
    say(&mut server, &format!("/send \"{}\"", source.display()));
    client.until_error_line("nas offers");
    say(&mut client, &format!("/receive \"{}\"", reverse.display()));
    client.until_error_line("BLAKE3 verified");
    server.until_error_line("laptop confirms:");
    // Either side may lose the connection: the joiner comes back through the slot.
    say(&mut client, "/test-drop");
    client.until_error_line("Connection lost; reconnecting");
    say(&mut client, "queued by the joiner");
    client.until_error_line("Reconnected; 1 pending message(s) resent.");
    assert_eq!(server.line(false), "laptop> queued by the joiner");
    say(&mut server, "/test-drop");
    server.until_error_line("Connection lost; waiting");
    client.until_error_line("Reconnected.");
    say(&mut client, "/status");
    client.until_error_line("Peer: admitted by paired machine nas");
    // A removed machine keeps its open session but cannot reconnect.
    pair.nas.run(&["peers", "rm", "laptop"]);
    say(&mut client, "still open after peers rm");
    assert_eq!(server.line(false), "laptop> still open after peers rm");
    say(&mut client, "/test-drop");
    client.until_error_line("does not know this machine");
    let _ = server.child.kill();
    let _ = client.child.kill();
}

#[test]
fn chat_connect_explains_an_absent_chat_and_checks_its_options() {
    let pair = Pair::direct();
    let addr = format!("127.0.0.1:{}", slots::free_port());
    let mut absent = pair.laptop.spawn(
        &[],
        &[
            "--direct-only",
            "--timeout",
            "2",
            "chat",
            "connect",
            "peer:nas/equipe",
            "--addr",
            &addr,
        ],
    );
    absent.wait(false);
    assert!(
        absent
            .errors()
            .contains("nas has no chat open; run 'iroh-toolbox chat serve --slot equipe' on nas"),
        "{}",
        absent.errors()
    );
    for (args, expected) in [
        (
            vec!["chat", "serve", "--peers-only", "--code-words", "8"],
            "`chat serve` with --peers-only or --allow creates none",
        ),
        (
            vec!["chat", "serve", "--peers-only", "--no-verify"],
            "`chat serve` with --peers-only or --allow creates none",
        ),
        (
            vec!["chat", "serve", "--allow", "laptop", "--no-verify"],
            "`chat serve` with --peers-only or --allow creates none",
        ),
        (
            vec!["chat", "connect", "peer:nas", "--no-verify"],
            "--no-verify and --pairing-timeout apply to code pairing",
        ),
        (
            vec![
                "chat",
                "connect",
                "code:abacus-abdomen",
                "--addr",
                "1.2.3.4:5",
            ],
            "--addr applies to peer: targets only",
        ),
        (
            vec!["chat", "connect", "peer:nas", "--peers-only"],
            "unexpected argument",
        ),
        (
            vec!["chat", "connect", "peer:nas", "--allow", "laptop"],
            "unexpected argument",
        ),
        (
            vec!["chat", "serve", "--addr", "1.2.3.4:5"],
            "unexpected argument",
        ),
        // Peer::spawn uses --direct-only: a code needs a relay.
        (vec!["chat", "serve"], "`chat serve --peers-only` opens one"),
    ] {
        let mut peer = Peer::spawn(&args);
        assert!(peer.wait(false).is_empty());
        assert!(
            peer.errors().contains(expected),
            "{args:?}: {}",
            peer.errors()
        );
    }
    // Without any paired machine, the peer options have nothing to serve.
    let lonely = Machine::new("lonely");
    let mut refused = lonely.spawn(
        &[],
        &[
            "--relay",
            "http://127.0.0.1:9",
            "chat",
            "serve",
            "--no-verify",
            "--slot",
            "x",
        ],
    );
    refused.wait(false);
    assert!(
        refused
            .errors()
            .contains("--slot and --port apply to paired machines, and this machine has none"),
        "{}",
        refused.errors()
    );
}
