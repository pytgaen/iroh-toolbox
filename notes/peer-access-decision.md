# Peer access: problem, alternatives and decision

Decided on 2026-11-01 from finding P2 of the 2026-10-31 code review
(`cr-session-2026-10-31.md`). This note keeps the alternatives that were
considered so that compatibility or ticket modes can be revisited later without
redoing the analysis.

## Problem

A ticket contains the server's endpoint ID and addresses, nothing secret. The
endpoint ID alone is enough to connect: `network::target` accepts a bare ID and
adds the default relays (`src/network.rs`, `target`). Every relay the server
registers with sees that ID, and it also appears in logs, screenshots and bug
reports. Someone who never saw the ticket can therefore connect:

| Command | Behaviour with a stranger who knows the endpoint ID | Impact |
| --- | --- | --- |
| `tunnel` | Accepted in parallel, up to `--max-connections` | Reaches the exposed TCP service from the server's loopback, bypassing firewalls and `localhost`-trusting ACLs |
| `pipe` | First connection wins (`network::accept_first`) | Reads the server's stdin or writes to its stdout |
| `send TICKET` | First receiver is pinned at connection time, before it proves the collection hash (`files.rs`, `accept_receiver`) | Blocks the transfer; content still needs the hash |
| `chat TICKET` | First valid connection is pinned | Takes the place of the intended correspondent |
| `ping`, `bench` | Accepted | Bandwidth and CPU only |
| Slots (`id:`) | Same as the command; the slot name is guessable | Same impact, on a stable address |

Slot endpoint keys are `HKDF(salt, machine || slot)` (`identity.rs`,
`slot_key`): the relay sees the derived endpoint ID but not the salt, and the
slot `Hello` (`slot.rs`, `Server::authenticate`) authenticates only the server.

## Alternatives considered

1. **Secret in every ticket.** A random 256-bit secret, sent by the client on
   the first stream after the QUIC handshake (never in ALPN or the ClientHello:
   QUIC Initial packets are only obfuscated), compared in constant time, before
   any service access or pinning. Fixes the relay vector for tickets. Leaves:
   leaked tickets are reusable while the server runs, residual DoS on pending
   handshakes, no per-client identity or revocation.
2. **Pairing codes for `tunnel` and `pipe`.** `pipe serve --code` was built
   then removed on 2026-10-28: stdin is usually redirected, so `--verify` is
   unusable. A code also needs someone at the server for every client restart.
3. **Peers with aliases** (`pair`, `peer:NAME[/slot]`, `--allow`). The server
   checks a client proof signed by the client's machine key: identity instead
   of possession, revocable, stable across client restarts.
4. **Slots: client proves the address salt.** Keeps the `id:` format; a leaked
   address is handled by `id rotate --address`.
5. **`send`: pin after the hash request** instead of at connection, using
   iroh-blobs `RequestMode::Intercept`. Keeps sendme compatibility. iroh-blobs
   also allows authenticating the connection before `BlobsProtocol::accept`,
   since `provider::handle_connection` does not check the ALPN.
6. **dumbpipe compatibility as an explicit opt-in** (`--dumbpipe`, plain
   endpoint ticket, warning). A server cannot offer both the secret and the
   legacy path: the legacy path would bypass the secret.

## Decision

Release a secure MVP first; compatibility can be added later.

- `send` and `chat` work with pairing codes only; ticket modes are removed.
- `pipe` and `tunnel` require peering: machines pair once, then servers accept
  only paired machines. Peers ship in 0.1.0.
- Every paired machine has a local name, chosen at pairing time and renamable.
  The confirmation number is mandatory for `pair` (no `--no-verify`).
- Access is per slot and per direction: at pairing, each machine chooses which
  of its own slots the other may reach (`none` by default, `all`, or a list),
  changed later with `peers access`. A `tunnel` or `pipe` server accepts a
  paired machine only on a granted slot; the optional `--allow NAME` narrows
  one server further and never grants. This replaces an earlier rule where
  pairing was mutual trust for every slot: an impostor's code would then have
  opened every tunnel.
- `ping` and `bench` slots stay open to any paired machine, without access
  control: they carry no data.
- `ping`, `bench`, `relay check` and `ticket inspect` keep their current
  behaviour: they expose diagnostics, not data or services.
- Machine addresses (`id:`) become internal: exchanged by `pair`, stored in
  the peer book, never printed or accepted as targets. `id` shows this machine
  (name, short key, peers), `id rename` sets the announced name, and
  `id rotate [--address]` is the incident response.
- Wire compatibility with sendme and dumbpipe is not a goal. The documentation
  names them factually in `docs/reference/compatibility.md`, after checking
  their upstream sources.

## Reopening compatibility later

Any ticket or legacy mode added back must not weaken the default: it needs an
explicit flag, a warning, and at least alternative 1 or 5 for the mode
concerned.
