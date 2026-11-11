//! Native chat clients paired by code through a local official relay.
use super::*;

fn say(peer: &mut Peer, line: &str) {
    writeln!(peer.child.stdin.as_mut().unwrap(), "{line}").unwrap();
}

/// Two chats paired by code, with connection diagnostics (`-v`) for timeouts.
fn pair(spawn: impl Fn(&[&str]) -> Peer, extra: &[&str]) -> (Peer, Peer) {
    let mut args = vec!["-v", "chat", "serve", "--no-progress", "--no-verify"];
    args.extend_from_slice(extra);
    let mut first = spawn(&args);
    let code = first.line(false);
    // The announcement reminds that whoever uses the code first gets the session.
    first.until_error_line(&format!(
        "Code: {code} (single-use: first admitted participant takes the session)"
    ));
    first.until_error_line(" chat connect code:");
    let joined = format!("code:{code}");
    let mut args = vec![
        "-v",
        "chat",
        "connect",
        &joined,
        "--no-progress",
        "--no-verify",
    ];
    args.extend_from_slice(extra);
    let mut second = spawn(&args);
    first.watch(&second);
    second.watch(&first);
    // Both sides name the admission path, never a label the peer chose.
    first.until_error_line("Chat ready; admission: code.");
    second.until_error_line("Chat ready; admission: code.");
    (first, second)
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_chat_messages_files_rejection_and_collisions() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("photos with spaces");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("picture"), b"picture bytes\0\xff").unwrap();
    std::fs::write(source.join("empty"), []).unwrap();
    let output = temp.path().join("received photos");
    let reverse = temp.path().join("reverse");
    let (mut a, mut b) = pair(|args| relay.spawn(args), &[]);
    say(&mut b, "hello from B");
    assert_eq!(a.line(false), "peer> hello from B");
    say(&mut a, &format!("/send \"{}\"", source.display()));
    b.until_error_line("Peer offers");
    assert!(!output.exists());
    say(&mut a, "still chatting while the offer is pending");
    assert_eq!(
        b.line(false),
        "peer> still chatting while the offer is pending"
    );
    say(&mut b, &format!("/receive \"{}\"", output.display()));
    b.until_error_line("BLAKE3 verified");
    a.until_error_line("Peer confirms:");
    assert_eq!(
        std::fs::read(output.join("photos with spaces/picture")).unwrap(),
        b"picture bytes\0\xff"
    );
    // Either participant can propose files; zero-byte files are valid offers.
    say(&mut b, &format!("/send {}", source.join("empty").display()));
    a.until_error_line("Peer offers");
    say(&mut a, &format!("/receive {}", reverse.display()));
    a.until_error_line("BLAKE3 verified");
    b.until_error_line("Peer confirms:");
    assert_eq!(std::fs::read(reverse.join("empty")).unwrap(), b"");
    say(&mut a, &format!("/send {}", source.display()));
    b.until_error_line("Peer offers");
    say(&mut b, "/reject");
    a.until_error_line("Peer rejected");
    say(&mut a, &format!("/send {}", source.display()));
    b.until_error_line("Peer offers");
    say(
        &mut b,
        &format!("/receive --no-replace {}", output.display()),
    );
    b.until_error_line("destination already exists");
    a.until_error_line("Peer could not receive");
    assert!(no_temporary_directories(&output));
    say(&mut b, "//a leading slash");
    assert_eq!(a.line(false), "peer> /a leading slash");
    // The joiner redials the address learned while pairing.
    say(&mut b, "/test-drop");
    b.until_error_line("Connection lost; reconnecting");
    say(&mut b, "after reconnection");
    b.until_error_line("Reconnected; 1 pending message(s) resent.");
    assert_eq!(a.line(false), "peer> after reconnection");
    say(&mut a, "/quit");
    a.wait(true);
    b.wait(true);
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_chat_through_private_relay_keeps_credentials_local() {
    let relay = LocalRelay::start(Some("chat-relay-secret"));
    let temp = tempfile::tempdir().unwrap();
    let client_config = temp.path().join("client.toml");
    std::fs::write(
        &client_config,
        format!(
            "[relay]\nurl = {:?}\n[access]\nshared_token = \"chat-relay-secret\"\n",
            relay.url
        ),
    )
    .unwrap();
    let config = client_config.to_str().unwrap();
    let (mut a, mut b) = pair(
        |args| {
            let mut all = vec!["--config", config, "--relay-only"];
            all.extend_from_slice(args);
            Peer::spawn(&all)
        },
        &[],
    );
    let source = temp.path().join("file");
    let output = temp.path().join("received");
    std::fs::write(&source, b"through the relay").unwrap();
    say(&mut a, &format!("/send {}", source.display()));
    b.until_error_line("Peer offers");
    say(&mut b, &format!("/receive {}", output.display()));
    b.until_error_line("BLAKE3 verified");
    a.until_error_line("Peer confirms:");
    assert_eq!(
        std::fs::read(output.join("file")).unwrap(),
        b"through the relay"
    );
    say(&mut b, "/quit");
    a.wait(true);
    b.wait(true);
    for log in [a.errors(), b.errors()] {
        assert!(!log.contains("chat-relay-secret"));
    }
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_chat_offer_limits_expiry_and_eof() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("file");
    std::fs::write(&source, b"ab").unwrap();
    // A acknowledges messages late, so B leaves before A writes its last
    // acknowledgement: that failed write must not hide B's goodbye.
    let mut command = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"));
    command
        .args(["--relay", &relay.url, "-v", "chat", "serve"])
        .args(["--transfer-timeout", "0.5", "--no-progress", "--no-verify"])
        .env("IROH_TOOLBOX_TEST_CHAT_ACK_DELAY_MS", "300");
    let mut a = Peer::start(command);
    let code = a.line(false);
    let mut b = relay.spawn(&[
        "-v",
        "chat",
        "connect",
        &format!("code:{code}"),
        "--max-bytes",
        "1",
        "--no-progress",
        "--no-verify",
    ]);
    a.watch(&b);
    b.watch(&a);
    a.until_error_line("Chat ready;");
    b.until_error_line("Chat ready;");
    say(&mut a, &format!("/send {}", source.display()));
    b.until_error_line("exceeds --max-bytes");
    a.until_error_line("Peer rejected");
    std::fs::write(&source, b"a").unwrap();
    say(&mut a, &format!("/send {}", source.display()));
    b.until_error_line("Peer offers");
    b.until_error_line("Peer withdrew");
    a.until_error_line("File offer expired");
    say(&mut b, "chat survives expiry");
    assert_eq!(a.line(false), "peer> chat survives expiry");
    b.child.stdin.take();
    b.wait(true);
    a.wait(true);
    assert!(a.errors().contains("Peer left the chat."), "{}", a.errors());
    assert!(!a.errors().contains("Connection lost"), "{}", a.errors());
}

