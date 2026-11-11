# Troubleshooting

Start with the command that checks the relevant layer. `relay check` tests relay
registration; `ping connect` tests a running peer; the destination application
tests the service behind a TCP tunnel.

## Relay registration fails

```sh
iroh-toolbox relay check --json
```

| Symptom | Check |
| --- | --- |
| No custom relay configured | Public iroh relays are used automatically; no token is needed. `relay check` verifies actual registration. |
| Authentication denied | Select a token accepted by the relay; do not include the `Bearer` prefix in its value. |
| Online timeout | Check DNS, TLS, and anonymous `/ping` and `/generate_204` probes. A timeout alone does not prove a bad token. |
| HTTP 101 succeeds, registration fails | Allow WebSocket forwarding on `/relay` and preserve its Authorization header. HTTP upgrade alone does not prove relay registration. |
| Ticket relay differs from selection | A custom relay outside the public/default or selected private set requires an explicit matching `--relay URL`. |
| Relay check with `--direct-only` | Remove this flag to test relays; direct-only mode disables them. |

See [private relay setup](../guides/private-relay.md) for probe and proxy details.

## Peer connection fails

Confirm the remote command is still running. For `ping` and `bench` in ticket
mode, the ticket belongs to the current launch of the server: restarting it
creates a new ticket. Use the matching command: a `ping serve` peer answers
`ping connect`; it does not serve files or TCP streams. A code works for one
attempt: after a failed `send`, `chat serve` or `pair`, create a new code.

Default operation uses public relays and can switch to direct UDP. With
`--direct-only`, `peer:` clients need `--addr` with an address the server
printed. If you prefer a private
relay, configure it on both peers. For a controlled relay test, use `--relay-only`
on both peers; it works with the public defaults too. Increase `--timeout` before the command if
registration or connection setup needs more than the default 15 seconds.

## A peer connection is refused { #a-tunnel-or-pipe-is-refused }

This applies to every `peer:NAME[/SLOT]` client: tunnels, pipes, `send PATH
peer:NAME`, `receive peer:NAME`, `chat connect peer:NAME` and slot ping/bench.
`NAME` and `SLOT` in the messages come from your own target, never from the
other machine, and no message repeats a key, a signature or text chosen by the
other end.

### What the message proves

| Client message | What it proves | Action |
| --- | --- | --- |
| `cannot authenticate NAME: its response does not match the expected identity or handshake context; check the target and whether NAME rotated its identity` | The server's proof does not verify against the machine key stored when pairing, for this slot, protocol and connection. Decided locally: this machine sent neither its own proof nor any data. It does not prove that the key changed | Check the target (`peer:NAME/SLOT`, `--addr`) and the command running on NAME. Another holder of NAME's address may have answered: ask whether NAME's server [warns that another machine took its address](#a-server-warns-that-another-machine-took-its-address). If NAME confirms it ran `id rotate`, run `peers rm NAME` and pair again; the stored identity is never replaced automatically |
| ``NAME does not know this machine; run `iroh-toolbox pair` on both machines`` | Code 30 after a verified proof: the server checked a valid signature from this machine, then found no entry for its key | This machine never paired with NAME, NAME ran `peers rm`, or this machine ran `id rotate`. Pair again; after `id rotate`, NAME must first remove the old entry with `peers rm` |
| `this NAME server (slot SLOT) does not allow this machine (--allow)` | Code 32 after a verified proof: paired, but that server's `--allow` does not list this machine. The server prints `refused NAME on slot SLOT: not listed by --allow` | Ask for the server to be restarted with this machine's name in `--allow`. Another slot may have another list |
| `NAME rejected this machine's authentication proof (invalid client proof); check the local identity and protocol implementation` | Code 33 after a verified proof: the server could not validate the proof it received, and decided so before reading its peer book | Check this machine's identity file and that both machines run compatible versions. A rotated identity gives 30, not 33: pairing again is not the remedy |
| `cannot establish an authenticated connection to slot SLOT on NAME: check that the server is running, the network and address (--addr), the slot name and compatible versions` | No usable server proof: no connection, incompatible version, deadline, or a missing or malformed proof. Nothing is known about a refusal | Check that the server runs, the relay or `--addr`, the slot name and both versions |
| `cannot authenticate NAME: the server did not provide a valid proof; its refusal reason is untrusted` | A refusal (30, 32, 33 or busy) arrived before the server proved itself | Treat it as unauthenticated. Check that the target is the machine you paired with |
| `the authenticated handshake with NAME (slot SLOT) did not complete…` | NAME proved itself, then the handshake ended without `ready`: a deadline, a malformed message, or a server that cannot read its peer book | Check NAME's output (with `-v`, a peer-book error such as `unsafe or unreadable peer book …`) and the network, then try again |

