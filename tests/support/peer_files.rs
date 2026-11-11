//! Files between paired machines without a code: inboxes and targeted sends,
//! directly between loopback endpoints (no relay).
use super::*;
use slots::{Machine, Pair, Served, errors_with};
use std::path::PathBuf;

/// A folder `name` with one file of `bytes` and a nested one, under `root`.
fn folder(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(path.join("nested")).unwrap();
    std::fs::write(path.join("file"), bytes).unwrap();
    std::fs::write(path.join("nested/inner"), b"inner").unwrap();
    path
}

/// Runs `send SOURCE TARGET` from `machine` to `served`, to completion.
fn send(pair: &Pair, machine: &Machine, served: &Served, source: &Path, target: &str) -> Peer {
    pair.connect_as(machine, served, &["send", source.to_str().unwrap(), target])
}

fn inbox(pair: &Pair, output: &Path, extra: &[&str]) -> Served {
    let mut args = vec!["receive", "--inbox", "--output", output.to_str().unwrap()];
    args.extend_from_slice(extra);
    pair.serve(&args)
}

#[test]
fn an_inbox_receives_targeted_sends_and_never_replaces_files() {
    let pair = Pair::direct();
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("inbox");
    let mut server = inbox(&pair, &output, &[]);
    errors_with(&server, "Serving to: laptop");
    errors_with(&server, "send PATH peer:nas --addr ");
    errors_with(&server, "until Ctrl-C");

    let photos = folder(temp.path(), "photos", b"first");
    let mut sender = send(&pair, &pair.laptop, &server, &photos, "peer:nas");
    assert!(sender.wait(true).is_empty());
    assert!(
        sender
            .errors()
            .contains("nas received photos: BLAKE3 verified and published"),
        "{}",
        sender.errors()
    );
    errors_with(&server, "received photos from laptop");
    assert_eq!(std::fs::read(output.join("photos/file")).unwrap(), b"first");
    assert_eq!(
        std::fs::read(output.join("photos/nested/inner")).unwrap(),
        b"inner"
    );

    // The same files again: recognized, nothing rewritten.
    let mut again = send(&pair, &pair.laptop, &server, &photos, "peer:nas");
    again.wait(true);
    assert!(
        again
            .errors()
            .contains("nas already holds identical files; nothing was sent again"),
        "{}",
        again.errors()
    );
    // Other contents under the same names: refused, the destination is unchanged.
    std::fs::write(photos.join("file"), b"second").unwrap();
    let mut changed = send(&pair, &pair.laptop, &server, &photos, "peer:nas");
    changed.wait(false);
    assert!(
        changed
            .errors()
            .contains("nas refused photos: its inbox already holds files with these names; nothing was replaced"),
        "{}",
        changed.errors()
    );
    assert_eq!(std::fs::read(output.join("photos/file")).unwrap(), b"first");

    // The inbox stays open for the next transfer.
    let notes = folder(temp.path(), "notes", b"notes");
    let mut next = send(&pair, &pair.laptop, &server, &notes, "peer:nas");
    next.wait(true);
    assert_eq!(std::fs::read(output.join("notes/file")).unwrap(), b"notes");

    // A machine nas does not know gets nothing in.
    let stranger = Machine::new("stranger");
    stranger.pair_with("nas", &pair.nas);
    let mut refused = send(&pair, &stranger, &server, &notes, "peer:nas");
    refused.wait(false);
    assert!(
        refused.errors().contains("nas does not know this machine"),
        "{}",
        refused.errors()
    );
    assert!(server.child.try_wait().unwrap().is_none());
}

#[test]
fn an_inbox_refuses_top_level_hidden_names_without_hidden() {
    let pair = Pair::direct();
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("home");
    std::fs::create_dir(&output).unwrap();
    let source = temp.path().join(".bashrc");
    std::fs::write(&source, b"evil").unwrap();

    let server = inbox(&pair, &output, &[]);
    errors_with(&server, "until Ctrl-C");
    let mut refused = send(&pair, &pair.laptop, &server, &source, "peer:nas");
    refused.wait(false);
    assert!(
        refused.errors().contains(
            "nas refused .bashrc: a name starts with a dot; its inbox accepts them with --hidden"
        ),
        "{}",
        refused.errors()
    );
    errors_with(
        &server,
        ".bashrc starts with a dot; accept such names with --hidden",
    );
    assert!(!output.join(".bashrc").exists());
    drop(server);

    let server = inbox(&pair, &output, &["--hidden"]);
    errors_with(&server, "until Ctrl-C");
    let mut accepted = send(&pair, &pair.laptop, &server, &source, "peer:nas");
    accepted.wait(true);
    assert_eq!(std::fs::read(output.join(".bashrc")).unwrap(), b"evil");
}

