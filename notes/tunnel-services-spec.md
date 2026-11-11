# Spec: named TCP services on tunnel slots (`tunnel tcp`)

Status: implemented on **2026-11-07** (specification revised on 2026-11-03).

## Goal

A `tunnel tcp serve` slot exposes one TCP destination or a **group of named
services**. A client opens the whole group without repeating every port, picks
one service with `peer:NAME/SLOT/SERVICE`, or overrides local listeners with
`--bind SERVICE=HOST:PORT`.

Both sides use the same `SERVICE=HOST:PORT` convention. This replaces the
asymmetric `--to PORT=HOST:PORT` / `--bind HOST:PORT=REMOTE_PORT` pair. No
version has been published yet, so the `iroh-toolbox/slot/2/tunnel-tcp` and
`iroh-toolbox/slot/2/tunnel-http` exchanges change in place, without an ALPN
bump.

## User experience

```console
# 1. Single service: the documented commands keep working
nas$ iroh-toolbox tunnel tcp serve --to 127.0.0.1:3000
On the other machine, run:
  iroh-toolbox tunnel tcp connect peer:nas --bind 127.0.0.1:8080
TCP service 3000 -> 127.0.0.1:3000 (client port 8080)
laptop$ iroh-toolbox tunnel tcp connect peer:nas --bind 127.0.0.1:8080
TCP listening on 127.0.0.1:8080
tunnel: local listener ready; peer and destination service unchecked until a local connection; open your application on this port

# 2. Named group on a slot
nas$ iroh-toolbox tunnel tcp serve --slot dev \
  --to web=127.0.0.1:3000 \
  --to db=127.0.0.1:5432
On the other machine, run:
  iroh-toolbox tunnel tcp connect peer:nas/dev
TCP service web -> 127.0.0.1:3000 (client port 3000)
TCP service db -> 127.0.0.1:5432 (client port 5432)

# Whole group on the announced ports
laptop$ iroh-toolbox tunnel tcp connect peer:nas/dev
TCP listening on 127.0.0.1:3000 -> service web
TCP listening on 127.0.0.1:5432 -> service db
tunnel: 2 services of nas/dev ready; destinations are checked at each local connection

# Whole group, one local port overridden
laptop$ iroh-toolbox tunnel tcp connect peer:nas/dev --bind db=127.0.0.1:15432
TCP listening on 127.0.0.1:3000 -> service web
TCP listening on 127.0.0.1:15432 -> service db

# One service only
laptop$ iroh-toolbox tunnel tcp connect peer:nas/dev/web
TCP listening on 127.0.0.1:3000 -> service web
laptop$ iroh-toolbox tunnel tcp connect peer:nas/dev/web --bind 127.0.0.1:8080
TCP listening on 127.0.0.1:8080 -> service web
```

Errors:

```console
laptop$ iroh-toolbox tunnel tcp connect peer:nas/dev/unknown
error: slot dev of nas has no service "unknown"; available: web, db

laptop$ iroh-toolbox tunnel tcp connect peer:nas/dev
error: cannot bind 127.0.0.1:5432 for service db; pass --bind db=127.0.0.1:PORT with a free port: Address already in use

laptop$ iroh-toolbox tunnel tcp connect peer:nas --bind 127.0.0.1:8080=3000
error: --bind 127.0.0.1:8080=3000 uses the old order; write --bind 3000=127.0.0.1:8080

laptop$ iroh-toolbox tunnel tcp connect peer:nas/dev
error: cannot establish an authenticated connection to slot dev on nas: …; to start before the server, pass peer:nas/dev/SERVICE --bind HOST:PORT; if dev is a service of the default slot, use peer:nas/tunnel/dev
```

## `tunnel tcp serve`

`--to [SERVICE=]HOST:PORT` (repeatable, required):

| Form | Service | Client port announced | Destination |
| --- | --- | --- | --- |
| `--to 127.0.0.1:3000` (sole `--to`) | `3000` | `8080` | `127.0.0.1:3000` |
| `--to 127.0.0.1:3000` (among several) | `3000` | `3000` | `127.0.0.1:3000` |
| `--to web=127.0.0.1:3000` | `web` | `3000` | `127.0.0.1:3000` |
| `--to 8001=192.168.1.20:80` | `8001` | `8001` | `192.168.1.20:80` |

