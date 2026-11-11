# Machine identity and slots

A **slot** is a named server of a machine: `tunnel tcp serve`, `pipe serve`,
`receive --inbox` and `send PATH --peers-only` always run on one, `chat serve`
does when the machine has paired machines, and `ping serve` / `bench serve`
can. A paired machine reaches it with `peer:NAME/SLOT`, and the slot keeps its
address across restarts. Several slots run at once in independent processes,
with no central process and no directory or DNS of your own.

```console
nas$ iroh-toolbox tunnel tcp serve --to 127.0.0.1:22 --slot ssh --allow laptop
Serving slot ssh to: laptop
On the other machine, run:
  iroh-toolbox tunnel tcp connect peer:nas/ssh --bind 127.0.0.1:8080
Use the name this machine has in that machine's `iroh-toolbox peers` list if it differs.
TCP service 22 -> 127.0.0.1:22 (client port 8080)

nas$ iroh-toolbox pipe serve --slot backup --allow all   # runs alongside the tunnel

laptop$ iroh-toolbox tunnel tcp connect peer:nas/ssh --bind 127.0.0.1:2222
laptop$ iroh-toolbox ping connect peer:nas       # default slot "ping"; needs ping serve --slot on nas
```

Before anything works, the two machines must be [paired](peers.md). A server
then accepts the paired machines that its `--allow` names (`--allow all`: every
one of them); tunnels and pipes refuse to start without it, other servers
accept every paired machine without it. There is no per-slot grant. The machine address behind a slot is internal: no command
prints or accepts it.

## Slots

| Option | Effect |
| --- | --- |
| `--slot [NAME]` | On `tunnel tcp serve`, `tunnel http serve`, `pipe serve`, `receive --inbox`, `send PATH --peers-only`, `chat serve`, `ping serve` and `bench serve`. Without a name, the slot is the command's default (below) |
| `--allow NAME` | Serve only these paired machines (repeatable or comma-separated), or `--allow all` for every paired machine, read at each connection; `all` cannot be combined with names. Required on `tunnel tcp serve`, `tunnel http serve` and `pipe serve`. On `tunnel tcp serve`, `tunnel http serve`, `pipe serve`, `receive --inbox`, server-mode `send`, `chat serve`, `ping serve` and `bench serve`. On `send`, `chat serve`, `ping serve` and `bench serve` it implies `--peers-only`: no code or ticket |
| `--peers-only` | On `ping serve` and `bench serve`: imply a slot, ask for the client proof and serve any paired machine. Without it, a ping/bench slot is open to anyone who knows its address, a removed peer included. On `send` and `chat serve` it means "no code" |
| `--port PORT` | Fixes the UDP port of the server endpoint, with or without a relay; useful for a firewall rule. A port already in use is an error |

| Server | Default slot | Client without `/SLOT` |
| --- | --- | --- |
| `tunnel tcp serve` | `tunnel` | `tunnel tcp connect peer:NAME` |
| `tunnel http serve` | `http` | `tunnel http connect peer:NAME` |
| `pipe serve` | `pipe` | `pipe connect peer:NAME` |
| `receive --inbox` | `inbox` | `send PATH peer:NAME` |
| `send PATH --peers-only`, with or without `--share` | `send` | `receive peer:NAME` |
| `chat serve`, with or without `--peers-only` | `chat` | `chat connect peer:NAME` |
| `ping serve --slot`, `bench serve --slot` | `ping`, `bench` | `ping connect peer:NAME`, `bench connect peer:NAME` |

A server on its default slot does not name it: it prints `Serving to: laptop`
and the client command without `/SLOT`. Another slot is named, as in
`Serving slot ssh to: laptop`.

`ping serve` and `bench serve` without `--slot` or `--peers-only` stay
ephemeral: they print a one-time ticket, as described in
[diagnostics](../guides/diagnostics.md). `chat serve` with an empty peer book
opens no slot: it is a code-only chat, and refuses `--slot` and `--port`.

