# Relay checks, ping and benchmarks

## Check relay access

```sh
iroh-toolbox relay check
# JSON output:
iroh-toolbox relay check --json
```

This checks whether the endpoint can register with a public iroh relay by
default, or your custom relay with its locally configured token. It reports the
actual relay URL, registration time and WebSocket/TCP transport. JSON also
includes `mode=public` or `custom`. No
second peer is needed, and this does not measure RTT between machines. A failure
reports the relay's internal registration error when available, including
explicit authentication denial. A successful HTTP 101 alone does not prove
registration. An online timeout without relay status points to DNS, TLS, or HTTP
probe access; it is not evidence that a token is incorrect.

## Ping: observe the connection path

On **machine A**, start a server:

```sh
iroh-toolbox ping serve
```

Copy the printed `endpoint…` ticket. On **machine B**, run the ping. A ticket-mode
ping server has no access control: it exposes round-trip measurements only, so
anyone holding the ticket can use it (never a tunnel or pipe):

```sh
iroh-toolbox ping connect 'endpoint…' --count 10
```

Example output when the connection switches from relay to direct:

```text
seq=1 rtt=35.210ms QUIC route=relay carrier=WebSocket/TCP remote=...
seq=2 rtt=8.430ms QUIC route=direct carrier=UDP remote=...
```

The iroh connection remains **QUIC**, encrypted end to end. Its packets travel
over UDP when direct, or over WebSocket/TCP through the relay. The selected path
is checked after each reply.

For JSON output with known paths and QUIC's estimated RTT:

```sh
iroh-toolbox ping connect 'endpoint…' --json
```

An endpoint ID can replace the ticket when the peer is on the public default
map or the same selected custom relay. `--direct-only` requires a full ticket
with reachable direct addresses.
The remote peer must run `ping serve`: this measures a round trip between peers,
not an ICMP ping to the relay server.

### Between paired machines

Without a ticket, a paired machine can reach diagnostics on a slot. The server
must run `ping serve --slot` (or `--peers-only`); `ping connect peer:NAME` fails
otherwise:

```sh
# Machine A: open slot, or paired machines only
iroh-toolbox ping serve --slot
iroh-toolbox bench serve --peers-only
# Machine B, paired with A (default slots: ping, bench)
iroh-toolbox ping connect peer:nas --count 10
iroh-toolbox bench connect peer:nas
```

A plain `--slot` server is **open**: it authenticates itself to the client but
serves anyone who knows the slot address, a removed peer included. `peers rm`
does not stop it; only `id rotate --address` does. `--peers-only` implies a
slot, asks for the client's machine proof and serves any paired machine;
`--allow NAME` (repeatable or comma-separated) implies `--peers-only` and
narrows it to those machines. Add `--addr HOST:PORT` (and `--port` on the
server) to measure a direct path without a relay.

### When a paired connection fails

A `peer:NAME` client trusts a refusal only after the server proved the machine
key stored when pairing. With `ping connect peer:nas`, the messages read:

| Message | What it establishes | Next step |
| --- | --- | --- |
| `nas does not know this machine; …` | nas proved its identity, then found no record of this machine (close code 30) | Pair again with `iroh-toolbox pair` on both machines |
| `this nas server (slot ping) does not allow this machine (--allow)` | This machine is paired, but outside that server's `--allow` (code 32) | Restart the server with this machine in `--allow` |
| `nas rejected this machine's authentication proof (invalid client proof); …` | nas could not validate this machine's proof (code 33); this says nothing about pairing | Check the local identity file and that both machines run compatible versions |
| `cannot authenticate nas: its response does not match the expected identity or handshake context; …` | nas's response does not verify; this machine sent no proof or data | Check the target; if nas confirms it ran `id rotate`, run `peers rm nas` and pair again |
| `cannot authenticate nas: the server did not provide a valid proof; its refusal reason is untrusted` | A refusal arrived before any valid proof | Check the target and the network |
| `cannot establish an authenticated connection to slot ping on nas: …` | No authenticated answer, for a reason the client cannot tell: server stopped, network, address, slot name or version | Check that nas runs `ping serve --slot` or `--peers-only`, then `--addr` |

When nas proved its identity but the handshake then stalls, the client reports
that the authenticated handshake with nas did not complete, not a refusal. The
same messages apply to tunnels, pipes, inboxes, offers and chats by name, except
that an absent inbox, offer or chat names the command to run on the other
machine (for example `nas is not receiving; run 'iroh-toolbox receive --inbox' on nas`).
See [troubleshooting](../reference/troubleshooting.md).

## Measure throughput

On **machine A**, start a benchmark server:

```sh
iroh-toolbox bench serve
```

On **machine B**, use the printed ticket:

```sh
iroh-toolbox bench connect 'endpoint…'
# 128 MiB per direction, with JSON output:
iroh-toolbox bench connect 'endpoint…' --bytes 128MiB --direction both --json
```

Throughput is reported in **Mbit/s**. Directions are relative to the machine
running `bench connect`: `upload` sends to the server; `download` receives from
it. The default, `both`, measures the two directions sequentially. Each direction
transfers 64 MiB by default; use `--bytes` to change that amount.
Size options accept integer bytes and the same [size suffixes as file
transfers](files.md): `64MiB` equals `67108864`, while `64MB` equals `64000000`.
JSON results continue reporting numeric byte counts.

By default stderr shows the steps that matter (waiting for the peer, each
transfer direction). During each transfer a line shows MiB transferred,
percentage, average throughput and the current direct/relay path; on a terminal
it is rewritten in place and stays on screen when the transfer ends. Redirected
stderr records each step once. The server reports its steps too.

Global `-v` adds internal steps such as endpoint registration and negotiation.
`-vv` prints every update as its own line, once per second, including while an
operation is waiting on the network.

Use `--no-progress` on either benchmark command to disable these updates.
With `--json`, stdout still contains only the final JSON results; progress stays
on stderr.

Results report the selected path at the end of each measurement. JSON output
also includes path changes observed during the transfer. This measures application
throughput through the toolbox, including its native Rust processing.

The server serves one benchmark and exits. It accepts up to 1 GiB per direction
(`bench serve --max-bytes N`). The total benchmark timeout defaults to 60 seconds;
set `--transfer-timeout SECONDS` on either command to change its timeout.

## Inspect a ticket offline

```sh
iroh-toolbox ticket inspect 'endpoint…'
iroh-toolbox ticket inspect 'endpoint…' --json
```

Displays the peer ID, relay, and advertised direct addresses. This command opens
no sockets and does not load relay configuration. A ticket describes possible
addresses; it does not prove which path an actual connection will use.