#[cfg(unix)]
#[test]
#[ignore = "requires an official iroh-relay binary and Unix PTYs; run mise run test:pairing"]
fn code_chat_confirmation_number_on_real_terminals() {
    pty_script("chat_verify.py", &LocalRelay::start(None));
}

/// Chat messages, file offers in both directions, refusals, limits, and
/// reconnection by either side, between two chats paired by code. `run`
/// starts a client with the relay settings of the scenario.
pub(crate) fn conversation(run: impl Fn(&[&str]) -> Peer, root: &Path) {
    let source = root.join("chat file");
    let output = root.join("chat received");
    let reverse = root.join("chat reverse");
    std::fs::write(&source, b"native chat bytes\0\xff").unwrap();
    let mut a = run(&["chat", "serve", "--no-progress", "--no-verify"]);
    let code = a.line(false);
    let joined = format!("code:{code}");
    let mut b = run(&["chat", "connect", &joined, "--no-progress", "--no-verify"]);
    a.watch(&b);
    b.watch(&a);
    a.until_error_line("Chat ready;");
    b.until_error_line("Chat ready;");
    assert!(!a.errors().contains("mailbox"));
    assert!(!b.errors().contains("mailbox"));
    // A code serves one partner: a third machine cannot join the chat.
    let mut other = run(&[
        "chat",
        "connect",
        &joined,
        "--pairing-timeout",
        "2",
        "--no-verify",
    ]);
    other.wait(false);
    assert!(
        other.errors().contains("no machine answered"),
        "{}",
        other.errors()
    );
    say(&mut b, "native hello");
    assert_eq!(a.line(false), "peer> native hello");
    say(&mut a, &format!("/send \"{}\"", source.display()));
    b.until_error_line("Peer offers");
    assert!(!output.exists());
    say(&mut b, &format!("/receive \"{}\"", output.display()));
    b.until_error_line("BLAKE3 verified");
    a.until_error_line("Peer confirms:");
    assert_eq!(
        std::fs::read(output.join("chat file")).unwrap(),
        b"native chat bytes\0\xff"
    );
    say(&mut b, &format!("/send {}", source.display()));
    a.until_error_line("Peer offers");
    say(&mut a, &format!("/receive {}", reverse.display()));
    a.until_error_line("BLAKE3 verified");
    b.until_error_line("Peer confirms:");
    assert_eq!(
        std::fs::read(reverse.join("chat file")).unwrap(),
        b"native chat bytes\0\xff"
    );
    say(&mut a, &format!("/send {}", source.display()));
    b.until_error_line("Peer offers");
    say(
        &mut b,
        &format!("/receive --no-replace {}", output.display()),
    );
    b.until_error_line("destination already exists");
    a.until_error_line("Peer could not receive");
    say(&mut a, &format!("/send {}", source.display()));
    b.until_error_line("Peer offers");
    say(&mut b, "/reject");
    a.until_error_line("Peer rejected");
    say(&mut a, &format!("/send {}", source.display()));
    b.until_error_line("Peer offers");
    say(&mut a, "/cancel");
    a.until_error_line("File offer withdrawn");
    b.until_error_line("Peer withdrew");
    say(&mut b, "/reject");
    b.until_error_line("! No incoming file offer");
    say(&mut b, &"x".repeat(5000));
    b.until_error_line("exceeds 4096 bytes");
    say(&mut b, "still here");
    assert_eq!(a.line(false), "peer> still here");
    // A lost connection is re-established by the joining side, whichever
    // side lost it; a message typed meanwhile is delivered once.
    say(&mut b, "/test-drop");
    b.until_error_line("Connection lost; reconnecting");
    say(&mut b, "queued by the joiner");
    b.until_error_line("Reconnected; 1 pending message(s) resent.");
    assert_eq!(a.line(false), "peer> queued by the joiner");
    say(&mut a, "/test-drop");
    a.until_error_line("Connection lost; waiting");
    say(&mut a, "queued by the creator");
    a.until_error_line("Reconnected; 1 pending message(s) resent.");
    b.until_error_line("Reconnected.");
    assert_eq!(b.line(false), "peer> queued by the creator");
    say(&mut b, "/status");
    b.until_error_line("Unacknowledged messages: 0");
    say(&mut a, "/quit");
    a.wait(true);
    b.wait(true);
    assert!(!a.errors().contains("test-relay-credential"));
    assert!(!b.errors().contains("test-relay-credential"));
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_chat_conversation_files_refusals_and_reconnection() {
    let relay = LocalRelay::start(None);
    let temp = tempfile::tempdir().unwrap();
    conversation(|args| relay.spawn(args), temp.path());
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_chat_timestamps_follow_the_option_then_the_environment() {
    let relay = LocalRelay::start(None);
    let chat = |env: &str, args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"));
        command
            .env("IROH_TOOLBOX_TIMESTAMPS", env)
            .args(["--relay", &relay.url, "chat"])
            .args(args)
            .args(["--no-progress", "--no-verify"]);
        Peer::start(command)
    };
    let mut a = chat("always", &["serve"]);
    let code = a.line(false);
    // The option wins over the environment.
    let mut b = chat(
        "always",
        &["connect", &format!("code:{code}"), "--timestamps", "never"],
    );
    a.watch(&b);
    b.watch(&a);
    a.until_error_line("Chat ready;");
    b.until_error_line("Chat ready;");
    say(&mut b, "stamped");
    let line = a.line(false);
    let (time, text) = line.split_once(' ').unwrap();
    assert_eq!(text, "peer> stamped");
    assert!(
        time.len() == 5
            && time.as_bytes()[2] == b':'
            && time.replace(':', "").parse::<u16>().is_ok(),
        "{line}"
    );
    say(&mut a, "plain");
    assert_eq!(b.line(false), "peer> plain");
    say(&mut a, "/quit");
    a.wait(true);
    b.wait(true);
}

#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn code_chat_replacement_confirmation_does_not_consume_messages() {
    let relay = LocalRelay::start(None);
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("file");
    let output = root.path().join("out");
    std::fs::create_dir(&output).unwrap();
    std::fs::write(output.join("file"), b"old").unwrap();
    std::fs::write(output.join("unrelated"), b"keep").unwrap();
    let (mut a, mut b) = pair(|args| relay.spawn(args), &[]);
    for (flag, answer, expected) in [
        ("", "/reject", b"old".as_slice()),
        ("", "/confirm", b"new".as_slice()),
        ("--no-replace ", "", b"new".as_slice()),
        ("--force ", "", b"forced".as_slice()),
    ] {
        std::fs::write(
            &source,
            if flag == "--force " {
                b"forced".as_slice()
            } else {
                b"new"
            },
        )
        .unwrap();
        say(&mut a, &format!("/send {}", source.display()));
        b.until_error_line("Peer offers");
        say(&mut b, &format!("/receive {flag}{}", output.display()));
        if flag.is_empty() {
            b.until_error_line("Use /confirm");
            assert_eq!(std::fs::read(output.join("file")).unwrap(), b"old");
            say(&mut a, "a message while confirmation is pending");
            assert_eq!(
                b.line(false),
                "peer> a message while confirmation is pending"
            );
            say(&mut b, answer);
        }
        if answer == "/reject" || flag == "--no-replace " {
            a.until_error_line("Peer could not receive");
        } else {
            a.until_error_line("Peer confirms:");
        }
        assert_eq!(std::fs::read(output.join("file")).unwrap(), expected);
        assert_eq!(std::fs::read(output.join("unrelated")).unwrap(), b"keep");
    }
    say(&mut a, "/quit");
    a.wait(true);
    b.wait(true);
}