Rules:

- **Service names** follow the slot name rules (1 to 32 lowercase ASCII
  letters, digits or hyphens; `all` and `none` reserved), so a `/SERVICE`
  target segment has the same grammar as the other segments. An all-digit name
  must be a canonical port from `1` to `65535` (no leading zero) and sets the
  announced client port.
- **Client port**: the numeric service name, else the destination port. A sole
  unnamed `--to HOST:PORT` announces `8080` instead: a single service usually
  targets a well-known port (22, 80) that the client cannot or should not bind.
- **Uniqueness**: names and announced client ports are distinct within a slot,
  checked at startup. Two destinations on port 80 need a numeric name for at
  least one (`--to 8001=192.168.1.20:80`).
- **Limit**: at most 64 services per slot.
- **Hint**: a sole unnamed `--to` prints
  `tunnel tcp connect peer:NAME[/SLOT] --bind 127.0.0.1:8080`, which stays lazy
  on the client. Every other configuration prints
  `tunnel tcp connect peer:NAME[/SLOT]`.

Destination hosts and ports are never sent to clients.

## `tunnel tcp connect`

### Targets

| Target | Meaning |
| --- | --- |
| `peer:NAME` | Slot `tunnel`, all its services |
| `peer:NAME/SLOT` | Slot `SLOT`, all its services |
| `peer:NAME/SLOT/SERVICE` | Slot `SLOT`, only `SERVICE` |

A service of the default slot is written with the slot explicitly:
`peer:nas/tunnel/web`. Every other command (`pipe`, `tunnel http`, `send`,
`receive`, `chat`, `ping`, `bench`) refuses a third segment.

An unknown slot cannot be told apart from a stopped server. When listing fails
that way on a slot other than `tunnel`, the error adds:
`if SLOT is a service of the default slot, use peer:NAME/tunnel/SLOT`.

### `--bind`

`--bind` has no default value. Its forms:

- `HOST:PORT`: one listener for the selected service. Given at most once and
  never combined with `SERVICE=` forms.
- `SERVICE=HOST:PORT` (repeatable): overrides the listener of one service of
  the group. Refused when the target names a service, and when the same
  `SERVICE` appears twice.
- `HOST:PORT=PORT` (old order): refused with the rewritten form.

Local port `0` allocates a free port and reports it in every form.

| Invocation | Startup | Listeners |
| --- | --- | --- |
| `connect peer:NAME[/SLOT]` | `list` | Every service on `127.0.0.1:<announced port>` |
| `connect peer:NAME[/SLOT] --bind SERVICE=HOST:PORT…` | `list`; unknown `SERVICE` fails | Overridden services on their address, the rest on `127.0.0.1:<announced port>` |
| `connect peer:NAME/SLOT/SERVICE` | `list`; unknown `SERVICE` fails | `SERVICE` on `127.0.0.1:<announced port>` |
| `connect peer:NAME[/SLOT] --bind HOST:PORT` | Lazy | `HOST:PORT` for the slot's sole service |
| `connect peer:NAME/SLOT/SERVICE --bind HOST:PORT` | Lazy | `HOST:PORT` for `SERVICE` |

**`list` at startup**: the client contacts the server only when it needs the
service list. If the server is unreachable within `--timeout`, `connect` fails
and suggests the lazy form. The list is read once: services added on the server
later need a client restart, and removed services are refused at connection
time.

**Lazy forms** keep today's behavior: the client binds immediately, can start
before the server, and checks the peer and service at the first local
connection. A server with several services refuses an unnamed request with
`service_required`, reported as `nas/dev serves several services; name one with
peer:nas/dev/SERVICE or --bind SERVICE=HOST:PORT`. An unknown service is
reported at that point with the available names.

All listeners bind before any readiness message. If one fails, the listeners
already bound are released and the error names the service and the address.

## Wire protocol

Applies to `iroh-toolbox/slot/2/tunnel-tcp` and `iroh-toolbox/slot/2/tunnel-http`.

Every message is sent after the slot handshake (`ready`), so unauthenticated
peers and relays learn nothing about services or ports.

