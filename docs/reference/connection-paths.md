# Connection paths and tickets

## Direct and relay paths

The iroh connection uses QUIC and is encrypted between the peers in either case.
On a direct path, packets travel over UDP. On a relay path, QUIC packets travel
through the relay over WebSocket/TCP. A configured relay does not force a relay
path: iroh can establish a direct connection and switch the selected path.

`ping` reports the selected path after each response. Benchmark JSON also records
path changes observed during a measurement. A ticket lists possible addresses;
only an actual connection tells you which path is in use.

Public iroh relays are used without configuration. A custom relay is optional;
when selecting one, configure it on both peers. To exercise relays exclusively,
use `--relay-only` on **both peers**:

```sh title="Peer A"
iroh-toolbox --relay-only ping serve
```

```sh title="Peer B"
iroh-toolbox --relay-only ping connect 'endpoint…' --json
```

Between [paired machines](peers.md), `ping serve --slot` and
`ping connect peer:NAME` need no ticket.

## Tickets

| Ticket | Contains | Used by |
| --- | --- | --- |
| `endpoint…` | Peer identity and connection addresses | `ping` and `bench` in ticket mode |
| `blob…` | Peer addresses, collection hash and blob format | Travels inside the encrypted channel of a code; never typed |

`send`, `chat`, `tunnel` and `pipe` no longer print tickets: files and chat use
a one-use code or, between paired machines, `peer:NAME[/SLOT]`; tunnels and
pipes use `peer:NAME[/SLOT]` only. A ticket is an address, not a password: it
contains no relay token and no authorization, and whoever knows the endpoint ID
can connect to a ticket-mode `ping` or `bench` server, which expose
measurements only.

Inspect a ticket without opening sockets or loading relay configuration:

```sh
iroh-toolbox ticket inspect 'endpoint…' --json
iroh-toolbox ticket inspect 'blob…' --json
```

The toolbox uses the public production map by default or only your selected
custom relay, with no public address lookup. Tickets advertising relays outside
that selected set are rejected; explicitly configure a custom URL to use it.
An endpoint ID can replace a full endpoint ticket for `ping` and `bench` when
its peer is registered on the selected custom relay or one of the built-in public
relays. The public ID form tries those public relay URLs; it does not perform
public address lookup.

`--direct-only` disables relay transport, even when a ticket contains relay
hints. Use a full ticket with reachable direct addresses for `ping` and `bench`,
or `--addr HOST:PORT` for a `peer:` target (the server prints it with
`--direct-only --port PORT`). This flag can also
override a relay/token configured through environment variables or TOML. If
public relay registration is unavailable, ordinary startup can still continue
with direct addresses after its registration timeout; explicit custom-relay or
`--relay-only` failures remain errors. `relay check` tests actual registration.

## Service lifetime

Ticket-mode servers (`ping serve`, `bench serve`) start a fresh peer identity
each time; restart them and exchange the new ticket. Slots keep their address
across restarts, and paired machines find them by name. A relay does not start a
stopped service, and the toolbox does not provide background service management.

`send` (with a code, `peer:NAME`, or `--peers-only` without `--share`),
`receive --inbox --once`, `bench serve` and `pipe serve` each handle one peer
session and exit, and `chat serve` ends with its conversation. `ping serve`,
TCP tunnel commands, `receive --inbox` and `send --peers-only --share` stay
running until stopped.
