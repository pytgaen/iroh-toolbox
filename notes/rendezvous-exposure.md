# Rendezvous address exposure

Date: 2026-11-10. Item E of [security-hardening-spec.md](security-hardening-spec.md).
Versions: iroh 1.4.1, noq 1.4.0, iroh-relay 1.4.1 (the pinned ones).

## Question

The rendezvous key of a code derives from public words 1–3, so anyone who
knows or enumerates them derives the same endpoint ID. Can that third party,
connecting on the pairing ALPN and never sending a `hello`, learn the creator's
direct IP addresses? What does the pkarr record contain? What does a squatter
of the rendezvous learn about the joiner?

## Findings

1. **The pkarr record holds the relay URL only.** `PkarrPublisher` filters
   what it publishes with `AddrFilter::relay_only()` by default
   (`iroh-1.4.1/src/address_lookup/pkarr.rs` lines 22–23, 171 and 213–217; the
   filter is applied in `update_endpoint_data`, line 363). `network::bind_keyed`
   uses `PkarrPublisher::n0_dns()` without `addr_filter`, so no IP address is
   published. Whoever reads n0's DNS learns the endpoint ID and the creator's
   home relay, not its addresses.
2. **A connection leaks the direct addresses, with no authentication.** For
   every connection, accepted or dialed, iroh copies the endpoint's local
   direct addresses into the QUIC NAT-traversal candidates
   (`iroh-1.4.1/src/socket/remote_map/remote_state.rs`:
   `handle_msg_add_connection` lines 383–420 calls `update_qnt_candidates`,
   defined at lines 1123–1141; `local_candidates` at lines 1109–1115;
   `update_local_direct_address` at lines 498–504 keeps them current). The
   candidates are exchanged inside the QUIC connection (noq `n0_nat_traversal`)
   and the dialer then holepunches toward them (`trigger_holepunching`, lines
   523–571). None of this waits for application data, so a stranger that only
   completes the handshake obtains the creator's interface addresses and a
   direct path. The same holds in the other direction: the accepting side
   learns the dialer's addresses and sees its packets' source address.
3. **iroh can make an endpoint relay-only.** `Builder::clear_ip_transports`
   (`iroh-1.4.1/src/endpoint.rs` lines 506–512, used the same way by iroh's own
   test at line 2624 "disable direct") removes the IP sockets, so there are no
   direct addresses to advertise or probe. The toolbox already uses it for
   `--relay-only` (`network::bind_with`).

## What was run

An ignored unit test, `pairing::tests::rendezvous_exposes_no_direct_address_to_a_stranger`
(`src/pairing/session.rs`), starts the official `iroh-relay` 1.4.1 on loopback
(`IROH_TEST_RELAY_BINARY`), with no public relay and no n0 DNS involved. A
stranger endpoint with default (direct and relay) transports dials the
target's endpoint ID with `EndpointAddr::new(id).with_relay_url(relay)`, which
is what a resolver would return from the record of finding 1: an ID and a
relay URL, no IP. It completes the handshake, sends nothing and waits six
seconds. The test then lists the `Connection::paths()` of both sides.

Command:

```console
IROH_TEST_RELAY_BINARY=…/iroh-relay cargo test --locked --bin iroh-toolbox \
  rendezvous_exposes -- --ignored --nocapture
```

Addresses below are from this WSL2 host; the relay port varies.

| Target | Stranger's paths | Target's paths |
| --- | --- | --- |
| Before the fix: rendezvous bound by `bind_keyed` (relay + UDP) | relay, **`ip:172.17.0.1:53314`** (the creator's interface address, not in the dialed address) | relay, **`ip:10.255.255.254:53172`** (the stranger's address) |
| Control, `bind_keyed` after the fix (relay + UDP) | relay, `ip:172.21.73.229:…` | relay, `ip:10.255.255.254:…` |
| After the fix: rendezvous bound by `bind_rendezvous` (relay only) | relay only | relay only |

Before the fix the endpoint advertised three IP addresses
(`10.71.100.1`, `172.17.0.1`, `172.21.73.229`) next to the relay; after it,
`ep.addr()` holds the relay URL alone. The control row shows the test would
detect a leak: the same stranger does get a direct path to a regular keyed
endpoint.

Limits of the evidence: one host, so the "direct" addresses are private ones
(on the internet the candidates would also include public addresses; not
reproduced here, since the local relay has address discovery disabled). A stranger running a modified client
could read the candidate list directly; the paths above are the strictly
weaker view of an unmodified iroh.

## Conclusion

