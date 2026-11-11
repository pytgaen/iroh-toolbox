//! Explicit, small live checks of the default public-relay configuration.
use super::*;

fn public(args: &[&str]) -> Peer {
    let mut command = Command::new(env!("CARGO_BIN_EXE_iroh-toolbox"));
    command.args(["--timeout", "10"]).args(args);
    Peer::start(command)
}

#[test]
#[ignore = "contacts real public iroh relays; requires internet; run mise run test:public-relay"]
fn default_public_registration_and_relayed_ping_without_custom_configuration() {
    let mut check = public(&["relay", "check", "--json"]);
    let result: serde_json::Value = serde_json::from_slice(&check.wait(true)).unwrap();
    assert_eq!(result["status"], "online");
    assert_eq!(result["mode"], "public");
    let relay: iroh::RelayUrl = result["relay"].as_str().unwrap().parse().unwrap();
    assert!(iroh::RelayMode::Default.relay_map().contains(&relay));
    let server = public(&["--relay-only", "ping", "serve"]);
    let ticket = server.line(false);
    let parsed: iroh_tickets::endpoint::EndpointTicket = ticket.parse().unwrap();
    assert!(parsed.endpoint_addr().relay_urls().next().is_some());
    assert!(parsed.endpoint_addr().ip_addrs().next().is_none());
    // Full tickets and bare endpoint identities both work with the public map.
    for target in [&ticket, &parsed.endpoint_addr().id.to_string()] {
        let mut peer = public(&[
            "--relay-only",
            "ping",
            "connect",
            target,
            "--count",
            "1",
            "--json",
        ]);
        let pong: serde_json::Value = serde_json::from_slice(&peer.wait(true)).unwrap();
        assert_eq!(pong["route"], "relay");
    }
}

#[test]
#[ignore = "contacts real public iroh relays; requires internet; run mise run test:public-relay"]
fn a_public_slot_reads_back_its_own_dns_record_without_a_warning() {
    let pair = slots::Pair::relayed(vec!["--timeout".into(), "10".into()]);
    let server = pair.serve(&[
        "-v",
        "pipe",
        "serve",
        "--allow",
        "all",
        "--slot",
        "takeover-check",
    ]);
    // The first check runs one TTL after the server is online.
    let errors = slots::errors_within(
        &server,
        "slot takeover-check: its address record is this server's",
        Duration::from_secs(90),
    );
    assert!(!errors.contains("another machine"), "{errors}");
}

#[test]
#[ignore = "contacts real public iroh relays; requires internet; run mise run test:public-relay"]
fn default_public_chat_and_file_without_custom_configuration() {
    let root = tempfile::tempdir().unwrap();
    // Reuses the native conversation/file scenario, including explicit consent.
    pairing::chat::conversation(
        |args| {
            let mut relay_only = vec!["--relay-only"];
            relay_only.extend_from_slice(args);
            public(&relay_only)
        },
        root.path(),
    );
}
