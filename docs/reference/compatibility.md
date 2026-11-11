# Compatibility and limitations

iroh-toolbox speaks its own protocols over iroh. It does not exchange data
with [sendme](https://github.com/n0-computer/sendme) or
[dumbpipe](https://github.com/n0-computer/dumbpipe), and no command connects
with a ticket from either tool. `ticket inspect` only decodes their tickets
offline, and `ping connect` parses dumbpipe's. Files still use the standard
iroh-blobs ALPN and collection format, so the content layout is the same; what
differs is who may ask for it.

## Protocols

The native engine is pinned to `iroh 1.4.1` and `iroh-blobs 0.104.1` in
`Cargo.lock`. Every ALPN the toolbox uses:

| ALPN | Used by |
| --- | --- |
| `iroh-toolbox/pairing/1` | Rendezvous of the codes of `send`, `receive`, `chat serve`, `chat connect` and `pair` ([protocol](pairing-protocol.md)) |
| `/iroh-bytes/4` (iroh-blobs) | File contents of code transfers and chat file offers, served only to the paired identity. Between paired machines, files use iroh-blobs on the authenticated slot connection itself, and a chat slot accepts this ALPN from its participant's endpoint only |
| `iroh-toolbox/chat/1` | Chat messages of a chat joined by code |
| `iroh-toolbox/slot/2/tunnel-tcp` | `tunnel tcp` ([slot handshake](slots.md#slot-handshake), then one service request per stream) |
| `iroh-toolbox/slot/2/tunnel-http` | `tunnel http` (handshake, a `connect` request, then HTTP/1.1) |
| `iroh-toolbox/slot/2/pipe` | `pipe` |
| `iroh-toolbox/slot/2/inbox` | `receive --inbox` and `send PATH peer:NAME` (handshake, then file control messages and blobs) |
| `iroh-toolbox/slot/2/send` | `send PATH --peers-only` and `receive peer:NAME` |
| `iroh-toolbox/slot/2/chat` | `chat serve` and `chat connect peer:NAME` (handshake, then chat messages on the same stream) |
| `iroh-toolbox/slot/2/ping` and `iroh-toolbox/slot/2/bench` | `ping` and `bench` on a slot |
| `iroh-toolbox/ping/1` and `iroh-toolbox/bench/1` | `ping` and `bench` in ticket mode |

Each forwarded TCP connection opens its own iroh connection and completes the
slot handshake; listing the services uses one more. After the handshake, the
client opens a bidirectional stream per request, and the server accepts
several on one connection. A request and its reply are each a 32-bit
big-endian length followed by a JSON object of at most 16 KiB, tagged by
`type`; unknown fields and types are rejected:

| Request | Reply |
| --- | --- |
| `{"type":"list"}` | `{"type":"services","services":[{"name":"web","port":3000}]}`: names and announced client ports, never destinations |
| `{"type":"connect","service":"web"}`, or without `service` for the slot's sole service | `{"type":"connected"}`, then raw bytes in both directions |

A request that cannot be served receives
`{"type":"refused","reason":REASON}`, with `unknown_service`,
`service_required` (no service named on a slot with several), `unreachable`
(the destination TCP connection failed) or `unsupported`; the server then
finishes that stream and keeps the connection. `tunnel http` accepts only
`{"type":"connect"}`, after which the serving machine answers HTTP/1.1 itself
on the stream. Unix sockets and custom ALPNs are not supported.

## sendme and dumbpipe

Both are examples by n0 for iroh. Their documented behavior, read from their
sources (versions 0.37.1 and 0.40.1) on 2026-11-01, differs from the model
here on one point that matters for access:

- **sendme** prints a ticket that holds the sender's address, the collection hash
  and its format (`BlobTicket`). Its provider serves the iroh-blobs ALPN with an
  event mask that only notifies of connections and requests
  (`ConnectMode::Notify`, `RequestMode::NotifyLog`) and has no client allowlist
  in its source; its README says it runs until interrupted with Control-C.
- **dumbpipe** prints a ticket that holds the listener's endpoint address
  (`EndpointTicket`). Its listeners use the ALPN `DUMBPIPEV0`; the connecting side
  sends the five bytes `hello`, and the listener accepts any client that sends
  them, with no other client check in its source. Its README says that any number
  of connections can flow through a `listen-tcp` pipe. It also forwards Unix
  sockets and accepts a custom ALPN.

In both, the ticket is an address, and their servers accept any client that
reaches the endpoint. iroh-toolbox instead serves only the machine that used a
one-use code (files, chat) or a paired machine that a running server admits
(tunnels, pipes, inboxes, `send --peers-only`, chat), optionally narrowed by
that server's `--allow`, and has no ticket mode for these commands. There is no
`DUMBPIPEV0` server here, and `send`, `chat`, `tunnel` and `pipe` print no
ticket that sendme or dumbpipe could use. Exchange with either binary is not part of the
test suite and not a goal.

## Relays, direct connections and limits

The toolbox uses iroh's built-in production public relay map by default. Slot
servers publish their relay through n0's DNS, and codes use pkarr to publish the
temporary rendezvous address. A custom relay URL replaces that map with the selected
relay only; private credentials stay scoped to it. Tickets are checked against
the selected map. A different custom relay requires explicit configuration.
`--direct-only` disables iroh relays entirely and requires direct addresses
(`--addr` for a `peer:` target, or a full ticket for `ping` and `bench`). With relays enabled, iroh can still establish direct UDP connections.
To disable IP transports on **both peers**, using public relays by default or
the custom relay you selected:

```sh
iroh-toolbox --relay-only ping serve
# On the other peer:
iroh-toolbox --relay-only ping connect 'endpoint…'
```

For file transfers:

- Payload is limited to 10 GiB by default; change it with `receive --max-bytes N`.
  Collection metadata has a separate 32 MiB limit.
- The transfer timeout is one hour by default, configurable with
  `--transfer-timeout SECONDS`. Connection/registration timeout is separate.
- Temporary disk space is required for the sender's snapshot and receiver's
  verified blobs and staged files. An interrupted `receive` keeps its verified
  data in `.iroh-partial-…` and resumes with the same command while `send` runs;
  between paired machines, resumption is bound to the sender's machine, the
  offer and the destination. Chat receptions are not resumable.
- An inbox receives one transfer at a time and keeps at most 8 senders
  waiting; `send --peers-only --share` serves at most 8 receivers at once.
- Existing names trigger a question before download; without an interactive
  stdin they are refused. `--force` replaces regular files and merges
  directories without asking, preserving unrelated files; `--no-replace`
  always refuses. An inbox never asks: it refuses existing names, recognizes
  identical files and refuses `--force`. Type conflicts and links remain errors. Publication is staged after
  verification, with replacement backups and rollback on filesystem errors,
  but a directory is not published atomically as a whole. See
  [replacement and recovery](../guides/files.md#replace-existing-files).
- The destination filesystem must support hard links (for example, ext4 or NTFS).
  FAT/exFAT destinations are not supported.
- Links, special files, unsafe paths, Windows reserved names, and case-insensitive
  name collisions are refused. Permissions and timestamps are not restored.

## Known limitations

- **Pairing needs a relay, once.** Codes meet through a relay (public by
  default, or a local one with `--relay`); `--direct-only` is refused. After
  pairing, peer services work without one; a `chat serve` that prints a code
  still needs one (`chat serve --peers-only` does not).
- **Without a relay**, peer services need `--direct-only` with `--port` on
  the server and `--addr HOST:PORT` on the client, passing the address the
  server prints.
- **No local network discovery yet:** `pair` and connections without `--addr`
  cannot find a machine on the same network by themselves.
- **No delivery to offline machines:** `send PATH peer:NAME` needs a running
  `receive --inbox`, and `receive peer:NAME` a running `send --peers-only`.
  Chat has two participants.
- Rotating an address does not notify peers: pair again with each of them.

## Project status

iroh-toolbox is pre-1.0 software. Commands and wire compatibility may evolve;
breaking changes increment the minor version. The [changelog](../maintainers/changelog.md)
records user-visible changes and known limitations.

### Test coverage

Automated tests run the native CLI over real local sockets. Slots, files and
chat between paired machines run over direct loopback connections; code
pairing, slots and private-relay scenarios also run through a local official
relay, on Linux and Windows; every release archive is installed with its
install script before publication. Transfers across separate networks and deployed HTTPS relay
proxies are outside the automated suite: mention your environment when you open
an issue.