**Connections.** The client opens one iroh connection per forwarded TCP
connection and one for `list`. The server nevertheless accepts several streams
per connection, each carrying one request, until the client closes the
connection or opens no stream for `--timeout`. A future client can then
multiplex without changing the wire. `--max-connections` still counts
authenticated connections. A refused request also leaves the connection open:
a lazy client uses it to list the services for its error message.

**Framing.** On a new bidirectional stream, the client sends one request and
the server one reply. Each message is a 32-bit big-endian length followed by a
JSON object of at most 16 KiB, tagged by `type` (snake_case). Unknown fields
and unknown types are rejected. The server waits for the request at most
`--timeout`.

Requests:

```json
{ "type": "list" }
{ "type": "connect", "service": "web" }
{ "type": "connect" }
```

`connect` without `service` (or with `null`) selects the slot's sole service.

Replies:

```json
{ "type": "services", "services": [ { "name": "web", "port": 3000 }, { "name": "db", "port": 5432 } ] }
{ "type": "connected" }
{ "type": "refused", "reason": "unknown_service" }
```

- `services` answers `list`. `port` is the announced client port.
- `connected` answers `connect` once the server's TCP connection to the
  destination succeeds. Raw bytes then flow in both directions on the stream.
- `refused` answers a request that cannot be served, then the server finishes
  the stream and keeps the connection. Reasons: `unknown_service`,
  `service_required` (unnamed `connect` on a slot with several services),
  `unreachable` (the destination TCP connection failed or timed out) and
  `unsupported` (request not valid on this protocol).

**`tunnel http`** has no services: the client sends `{ "type": "connect" }`,
the server replies `connected` then answers HTTP/1.1 itself on the stream.
`list` and a named `connect` are refused with `unsupported`.

## Non-goals

- Access rules per service: `--allow` applies to the whole slot.
- Client multiplexing of several TCP connections on one iroh connection. The
  server is ready for it; the client keeps one connection per TCP connection.
- Opening a subset of several services in one process: run one `connect` per
  service, or open the group.
- Refreshing the service list while the client runs.

## Acceptance criteria

- `serve --to 127.0.0.1:3000` with `connect peer:nas --bind 127.0.0.1:8080`
  works as today, client started before the server included.
- `serve --to 127.0.0.1:3000` with `connect peer:nas` binds `127.0.0.1:8080`.
- `serve --slot dev --to web=127.0.0.1:3000 --to db=127.0.0.1:5432`:
  - `connect peer:nas/dev` opens `127.0.0.1:3000` and `127.0.0.1:5432` and
    routes each to its destination;
  - `connect peer:nas/dev --bind db=127.0.0.1:15432` opens `127.0.0.1:3000`
    and `127.0.0.1:15432`;
  - `connect peer:nas/dev/web` opens only `127.0.0.1:3000`;
    `--bind 127.0.0.1:8080` moves it;
  - `connect peer:nas/dev --bind 127.0.0.1:8080` starts, and the first local
    connection reports `service_required`;
  - `--bind db=127.0.0.1:0` reports the allocated port.
- `peer:nas/dev/unknown` and `--bind unknown=127.0.0.1:8080` fail at startup
  and list the available services; `peer:nas/dev/unknown --bind HOST:PORT`
  reports it at the first local connection.
- An occupied local port releases the other listeners and names the service.
- `--bind HOST:PORT=PORT` fails with the rewritten form.
- `serve` refuses duplicate names, duplicate client ports, invalid names,
  numeric names outside `1..=65535` and more than 64 services.
- `connect` that needs `list` fails clearly when the server is unreachable.
- A third target segment is refused by every command other than
  `tunnel tcp connect`.
- An unreachable destination refuses that stream with `unreachable`; other
  services stay available.
- `list` and `connect` are answered only after the slot handshake.
- The server serves two `connect` streams on one connection (unit or
  integration test).
- `tunnel http serve` / `connect` work without regression.
- Updated: `docs/guides/tunnels.md`, `docs/reference/compatibility.md`,
  `docs/reference/cli.md`, `docs/reference/slots.md`,
  `notes/changelog-0.1.0.md`.