File and chat clients replace the availability message with the command to run
on the other machine: `NAME is not receiving; run 'iroh-toolbox receive
--inbox' on NAME`, `NAME is not sending; run 'iroh-toolbox send PATH
--peers-only' on NAME` and `NAME has no chat open; run 'iroh-toolbox chat
serve' on NAME`, with `--slot SLOT` for a named slot. Peer clients keep trying
within `--timeout` before they report that a server is unavailable.
On the server, refusals by `--allow` are printed; unpaired machines and invalid
proofs are visible only with `-v`.

### Other messages

| Message | Cause and action |
| --- | --- |
| ``no paired machine; pair one with `iroh-toolbox pair` first`` (server start) | A peer service needs at least one paired machine. Pair first; without one, `chat serve` is code-only |
| `name the machines to serve: --allow NAME (repeatable), or --allow all` (server start) | `tunnel tcp serve`, `tunnel http serve` and `pipe serve` do not start without `--allow`: list the machines, or pass `--allow all` for every paired machine |
| `--allow all cannot be combined with machine names` (server start) | Use `--allow all` alone, or list names only |
| `` --allow NAME: no paired machine named NAME; see `iroh-toolbox peers` `` (server start) | The name is not in the server's `peers` list: use its local name there |
| `` no paired machine named NAME; see `iroh-toolbox peers` `` (client) | `peer:NAME` uses your local name for the server. List it with `peers` |
| `peer book PATH holds per-slot access from a development build, …; delete it and pair again with each machine …` | A development build stored per-slot access, which this version no longer uses. Delete the file and pair again with each machine; servers then accept every paired machine unless started with `--allow`; tunnels and pipes require it |
| `tunnel and pipe connect to paired machines only; pair with …` | A ticket was given. Pair, then use `peer:NAME` |
| `id: targets no longer exist; pair the machines …` | Replace `id:…` addresses with `peer:NAME[/SLOT]` after pairing |
| `the server cannot be found without a relay; pass --addr HOST:PORT printed by the server` | `--direct-only` needs the address and UDP port the server printed (start it with `--direct-only --port PORT`) |
| `NAME refused the connection: unknown slot or protocol; check that NAME serves slot SLOT with …` | The slot name is wrong, or the command is (for example `receive peer:NAME` against a chat). The server does not say which. Check the name and the command on the server |
| `slot SLOT of NAME has no service "SERVICE"; available: …` | The service in `peer:NAME/SLOT/SERVICE` or `--bind SERVICE=HOST:PORT` is not one of the slot's. Use a listed name. A service of the default slot needs the slot: `peer:NAME/tunnel/SERVICE` |
| `NAME/SLOT serves several services (…); name one with …` | `--bind HOST:PORT` opens one listener, and the slot has several services. Name one in the target, use `--bind SERVICE=HOST:PORT`, or drop `--bind` to open them all |
| `--bind HOST:PORT=PORT uses the old order; write --bind PORT=HOST:PORT` | `--bind` takes the service first, as `--to` does |
| `cannot bind ADDRESS for service SERVICE; pass --bind SERVICE=127.0.0.1:PORT with a free port` | The announced port is taken on this machine. Move that service with `--bind` |
| `slot "SLOT" is already served by another iroh-toolbox process on this machine` | Choose another `--slot` or stop the other process |
| `… is already serving another client; try again later` | `pipe serve` and `bench serve` serve one client at a time |
| `… is in a chat session with another participant; try again later` | That chat already admitted its participant, by code or as a paired machine |
| `… is at its connection limit; try again later` | The server reached its `--max-connections` |
| `NAME is handling too many connections at once; try again` | The server dropped the handshake to make room; the client already retried. Try again later |
| ``--slot and --port apply to paired machines, and this machine has none; pair one with `iroh-toolbox pair` `` | `chat serve` with an empty book is code-only. Pair first, or drop these options |
| ``a chat with a code needs a relay; `chat serve --peers-only` opens one for paired machines without a relay: …`` | `chat serve` prints a code, which needs a relay. Remove `--direct-only`, or use `--peers-only` |

If the server is unreachable instead, keep it running and check the relay on
both machines; `ping connect peer:NAME` tests the path without touching your
services. It needs `ping serve --slot` (or `--peers-only`) running on the
server.

## A server warns that another machine took its address