#[test]
fn an_inbox_with_once_exits_after_one_success_only() {
    let pair = Pair::direct();
    let friend = Machine::new("friend");
    pair.nas.pair_with("friend", &friend);
    friend.pair_with("nas", &pair.nas);
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("inbox");
    let mut server = inbox(
        &pair,
        &output,
        &["--allow", "laptop", "--once", "--max-bytes", "64"],
    );
    errors_with(&server, "Serving to: laptop");
    errors_with(&server, "one transfer, then exit");
    let small = folder(temp.path(), "small", b"small");
    // Refusals do not consume --once: a machine outside --allow, then too large.
    let mut outsider = send(&pair, &friend, &server, &small, "peer:nas");
    outsider.wait(false);
    assert!(
        outsider
            .errors()
            .contains("this nas server (slot inbox) does not allow this machine (--allow)"),
        "{}",
        outsider.errors()
    );
    let large = folder(temp.path(), "large", &[7; 1024]);
    let mut too_large = send(&pair, &pair.laptop, &server, &large, "peer:nas");
    too_large.wait(false);
    assert!(
        too_large
            .errors()
            .contains("nas refused large: larger than its inbox accepts (--max-bytes)"),
        "{}",
        too_large.errors()
    );
    assert!(server.child.try_wait().unwrap().is_none());
    let mut sender = send(&pair, &pair.laptop, &server, &small, "peer:nas");
    sender.wait(true);
    server.wait(true);
    assert_eq!(std::fs::read(output.join("small/file")).unwrap(), b"small");
    assert!(!output.join("large").exists());
    // Once closed, a sender is told how to reach an inbox again.
    let globals = [
        "--direct-only",
        "--timeout",
        "2",
        "send",
        small.to_str().unwrap(),
        "peer:nas",
        "--addr",
    ];
    let mut args: Vec<String> = globals.iter().map(|arg| (*arg).to_owned()).collect();
    args.push(format!("127.0.0.1:{}", server.port.unwrap()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut late = pair.laptop.spawn(&[], &args);
    late.wait(false);
    assert!(
        late.errors()
            .contains("nas is not receiving; run 'iroh-toolbox receive --inbox' on nas"),
        "{}",
        late.errors()
    );
}

#[test]
fn senders_wait_their_turn_and_learn_that_a_once_inbox_closed() {
    let pair = Pair::direct();
    let friend = Machine::new("friend");
    pair.nas.pair_with("friend", &friend);
    friend.pair_with("nas", &pair.nas);
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("inbox");
    let mut server = inbox(&pair, &output, &["--once"]);
    let slow = folder(temp.path(), "slow", &[1; 256 * 1024]);
    let quick = folder(temp.path(), "quick", b"quick");
    // The first sender serves slowly; the second arrives meanwhile.
    let mut args = vec![
        "send".to_owned(),
        slow.to_str().unwrap().to_owned(),
        "peer:nas".to_owned(),
    ];
    args.extend([
        "--addr".to_owned(),
        format!("127.0.0.1:{}", server.port.unwrap()),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut first = pair.laptop.spawn_with(
        &pair.globals,
        &args,
        &[("IROH_TOOLBOX_TEST_SERVE_DELAY_MS", "20")],
    );
    errors_with(&server, "receiving slow from laptop");
    let mut second = send(&pair, &friend, &server, &quick, "peer:nas");
    errors_with(
        &second,
        "inbox is receiving another transfer; waiting for its turn",
    );
    errors_with(&server, "friend waits to send quick");
    first.wait(true);
    second.wait(false);
    assert!(
        second
            .errors()
            .contains("nas's inbox is closing: it already received its single transfer (--once)"),
        "{}",
        second.errors()
    );
    server.wait(true);
    assert!(output.join("slow/file").exists() && !output.join("quick").exists());
}

#[test]
fn an_interrupted_send_resumes_its_verified_data() {
    let pair = Pair::direct();
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("inbox");
    let server = inbox(&pair, &output, &[]);
    let data: Vec<u8> = (0..8 * 1048576u32).map(|v| (v % 249) as u8).collect();
    let big = folder(temp.path(), "big", &data);
    let mut args = vec![
        "send".to_owned(),
        big.to_str().unwrap().to_owned(),
        "peer:nas".to_owned(),
    ];
    args.extend([
        "--addr".to_owned(),
        format!("127.0.0.1:{}", server.port.unwrap()),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // The sender's connection breaks midway, as a lost network would.
    let mut cut = pair.laptop.spawn_with(
        &pair.globals,
        &args,
        &[
            ("IROH_TOOLBOX_TEST_SERVE_DELAY_MS", "4"),
            ("IROH_TOOLBOX_TEST_SERVE_CUT_BYTES", "2097152"),
        ],
    );
    cut.wait(false);
    assert!(
        cut.errors().contains("the outcome is unknown"),
        "{}",
        cut.errors()
    );
    errors_with(&server, "interrupted; verified data is kept");
    assert!(!output.join("big").exists());
    let mut again = send(&pair, &pair.laptop, &server, &big, "peer:nas");
    again.wait(true);
    errors_with(&server, "resuming a partial transfer");
    assert_eq!(std::fs::read(output.join("big/file")).unwrap(), data);
    // No working directory is left next to the destination or in it.
    let leftovers: Vec<_> = [temp.path(), output.as_path()]
        .into_iter()
        .flat_map(|dir| std::fs::read_dir(dir).unwrap())
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".iroh-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn named_inboxes_and_their_hints() {
    let pair = Pair::direct();
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("depot");
    let server = inbox(&pair, &output, &["--slot", "depot"]);
    errors_with(&server, "send PATH peer:nas/depot");
    let notes = folder(temp.path(), "notes", b"notes");
    let mut named = send(&pair, &pair.laptop, &server, &notes, "peer:nas/depot");
    named.wait(true);
    assert!(output.join("notes/file").exists());
    // A client of another command learns which command serves this slot.
    let mut ping = pair.connect(
        &server,
        &["ping", "connect", "peer:nas/depot", "--count", "1"],
    );
    ping.wait(false);
    assert!(
        ping.errors().contains("unknown slot or protocol"),
        "{}",
        ping.errors()
    );
}

#[test]
fn inbox_and_targeted_send_options_are_checked_before_any_network_use() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("file");
    std::fs::write(&source, b"data").unwrap();
    let source = source.to_str().unwrap();
    for (args, expected) in [
        (
            vec!["receive", "--inbox", "--force"],
            "an inbox never replaces or merges existing files",
        ),
        (
            vec!["receive", "--inbox", "--no-verify"],
            "--no-verify and --pairing-timeout apply to code receptions",
        ),
        (
            vec!["send", source, "peer:nas", "--code-words", "8"],
            "--code-words, --no-verify and --pairing-timeout apply to code transfers",
        ),
        (
            vec!["send", source, "code:abacus-abdomen"],
            "send creates its code itself",
        ),
        (
            vec!["send", source, "endpointabc"],
            "send PATH accepts peer:NAME[/SLOT] only",
        ),
        (
            vec!["send", source, "peer:ghost"],
            "no paired machine named ghost",
        ),
    ] {
        let start = Instant::now();
        let mut peer = Peer::spawn(&args);
        assert!(peer.wait(false).is_empty());
        assert!(
            peer.errors().contains(expected),
            "{args:?}: {}",
            peer.errors()
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}

/// `send SOURCE --peers-only EXTRA` on nas, with optional environment.
fn offer(pair: &Pair, source: &Path, extra: &[&str], env: &[(&str, &str)]) -> Served {
    let mut args = vec!["send", source.to_str().unwrap(), "--peers-only"];
    args.extend_from_slice(extra);
    pair.serve_with(&pair.nas, &args, env)
}

/// `receive TARGET --output OUTPUT` from `machine`.
fn fetch(pair: &Pair, machine: &Machine, served: &Served, target: &str, output: &Path) -> Peer {
    pair.connect_as(
        machine,
        served,
        &["receive", target, "--output", output.to_str().unwrap()],
    )
}

#[test]
fn send_peers_only_serves_one_receiver_then_exits() {
    let pair = Pair::direct();
    let friend = Machine::new("friend");
    pair.nas.pair_with("friend", &friend);
    friend.pair_with("nas", &pair.nas);
    let temp = tempfile::tempdir().unwrap();
    let photos = folder(temp.path(), "photos", &[5; 256 * 1024]);
    // The sender serves slowly, so a second receiver arrives during the reception.
    let mut server = offer(
        &pair,
        &photos,
        &["--allow", "laptop,friend"],
        &[("IROH_TOOLBOX_TEST_SERVE_DELAY_MS", "20")],
    );
    errors_with(&server, "Serving to: laptop, friend");
    errors_with(&server, "receive peer:nas --output ./received --addr ");
    errors_with(&server, "to the first paired machine that receives it");
    let output = temp.path().join("laptop");
    let mut receiver = fetch(&pair, &pair.laptop, &server, "peer:nas", &output);
    errors_with(&server, "laptop receives photos");
    let mut late = fetch(
        &pair,
        &friend,
        &server,
        "peer:nas",
        &temp.path().join("friend"),
    );
    late.wait(false);
    assert!(
        late.errors()
            .contains("nas is sending to another machine; ask nas to share with `send PATH --peers-only --share`"),
        "{}",
        late.errors()
    );
    receiver.wait(true);
    server.wait(true);
    errors_with(
        &server,
        "laptop received photos: BLAKE3 verified and published",
    );
    assert_eq!(
        std::fs::read(output.join("photos/file")).unwrap(),
        [5; 256 * 1024]
    );
    assert!(!temp.path().join("friend").exists());
}

#[test]
fn a_refused_receiver_does_not_reserve_the_offer() {
    let pair = Pair::direct();
    let friend = Machine::new("friend");
    pair.nas.pair_with("friend", &friend);
    friend.pair_with("nas", &pair.nas);
    let temp = tempfile::tempdir().unwrap();
    let photos = folder(temp.path(), "photos", b"photos");
    let mut server = offer(&pair, &photos, &["--allow", "laptop"], &[]);
    let mut outsider = fetch(
        &pair,
        &friend,
        &server,
        "peer:nas",
        &temp.path().join("friend"),
    );
    outsider.wait(false);
    assert!(
        outsider
            .errors()
            .contains("this nas server (slot send) does not allow this machine (--allow)"),
        "{}",
        outsider.errors()
    );
    let output = temp.path().join("laptop");
    let mut receiver = fetch(&pair, &pair.laptop, &server, "peer:nas", &output);
    receiver.wait(true);
    server.wait(true);
    assert_eq!(
        std::fs::read(output.join("photos/file")).unwrap(),
        b"photos"
    );
}

#[test]
fn a_share_serves_every_receiver_until_stopped() {
    let pair = Pair::direct();
    let friend = Machine::new("friend");
    pair.nas.pair_with("friend", &friend);
    friend.pair_with("nas", &pair.nas);
    let temp = tempfile::tempdir().unwrap();
    let photos = folder(temp.path(), "photos", &[9; 128 * 1024]);
    let mut server = offer(
        &pair,
        &photos,
        &["--share", "--slot", "photos"],
        &[("IROH_TOOLBOX_TEST_SERVE_DELAY_MS", "5")],
    );
    errors_with(&server, "until Ctrl-C");
    // The offer is a snapshot: later edits of the source change nothing.
    std::fs::write(photos.join("file"), b"edited").unwrap();
    // Two receivers at once, then the same machine again.
    let (laptop_out, friend_out) = (temp.path().join("laptop"), temp.path().join("friend"));
    let mut first = fetch(&pair, &pair.laptop, &server, "peer:nas/photos", &laptop_out);
    let mut second = fetch(&pair, &friend, &server, "peer:nas/photos", &friend_out);
    first.wait(true);
    second.wait(true);
    let again_out = temp.path().join("again");
    let mut again = fetch(&pair, &pair.laptop, &server, "peer:nas/photos", &again_out);
    again.wait(true);
    for output in [&laptop_out, &friend_out, &again_out] {
        assert_eq!(
            std::fs::read(output.join("photos/file")).unwrap(),
            [9; 128 * 1024]
        );
    }
    // Into the same destination: recognized, nothing changed, and reported per receiver.
    let mut identical = fetch(&pair, &pair.laptop, &server, "peer:nas/photos", &laptop_out);
    identical.wait(true);
    assert!(
        identical
            .errors()
            .contains("identical files are already in"),
        "{}",
        identical.errors()
    );
    errors_with(&server, "laptop already holds identical files");
    errors_with(&server, "friend received photos");
    assert!(
        !server.errors().contains("all peers"),
        "{}",
        server.errors()
    );
    // The share keeps running until it is stopped.
    assert!(server.child.try_wait().unwrap().is_none());
}

#[test]
fn an_interrupted_reception_from_a_peer_resumes() {
    let pair = Pair::direct();
    let temp = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..8 * 1048576u32).map(|v| (v % 251) as u8).collect();
    let big = folder(temp.path(), "big", &data);
    let mut server = offer(
        &pair,
        &big,
        &[],
        &[
            ("IROH_TOOLBOX_TEST_SERVE_DELAY_MS", "4"),
            ("IROH_TOOLBOX_TEST_SERVE_CUT_BYTES", "2097152"),
        ],
    );
    let output = temp.path().join("received");
    let mut cut = fetch(&pair, &pair.laptop, &server, "peer:nas", &output);
    cut.wait(false);
    assert!(
        cut.errors()
            .contains("the connection to nas was lost; run the same command again"),
        "{}",
        cut.errors()
    );
    assert!(
        cut.errors().contains("partial transfer kept"),
        "{}",
        cut.errors()
    );
    // Only the interrupted receiver may resume, and it does.
    let mut again = fetch(&pair, &pair.laptop, &server, "peer:nas", &output);
    again.wait(true);
    assert!(
        again.errors().contains("resuming a partial transfer"),
        "{}",
        again.errors()
    );
    server.wait(true);
    assert_eq!(std::fs::read(output.join("big/file")).unwrap(), data);
}

#[test]
fn peer_receptions_explain_an_absent_sender_and_check_their_options() {
    let pair = Pair::direct();
    let temp = tempfile::tempdir().unwrap();
    let port = slots::free_port().to_string();
    let addr = format!("127.0.0.1:{port}");
    let output = temp.path().join("out");
    let mut absent = pair.laptop.spawn(
        &[],
        &[
            "--direct-only",
            "--timeout",
            "2",
            "receive",
            "peer:nas/photos",
            "--addr",
            &addr,
            "--output",
            output.to_str().unwrap(),
        ],
    );
    absent.wait(false);
    assert!(
        absent.errors().contains(
            "nas is not sending; run 'iroh-toolbox send PATH --peers-only --slot photos' on nas"
        ),
        "{}",
        absent.errors()
    );
    let source = folder(temp.path(), "source", b"x");
    let source = source.to_str().unwrap();
    for (args, expected) in [
        (
            vec!["send", source, "--peers-only", "--code-words", "8"],
            "--code-words, --no-verify and --pairing-timeout apply to code transfers",
        ),
        // --allow implies --peers-only: no code to shape.
        (
            vec!["send", source, "--allow", "laptop", "--code-words", "8"],
            "--peers-only and --allow rely on the pairing of both machines",
        ),
        (
            vec!["receive", "peer:nas", "--no-verify"],
            "--no-verify and --pairing-timeout apply to code receptions",
        ),
        (
            vec!["receive", "code:abacus-abdomen", "--addr", "1.2.3.4:5"],
            "--addr applies to peer: targets only",
        ),
    ] {
        let mut peer = Peer::spawn(&args);
        assert!(peer.wait(false).is_empty());
        assert!(
            peer.errors().contains(expected),
            "{args:?}: {}",
            peer.errors()
        );
    }
}