Slot names use 1 to 32 lowercase letters, digits or hyphens; `all` and `none`
are reserved. A name designates one server on the machine: a second process
asking for a slot already served receives an error. A slot name does not change
the protocol: connecting with the wrong slot or the wrong command (for example
`receive peer:nas/equipe` against a chat slot) fails with the generic
`unknown slot or protocol` message, which names neither.

Some servers admit one client or transfer at a time; the others are told the
server is busy, never that it is absent. `pipe serve` and `bench serve` serve
one client, and refused attempts do not stop them. An inbox receives one
transfer at a time and keeps at most 8 senders waiting. `send --peers-only`
serves its first admitted receiver until it confirms; with `--share` it serves
up to 8 receivers at once. A chat has one participant. See the
[files](../guides/files.md) and [chat](../guides/chat.md) guides.

## Connection targets

| Target | Accepted by |
| --- | --- |
| `code:WORDS`, `code:` (prompt) | `receive`, `chat connect`, `pair` |
| `peer:NAME[/SLOT]` | `send PATH`, `receive`, `chat connect`, `tunnel tcp connect`, `tunnel http connect`, `pipe connect`, `ping connect`, `bench connect`. `NAME` is a local name from `iroh-toolbox peers`; without `/SLOT`, the command's default slot |
| `peer:NAME/SLOT/SERVICE` | `tunnel tcp connect` only: one [named service](../guides/tunnels.md#several-services-on-one-slot) of the slot |
| ticket | `ping connect`, `bench connect`, `ticket inspect` |

`id:` targets and tickets for tunnels and pipes were removed: `tunnel tcp
connect` and `pipe connect` say that they connect to paired machines only.
`send`, `receive` and `chat connect` refuse tickets.

## Without a relay

Access control does not depend on the path: the slot handshake authenticates
both machines whether packets go through a relay or not. A relay only helps the
client find the server, so a known address can replace it:

```console
nas$ iroh-toolbox tunnel tcp serve --to 127.0.0.1:22 --slot ssh --allow laptop --direct-only --port 7777
Serving slot ssh to: laptop
Direct addresses: 192.168.1.10:7777
On the other machine, run:
  iroh-toolbox --direct-only tunnel tcp connect peer:nas/ssh --bind 127.0.0.1:8080 --addr 192.168.1.10:7777
laptop$ iroh-toolbox tunnel tcp connect peer:nas/ssh --direct-only --addr 192.168.1.10:7777
```

- With `--direct-only`, a slot server registers with no relay, publishes
  nothing and prints its direct addresses (loopback addresses last). This
  applies to every peer service: tunnels, pipes, inboxes, `send --peers-only`,
  `chat serve --peers-only` and slot ping/bench servers.
- `--addr HOST:PORT` (repeatable) on `send PATH peer:NAME`, `receive
  peer:NAME`, `chat connect peer:NAME`, `tunnel tcp connect`, `tunnel http
  connect`, `pipe connect`,
  `ping connect` and `bench connect` adds direct addresses to a `peer:` target.
  With a relay it is a shortcut to the direct path; with `--direct-only` at
  least one is required, or the client says "the server cannot be found without
  a relay; pass --addr HOST:PORT printed by the server". `--addr` is refused
  with a ticket or a code.
- `pair` still needs a relay, once: pair with a relay (public, or a local one),
  then peer services work without one. A `chat serve` that also prints a code
  needs a relay for that code; `chat serve --peers-only` does not. There is no
  local network discovery yet, so the address and port must be passed by hand.

## How it works

| Element | Role |
| --- | --- |
| **Machine key** `M` | Ed25519 key identifying the machine. It only signs proofs and **never binds an iroh endpoint**, so it never conflicts at a relay. |
| **Address salt** `s` | 10 random bytes, exchanged by `pair` and stored in the peer book; without them, the machine's slots cannot be located. |
| **Slot key** `E` | `HKDF-SHA256(ikm = s, salt = "iroh-toolbox/slot/v1", info = M_public ‖ slot)`. A peer computes it from what it stored when pairing. |

A slot process binds an iroh endpoint with `E`. With public relays, `E`
publishes its relay through n0's DNS (iroh's standard lookup); with `--relay`,
the client dials `E` at that relay and nothing is published. Relays and DNS
never see `M` or slot names: the slot name travels only inside the encrypted
connection. The server does not tell a stranger whether the slot or the protocol
was wrong: both get the same generic `unknown slot or protocol` refusal.