```text
warning: another machine has taken slot ssh's address at the relay; paired machines cannot reach this slot through it
  It holds this machine's address: a paired, removed or lost machine, or a copy of this machine's identity file.
  If you removed a machine, run `iroh-toolbox id rotate --address`, then restart this server; `--direct-only` servers are not affected.
```

Every machine that holds this machine's address can compute a slot's endpoint
key. Here another one registered it at the relay, which delivers to the newest
registration only. With public relays, the variant `another machine has
replaced slot ssh's address record` means it published its own DNS record for
the slot. Paired machines then fail with `cannot authenticate NAME: its
response does not match…`: that machine cannot prove this machine's identity,
so no data reaches it. The server keeps running and does not fight for the
address.

- If you removed a lost or stolen machine, run `iroh-toolbox id rotate
  --address`, restart the servers and pair again with the remaining machines
  (see [after an incident](peers.md#after-an-incident)).
- If a copy of the identity file runs elsewhere (a synced or restored
  configuration directory), stop it; if it may have leaked, run `id rotate`.
- Meanwhile, a server started with `--direct-only --port PORT` is reached with
  `--direct-only --addr HOST:PORT`, without relay or DNS.

`note: slot ssh is reachable through the relay again` (or `… address record
points to this server again`) means the other registration left.

## An unexpected participant joined

The chat banner says how the other side was admitted: `Chat ready; admission:
code`, or `Chat ready; admission: paired machine NAME`; `/status` repeats it as
`Peer: admitted by …`.

- **`paired machine NAME`** comes from a machine key verified through the slot
  handshake: it is the machine `NAME` of your peer book, or someone holding its
  identity file.
- **`code`** means that someone completed the code exchange first. The code may
  have been disclosed before it was used: that is one possible cause, which the
  join itself does not prove. A code participant is never labelled with a peer
  name, whatever it claims, and neither `peers rm` nor `--allow` stops a valid
  code.

To end the session and restrict the next one:

1. Stop the server with Ctrl-C: the session ends and the old code becomes
   useless.
2. If a paired machine should no longer join, run `peers rm NAME` (it does not
   end an open session by itself).
3. Start again: `chat serve` prints a new code; `chat serve --allow NAME`
   prints none and admits only `NAME`.

See the [chat guide](../guides/chat.md) and
[codes, pairings and sessions](security.md#codes-pairings-and-sessions).

## Pairing fails

| Symptom | Action |
| --- | --- |
| `pair needs an interactive terminal on each machine` | Run `pair` from a terminal on both machines: both people compare a number |
| `pair meets the other machine through a relay; remove --direct-only` | Pairing needs a relay once; peer services work without one afterwards |
| `the other machine uses a different command or --no-verify setting` | The code was made by another command (`send`, `chat serve`) or `--no-verify` is on one machine only. Create a new code, with `--no-verify` on both machines or on neither |
| `the confirmation number needs an interactive terminal` | `send`, `receive code:`, `chat serve` and `chat connect code:` compare a number by default. Run from a terminal, or pass `--no-verify` on both machines to skip it (whoever uses the code first is then served) |
| `the other machine did not confirm the confirmation numbers` or `confirmation numbers not confirmed; create a new code` | A number was typed wrong three times, or someone cancelled with an empty answer. If the number read aloud never matched, treat the code as exposed |
| `pairing failed: wrong code, or someone else tried to use it; create a new code` | A code allows one attempt: create a new one |
| `name taken; retry with --name ALIAS` / `the other machine has no free name for this one` | A local name belongs to another machine. Pair with `--name ALIAS`, or free the name with `peers rename` or `peers rm`. If that machine rotated its identity, the old entry must be removed first. Nothing was stored |
| `pairing ran out of time before it could be completed; nothing was stored; pair again` | The deadline (`--pairing-timeout`, 300 s by default) was too short for the answers. Pair again |
| `warning: NAME did not confirm that it stored this pairing` | The other machine's confirmation was lost. Check `peers` on both and repeat the pairing, or `peers rm NAME` |
| `waiting for another pairing or peers command to finish` | The peer book is locked by another `pair` (which holds it while the other machine answers) or `peers` command. It continues when that one ends; close forgotten `pair` terminals |
| `unsafe or unreadable peer book …` | `peers.toml` is owned by someone else or writable by others: `chmod 600` it as the message says |

## Code prompt and local files

| Message | Cause and action |
| --- | --- |
| `` receive reads the code from a terminal; run it in one, or pass `code:WORDS` `` (same for `chat connect` and `pair`) | `receive`, `receive code:`, `chat connect code:` and `pair code:` ask `Code:` on the terminal. Without one (script, pipe), pass `code:WORDS` |
| `no code entered` | The prompt got an empty line. Run the command again and type the words |
| `… it must belong to the current user and be private; run: chmod 600 PATH` | The `identity` file belongs to another user or others can read it. Fix the owner and mode as shown |
| `… was open to other users; its permissions are now 0700` | The configuration directory was too open and was tightened once. Nothing to do |
| `cannot restrict DIR; run: chmod 700 DIR` / `DIR belongs to another user; set IROH_TOOLBOX_HOME …` | The configuration directory is not yours. Fix it, or select a directory of your own with `IROH_TOOLBOX_HOME` |
| `warning: the filesystem of DIR does not keep file permissions; only file ownership is checked there` | WSL `/mnt/c`, exFAT or some Docker mounts have no POSIX modes. Only the owner is checked: keep the configuration directory on a Unix filesystem |

## File reception fails

| Symptom | Action |
| --- | --- |
| Destination already exists | Choose another output directory or rename the existing item. Run `receive` in an interactive terminal to be asked, or pass `--force` deliberately to replace regular files and preserve unrelated files. |
| Output parent missing | Create the parent directory before using `--output`. |
| Size exceeds limit | Confirm the expected content, then set `receive --max-bytes N` deliberately. |
| Hard-link error on FAT/exFAT | Use a destination filesystem with hard-link support, such as ext4 or NTFS. |
| Transfer timeout | Increase `--transfer-timeout`; check available temporary disk space and the peer connection. |
| Empty directory | Send at least one regular file. Blob collections omit empty directories. |
| Unsafe path, link or collision | Remove unsupported names/links from the source. Check portable naming rules and case-insensitive collisions. |

An interrupted reception resumes when the same `receive` command runs again
while `send` is still running. `cannot reach the sender` with a kept
`.iroh-partial-…` directory means the sender stopped: delete the directory and
start a new transfer. See [file-transfer limits](compatibility.md).

### Between paired machines

| Message (sender or receiver) | Cause and action |
| --- | --- |
| `NAME refused ITEM: its inbox already holds files with these names; nothing was replaced` | An inbox never replaces or merges. Ask for the existing files to be moved, or rename what you send. Identical files are recognized instead: `NAME already holds identical files; nothing was sent again` |
| `NAME refused ITEM: larger than its inbox accepts (--max-bytes)` | The inbox runs with a lower `--max-bytes`. Restart it with a deliberate limit |
| `NAME's inbox has too many senders waiting; try again later` | An inbox receives one transfer at a time and keeps at most 8 senders waiting |
| `NAME's inbox is closing: it already received its single transfer (--once); nothing was sent` | The `--once` inbox finished. Ask for a new `receive --inbox` |
| `` NAME is sending to another machine; ask NAME to share with `send PATH --peers-only --share` `` | Without `--share`, `send --peers-only` is reserved for its first receiver. Ask for `--share` |
| `NAME serves too many receivers at once; try again later` | A share serves at most 8 receivers at once |
| `the connection to NAME was lost before it confirmed the result; the outcome is unknown: …` | Check NAME, then send again: verified data resumes, and identical files are recognized without being rewritten |
| `the connection to NAME was lost; run the same command again while NAME still offers …` | Run the same `receive peer:NAME` into the same `--output`: it resumes the same offer from the same machine |

## A TCP tunnel connects but the service does not answer

Verify the destination service is listening on the address passed to `--to`,
from the machine running `tunnel tcp serve`. The local `--bind` address belongs
to the connecting machine. Use the port printed on stderr when binding port `0`.

The toolbox transports TCP; service login, HTTP routing, and TLS configuration
remain part of the destination application.

## A served folder answers 404 or a page does not work

`tunnel http serve` answers 404 for names starting with a dot (serve them with
`--hidden`), for links that lead outside the folder, and for anything that is
neither a file nor a folder. With `--site`, a folder without `index.html` also
answers 404: a site shows no listing. Without `--site`, HTML pages display but
do not run their scripts; serve a trusted site with `--site`.

`… is not a folder; to forward a running web server, use tunnel tcp serve`
means that an address was given instead of a folder: forward that web server
with `tunnel tcp serve --to HOST:PORT`.

## Share a useful report

Include `iroh-toolbox --version`, OS, commands on both peers, direct/relay mode,
expected behavior and sanitized errors. Mask tokens, tickets, private addresses
and sensitive file paths. See the [contribution guide](../maintainers/contributing.md).