/// `chat serve` opens a code and a peer path; the first participant admitted
/// on either is the only one, and the other path admits nobody.
#[test]
#[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
fn a_mixed_chat_admits_one_participant_by_code_or_as_a_paired_machine() {
    use crate::slots::{Machine, Pair};
    let relay = LocalRelay::start(None);
    let globals = vec!["--relay".to_owned(), relay.url.clone()];
    let pair = Pair::relayed(globals.clone());
    // Not paired with nas: it can only use a code.
    let stranger = Machine::new("stranger");
    let serve = |extra: &[&str]| {
        let mut args = vec!["chat", "serve", "--no-progress", "--no-verify"];
        args.extend_from_slice(extra);
        let nas = pair.nas.spawn(&globals, &args);
        let code = nas.line(false);
        (nas, code)
    };
    let connect = |machine: &Machine, target: &str, extra: &[&str]| {
        let mut args = vec!["chat", "connect", target, "--no-progress"];
        // Only a code is confirmed; a paired machine proves its key instead.
        if target.starts_with("code:") {
            args.push("--no-verify");
        }
        args.extend_from_slice(extra);
        machine.spawn(&globals, &args)
    };

    // A paired machine joins first: the code path closes.
    let (mut nas, code) = serve(&[]);
    nas.until_error_line("Serving to: laptop");
    nas.until_error_line(&format!(
        "Code: {code} (single-use: first admitted participant takes the session)"
    ));
    let mut laptop = connect(&pair.laptop, "peer:nas", &[]);
    nas.watch(&laptop);
    nas.until_error_line("Chat ready; admission: paired machine laptop.");
    laptop.until_error_line("Chat ready; admission: paired machine nas.");
    let code_target = format!("code:{code}");
    let mut late = connect(&stranger, &code_target, &["--pairing-timeout", "3"]);
    late.wait(false);
    assert!(
        late.errors().contains("no machine answered"),
        "{}",
        late.errors()
    );
    say(&mut laptop, "/quit");
    laptop.wait(true);
    nas.wait(true);

    // A code holder joins first: a paired machine then hears that the chat is
    // busy. (`--allow` opens no code: see peer_chat.)
    let (mut nas, code) = serve(&[]);
    let code_target = format!("code:{code}");
    let mut holder = connect(&stranger, &code_target, &[]);
    nas.watch(&holder);
    nas.until_error_line("Chat ready; admission: code.");
    holder.until_error_line("Chat ready; admission: code.");
    let mut laptop = connect(&pair.laptop, "peer:nas", &[]);
    laptop.wait(false);
    assert!(
        laptop
            .errors()
            .contains("nas is in a chat session with another participant; try again later"),
        "{}",
        laptop.errors()
    );
    say(&mut holder, "/quit");
    holder.wait(true);
    nas.wait(true);

    // A removed machine is refused as a peer, but an unused code still admits
    // it, as a code participant, never as the paired machine it was.
    let phone = Machine::new("phone");
    pair.nas.pair_with("phone", &phone);
    pair.nas.run(&["peers", "rm", "laptop"]);
    let (mut nas, code) = serve(&[]);
    let mut refused = connect(&pair.laptop, "peer:nas", &[]);
    refused.wait(false);
    assert!(
        refused.errors().contains("nas does not know this machine"),
        "{}",
        refused.errors()
    );
    let code_target = format!("code:{code}");
    let mut by_code = connect(&pair.laptop, &code_target, &[]);
    nas.watch(&by_code);
    nas.until_error_line("Chat ready; admission: code.");
    by_code.until_error_line("Chat ready; admission: code.");
    assert!(!nas.errors().contains("paired machine laptop"));
    say(&mut by_code, "/quit");
    by_code.wait(true);
    nas.wait(true);

    // Stopping the server voids its code; a restart prints a new one.
    let (mut nas, old) = serve(&[]);
    let _ = nas.child.kill();
    nas.wait(false);
    let (mut nas, new) = serve(&[]);
    assert_ne!(old, new);
    let old_target = format!("code:{old}");
    let mut stale = connect(&stranger, &old_target, &["--pairing-timeout", "3"]);
    stale.wait(false);
    assert!(
        stale.errors().contains("no machine answered"),
        "{}",
        stale.errors()
    );
    let _ = nas.child.kill();
}