- **Leak confirmed, then fixed.** Any third party that derived the rendezvous
  ID could obtain the creator's direct IP addresses without a `hello`. The
  pairing exchange is a few kilobytes, so `pairing::open` now binds the
  rendezvous with `network::bind_rendezvous`: a keyed endpoint with IP
  transports removed, whatever `--relay-only` says. Slot and transfer
  endpoints are unchanged and keep direct paths. Consequence: the creator's
  addresses reach the joiner only through the transfer endpoint, after
  authentication.
- **Cost.** Pairing always crosses a relay, which it already did to meet.
  `--direct-only` is refused for codes, so there is no direct-only fallback to
  keep. If the relay cannot be reached the rendezvous fails instead of
  continuing with direct addresses only.
- **Who sees what, per party.**
  - Relay operator: both endpoint IDs, their IP addresses as the relay
    sees them, and traffic volume. The pairing frames are end-to-end
    encrypted by QUIC/TLS to the rendezvous key, which the relay can derive
    only from the words.
  - n0 DNS (public relays): the rendezvous ID and the creator's home relay
    URL, not its addresses. DNS names are publicly enumerable.
  - Stranger who knows `R`: can connect, learns no address of the creator,
    can burn the attempt with a bogus `hello`.
- **Squatter of `R` (joiner side): fixed below.** Before the fix the joiner
  dialed with its transfer endpoint, which has UDP transports. A party that
  registered the rendezvous ID first, with direct transports, accepted the
  joiner's connection and learned the joiner's ephemeral endpoint ID, its home
  relay, its direct addresses (candidates in the handshake and the source of
  its probes; the "target's paths" column of the first row is exactly this
  view) and the `hello`. It cannot derive the password from these: it must
  know or guess words 1–3 and be registered at the relay before or alongside
  the creator (see [iroh upstream request 1](iroh-upstream-requests.md): a
  second registration of an ID displaces the first at iroh-relay).

## Joiner side (fixed)

The joiner's endpoint ID is bound into SPAKE2 (`idB`) and the creator serves
only that ID, so the transfer endpoint could not simply become relay-only.
The pairing and the transfer are now two endpoints:

1. The joiner pairs from a temporary relay-only endpoint
   (`network::bind_joiner`: `clear_ip_transports` plus, with public relays,
   the n0 address lookup that reaches the rendezvous by ID). Its ID is `idB`,
   authenticated by TLS on the creator's side as before. It is bound
   concurrently with the transfer endpoint (`pairing::prepare_join`) and closed
   when the pairing ends.
2. For `files` and `chat`, the joiner announces its transfer endpoint in the
   `payload` frame (`{"type":"payload","endpoint":"<64 hex digits>"}`), after
   key confirmation and the `ready` frames. The transcript covers it, so both
   `done` MACs do; the creator reads and checks it at once, but uses it only
   after verifying the joiner's `done`, and an invalid key ends the pairing.
3. The creator serves blobs and admits the chat only from the announced
   endpoint. The rendezvous connection's remote ID, the temporary endpoint, is
   refused. A resumed reception reuses the key saved in its partial directory,
   which is the key announced on the first run.
4. `pair` pairs from the same temporary endpoint; machine records are unchanged
   and nothing is announced.
5. The transfer endpoint no longer needs address lookup: the creator's ticket
   carries its relay and addresses, and a resumed reception already ran without
   it. The lookup remains on the temporary endpoint, which must resolve the
   rendezvous ID.

### What was run

`pairing::session::tests::joiner_exposes_no_direct_address_to_a_squatter`
(ignored, local `iroh-relay`, run by `mise run test:pairing` and CI). A
squatter with direct transports binds the rendezvous key derived from the words
of a fresh code and stays silent. A real joiner (`join`, the code path of
`receive`, `chat connect` and `pair`) dials it, and the test lists the
squatter's `Connection::paths()` after six seconds.

| Joiner | Squatter's paths | Joiner's advertised IP addresses |
| --- | --- | --- |
| Control and old behavior: regular endpoint (`bind_resolving`, direct and relay) | relay, **`ip:172.17.0.1:…`** | 3 |
| Now: `prepare_join` (relay only) | relay only | 0 |

The same test with `bind_joiner` temporarily left without `relay_only` (the
old behavior) fails: the pairing endpoint advertises 3 IP addresses instead of
0. The control row, which is the same joiner built by hand, shows the squatter
getting the direct path, so the path assertion is not vacuous either.

What a squatter now learns: the joiner's temporary endpoint ID (a random key
used once), its home relay, and the `hello` (protocol version, purpose, whether
a confirmation number is expected, a SPAKE2 message). Nothing else: no IP
address, and not the endpoint the joiner transfers from, which exists only
inside the authenticated channel. The transfer endpoint reaches the creator
directly only after authentication, using the creator's ticket.