Anyone who learns `E` (a relay sees it) can open a connection to the slot, so
the TLS identity of a slot proves nothing. The [handshake](#slot-handshake)
below is what authenticates both ends.

## Slot handshake

Every connection starts with the handshake on its first bidirectional stream,
under the ALPN `iroh-toolbox/slot/2/<protocol>` (`tunnel-tcp`, `tunnel-http`,
`pipe`, `inbox`, `send`, `chat`, `ping` or `bench`). Frames are JSON objects with a `type` field
and unknown fields rejected, prefixed by a 16-bit big-endian length (at most
1 KiB); bytes are hex strings.

```text
1. C → S  hello   { version: 2, slot, nonce_c }
2. S → C  proof   { signature: Sign_MS(server statement), nonce_s, client }
          C verifies the signature against the machine key MS stored in its peer book
3. C → S  client  { machine: MC, signature: Sign_MC(client statement) }   when the server asks
          S verifies the signature, then checks that MC is in its peer book
          and that --allow lists it when this server has one
4. S → C  ready   { }

server statement = "iroh-toolbox/slot-auth/v2" ‖ len(protocol) ‖ protocol ‖ len(slot) ‖ slot
                   ‖ E ‖ client endpoint ID ‖ nonce_c
client statement = "iroh-toolbox/peer-auth/v1" ‖ len(protocol) ‖ protocol ‖ len(slot) ‖ slot
                   ‖ E ‖ client endpoint ID ‖ nonce_s
```

Lengths are one byte. Nonces are 32 random bytes. No service byte is exchanged
before `ready`. After it, the handshake stream carries a pipe's data, the
control messages of the file services and the chat messages; tunnels, ping and
bench open their own streams.

- **The server alone decides whether step 3 is required**, from the protocol it
  serves: tunnel, pipe, inbox, send and chat servers always require a valid
  client frame, `ping` and `bench` servers only with `--peers-only` (implied by
  `--allow`). The `client` field of the proof tells the client whether to send
  its frame; the ALPN or slot the client announces never waives it.
- **The server does not send its machine key.** The client verifies against the
  key it stored when pairing, so a stranger who reaches an endpoint and guesses
  a slot name learns nothing that links the machine's slots. A proof that does
  not verify is a local verdict: the client sends neither its own proof nor
  service data, and no close code sent afterwards replaces that verdict. A
  client trusts a refusal (codes 30, 32, 33, and busy 22) only after this
  verification; before it, the refusal reason is reported as untrusted.
- **Order of checks on the server:** ALPN and slot (21), then the client's
  signature (33), then the peer book (30), then `--allow` (32). The signature
  is verified **before the peer book is read**, so a client that does not hold
  a machine key learns neither whether a key is paired nor whether this server
  would admit it: an invalid signature gets code 33 whether the claimed key is
  paired or not. The book is read for every connection with a valid client
  proof; an open ping/bench server never reads it.
- The client endpoint ID is authenticated by iroh's TLS, so a proof cannot be
  relayed to another connection, and `nonce_s` prevents replay. Each side signs
  under its own domain string, so a signature made for one cannot serve as the
  other. Signatures use Ed25519 strict verification.
- **Budget and deadline.** An unauthenticated connection has 5 seconds to
  finish its handshake, independently of `--timeout`, and must send its `hello`
  within `max(2 s, 4 × RTT)`, capped at 5 s. Servers run at most 128 handshakes
  at once; these have a budget of their own, so strangers cannot use up the
  `--max-connections` of authenticated clients. When the budget is full, new
  attempts go through QUIC address validation and evict the oldest handshake
  that has not sent its `hello`. After the server proof, a client that must
  prove itself sends its `client` frame within `max(1 s, 3 × RTT)`, capped at
  5 s. Handshakes past their `hello` may hold at most half of the budget and
  at most 8 from one direct source (a whole /64 for IPv6; relayed connections
  have no per-source cap). Beyond that a random one of them is evicted and
  closed with code 23, so stalled `hello` frames can slow a server but never
  pin it.
- **Retries.** A client tries again, a few times with a short backoff and
  within `--timeout`, when the connection could not be opened or was dropped
  without a reason before the server proof, or was closed with code 23
  (`CROWDED`) at any point. A timeout or a verdict (failed proof, refusal with
  a reason) is final. A client also runs at most 4 handshakes at once toward
  the same slot, so a burst of local connections does not crowd the server's
  budget.
- **Busy.** Code 22 is sent only to a client that passed the server's checks.
  Its reason is mapped to a fixed local text; the received text is never shown.

QUIC application close codes explain a refusal to the client. `NAME` and
`SLOT` come from the client's own target, never from the server:

| Code | Meaning | Client message |
| --- | --- | --- |
| 21 | Unknown slot, protocol or version, sent before the server proves itself | `` NAME refused the connection: unknown slot or protocol; check that NAME serves slot SLOT with `iroh-toolbox COMMAND` `` (with `--slot SLOT` for a named slot, and `--slot` for ping and bench) |
| 22 | Server busy (authorized clients only) | `… is already serving another client; try again later` (pipe, bench), `… is in a chat session with another participant; try again later` (chat), `… is at its connection limit; try again later` (`--max-connections`), else `… is busy; try again later`. The subject is `NAME`, or `the server` once the handshake completed |
| 23 | Handshake dropped to make room for newer ones; the client retries | `NAME is handling too many connections at once; try again` |
| 30 | Valid client proof, machine not in the peer book | ``NAME does not know this machine; run `iroh-toolbox pair` on both machines`` |
| 32 | Paired, not listed by `--allow` | `this NAME server (slot SLOT) does not allow this machine (--allow)` |
| 33 | Invalid client proof (key or signature does not verify), decided before any peer-book lookup | `NAME rejected this machine's authentication proof (invalid client proof); check the local identity and protocol implementation` |
| 1 | Another frame in place of the client proof, or the server cannot read its peer book | `the authenticated handshake with NAME (slot SLOT) did not complete; check the network and NAME's output, then try again` |

Code 31 (the former slot-grant refusal) is retired and never reused. A client
frame that never arrives, or a deadline, ends the handshake without `ready`;
it is never reported as an invalid proof. Before the server proof, codes 22,
30, 32 and 33 give `cannot authenticate NAME: the server did not provide a
valid proof; its refusal reason is untrusted`, and a proof that does not verify
gives `cannot authenticate NAME: its response does not match the expected
identity or handshake context; check the target and whether NAME rotated its
identity`. That failure does not prove that the key changed: see the
[diagnostics table](troubleshooting.md#a-tunnel-or-pipe-is-refused) for what each
message proves and what to do.

## Identity file

Created on first use, readable by the user only (mode 0600 on Unix; the file is
refused if another user owns it or others can read it, with a `chmod 600` hint),
in the configuration directory (mode 0700, tightened once with a note if it was
more open; lock files are 0600). On filesystems without POSIX modes (WSL
`/mnt/c`, exFAT, some Docker mounts) a warning is printed and only the owner is
checked. The directory is: `$XDG_CONFIG_HOME/iroh-toolbox/` (else
`~/.config/iroh-toolbox/`) on Linux, `~/Library/Application Support/iroh-toolbox/`
on macOS, `%APPDATA%\iroh-toolbox\` on Windows. `IROH_TOOLBOX_HOME` selects
another directory. It holds:

| File | Content |
| --- | --- |
| `identity` | The machine key and address salt: **a secret**, optionally [encrypted](#identity-file-format) |
| `name` | The name announced when pairing (`id rename`) |
| `peers.toml`, `peers.lock` | The [peer book](peers.md#peer-book) and its lock |
| `slots/` | One lock file per running slot |

The identity file is a secret. Never sync it with a cloud drive or dotfiles
repository, share it, copy it to another machine or commit it: anyone who has
it is that machine for all its peers. If it may have leaked, run `id rotate`
(see [incident response](peers.md#after-an-incident)).

### Identity file format

TOML, written atomically (mode 0600). Version 1, the default, holds the machine
seed in clear:

```toml
version = 1
machine_seed = "<64 hex digits>"
salt = "<16 z-base-32 characters>"
```

`id protect` writes version 2, which replaces `machine_seed` with an
XChaCha20-Poly1305 ciphertext of the 32-byte seed under a key that Argon2id
(version 0x13) derives from the passphrase:

```toml
version = 2
kdf = "argon2id"
memory_kib = 65536       # at least 65536 (64 MiB); at most 1048576
iterations = 3           # at least 3; at most 20
parallelism = 1          # always 1
kdf_salt = "<32 hex digits>"      # 16 random bytes
nonce = "<48 hex digits>"         # 24 random bytes
ciphertext = "<96 hex digits>"    # 32-byte seed + 16-byte tag
salt = "<16 z-base-32 characters>"  # the public address salt, readable but authenticated
```

The associated data is the ASCII string
`iroh-toolbox/identity/v2/argon2id/xchacha20poly1305` and a zero byte, then the
version, `memory_kib`, `iterations` and `parallelism` as 32-bit big-endian
integers, the 16 `kdf_salt` bytes and the 10 address-salt bytes. `id unprotect`
writes version 1 again, and version 1 files keep loading. See the
[security model](security.md#the-identity-file-is-a-secret).

## Behavior and limits

- **Restarts:** a restarted slot keeps its address. A new client connects at
  once; a client that was already running reconnects within about 15 seconds,
  the time iroh takes to abandon the previous process's network path.
- **No fixed number of slots:** each active slot keeps its own relay connection.
- `id rotate` takes effect for slots started afterwards; restart running slots.

## Threats

| Threat | Outcome |
| --- | --- |
| Guessing an address | Addresses are never shown; the 80-bit salt makes slot keys infeasible to guess |
| Endpoint ID known (relay, log) | The handshake refuses a machine not in the peer book, and a client learns nothing about the book |
| Paired machine, `--allow all` or no `--allow` on a server that allows it | Admitted: no per-slot grant exists. Tunnels and pipes do not start without `--allow` |
| Paired machine outside `--allow` | Refused (code 32) |
| Invalid client signature, for a paired or an unknown key | Code 33 before any peer-book lookup: nothing about book membership is disclosed |
| Forged refusal before the server proof | Reported as untrusted, never as the named peer's decision |
| Address known to a removed or lost machine | It can disrupt a slot by registering `E` at the relay, which keeps the newest connection, or by replacing the DNS record of `E`: denial of service only. The server warns and notes when the slot is back ([after an incident](peers.md#after-an-incident)). Remedy: `id rotate --address`, then pair again |
| Identity file leaked | Full impersonation of the machine to its peers, unless the key was [protected](security.md#the-identity-file-is-a-secret) with a strong passphrase. Remedy: `id rotate`, then `peers rm` on the peers and pair again |
| Replay or relaying impostor | Rejected by the nonces and the client ID in the signatures |
| Client announcing `ping` on a tunnel slot | Rejected: the server requires the proof its own protocol needs |
| Many silent connections or stalled `hello` frames | Dropped after the `hello` or `client` deadline; separate budget of 128 handshakes, address validation when full, stalled `hello` frames capped at half the budget and 8 per direct IP. Someone who knows the slot endpoint and its name can degrade service, not block it: prefer a non-default slot name for an exposed `--port`, and `id rotate --address` after losing a peer |
| Two local processes on one slot | Refused by the slot lock |
| Tracking | Only `E` is visible to relays and DNS |

A paired machine that a running server admits reaches the forwarded service as
a local client would. Keep authentication enabled on the service itself.
