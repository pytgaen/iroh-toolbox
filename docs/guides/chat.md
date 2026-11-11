# Chat and file offers

Open a private conversation, then exchange files without copying blob tickets.
Chat is included in the standard native build on both machines:

```sh
cargo build --locked
```

`chat serve` opens a chat and waits; `chat connect` joins it, with a one-use
code of words or, between [paired machines](../reference/peers.md), by name.
`chat` alone only shows help. Windows, separate networks and NAT/CGNAT remain
to be validated.

## Start a chat with a code

On machine A:

```sh
iroh-toolbox chat serve
```

A shows its code and the command for B on stderr:

```text
Code: 4-amber-… (single-use: first admitted participant takes the session)
On your partner's machine (keep the code private):
  iroh-toolbox chat connect code:
  then type the code at the prompt (it stays out of the process list)
```

Keep A running and type its complete code at the prompt on B (echo off, outside
the process list):

```sh
iroh-toolbox chat connect code:
# Code:
```

`chat connect code:WORDS` also works, but other local users can read the words
while the command runs. When A's stdout is redirected, or with `--no-hints`, it
also carries the bare code for scripts. A ticket is not accepted. The former
forms `chat code:WORDS`, `chat peer:NAME` and `chat --peers-only` are refused
with the subcommand to use.

Once B is admitted, both terminals show the admission path:

```text
· Chat ready; admission: code. Type a message or /send PATH, /cancel, …
```

A then accepts only B's iroh identity, for later connections and file
transfers alike.

## Chat with a paired machine

Paired machines join without a code. When this machine has at least one paired
machine, `chat serve` also opens the `chat` [slot](../reference/slots.md) and
prints the command for them, before its code:

```text
Serving to: laptop
On the other machine, run:
  iroh-toolbox chat connect peer:nas
```

To open the chat to laptop only, with no code and no relay needed:

```sh
# Machine A (nas)
iroh-toolbox chat serve --allow laptop
# Machine B (laptop)
iroh-toolbox chat connect peer:nas
```

Each terminal then reports `Chat ready; admission: paired machine NAME.`, and
names the other machine as its own `iroh-toolbox peers` list does: A shows
`laptop`, B shows `nas`.

`--peers-only` admits any paired machine; `--allow NAME` (repeatable or
comma-separated) admits only those and implies `--peers-only`. A machine outside
it is told `this nas server (slot chat) does not allow this machine (--allow)`.
If no chat is open, `chat connect peer:nas` keeps trying within `--timeout`,
then reports `nas has no chat open; run 'iroh-toolbox chat serve' on nas`.

One `chat serve` runs per slot on a machine. The default slot is `chat`; open a
second chat with `--slot NAME` and join it with `peer:nas/NAME`. Without a
relay, run `chat serve --peers-only --direct-only --port 7777` on A and
`chat connect peer:nas --direct-only --addr HOST:7777` on B, with an address A
prints. On a machine with no paired machine at all, `chat serve` is open by code
only and says so: `No paired machine: this chat is open by code only.`

## Who can join

A chat has two participants. The first one admitted, by code or as a paired
machine, takes the session, and the other path then admits nobody:

- the code cannot be used again;
- a paired machine that connects later is told
  `nas is in a chat session with another participant; try again later`;
- after a connection loss, only the admitted participant may reconnect.

The banner names the path that admitted the participant:
`admission: code` or `admission: paired machine NAME`. A machine is named only
when its key was verified on the slot. A participant admitted by code is never
labelled as a paired machine, whatever name it claims; the code does not tell
whether it is one.

A code is a separate credential, valid until it is used or expires:

- **`--allow` creates no code.** `chat serve --allow laptop` admits laptop
  only, like `--peers-only --allow laptop`: no code path bypasses the filter.
  A chat with a code admits any paired machine besides the code holder.
- **`peers rm NAME`** refuses that machine's next peer connection. It neither
  ends an open session nor revokes an unused code.
- **Single-use** limits the session to one participant, not necessarily the one
  you intended. If the code reached someone else, stop A with Ctrl-C and run
  `chat serve` again: the new code replaces the old one. Stopping A also ends a
  session that has already started.

The confirmation number is asked during code pairing only; `--no-verify` skips
it. In `chat serve`, it does not confirm paired machines; `chat serve --peers-only` and
`chat connect peer:NAME` refuse it, since a paired machine proves its key. See
the [security model](../reference/security.md).

## Exchange messages

Type messages and press Enter. Received messages appear on stdout after the
sender's name: the local name of a paired machine (`laptop>`), or `peer>` in a
chat opened by code, whose participant has no verified name. Status, file
offers, progress and errors appear on stderr, each after a mark:

| Mark | Meaning |
| --- | --- |
| `·` | Information: offers, progress, reconnection attempts, the end of the chat |
| `!` | A refusal, failure or mistyped command, with the usage or the closest command |
| `✓` | A completed outcome: a verified reception, the peer's confirmation, a reconnection |

A session has two participants, supports messages and file offers in either
direction, and keeps no history on disk.

In an interactive terminal (stdin, stdout and stderr all on a terminal), a line
editor keeps what you are typing intact while messages arrive above it:

```text
14:03 laptop> salut, je t'envoie les photos
14:03 · laptop offers photos — 1.2 GiB, 312 files. Use /receive DESTINATION or /reject.
14:04 you> merci
14:04 you> /recieve ./reception
14:04 ! Unknown command /recieve; did you mean /receive?
14:04 you> /receive ./reception
14:04 · Connecting to sender
14:09 ✓ Received photos → /home/me/reception/photos — 1.2 GiB, 312 files, 4 min 12 s, 4.9 MiB/s; BLAKE3 verified
14:09 you> merci, bien reçu█
```

The editor colors the marks and names: information in gray, refusals in yellow,
outcomes in green. Set the standard [`NO_COLOR`](https://no-color.org)
environment variable to any non-empty value to turn every color off, the prompt
included; redirected output never has colors.

↑ and ↓ recall earlier lines of the session, and Tab completes commands,
`/receive` options and paths:

```text
you> /se⇥                 →  you> /send
you> /send hol⇥           →  you> /send "holiday photos/
you> /receive --f⇥ in⇥    →  you> /receive --force inbox/
```

When several entries match, a second Tab shows them in columns. `/receive`
completes directories only. A running transfer prints its progress every 10
seconds, and `/status` shows it at once. Ctrl+D leaves like `/quit`; Ctrl+C
leaves and exits with status 130. The input history stays in memory. When any
of the three streams is redirected, lines are read and printed as plain text,
for scripts.

Messages show the time they were received or typed. `--timestamps` chooses
when; the `IROH_TOOLBOX_TIMESTAMPS` environment variable sets a lasting
default, which the option overrides:

| `WHEN` | Time shown |
| --- | --- |
| `auto` (default) | In an interactive terminal only |
| `always` | Also when output is redirected: `14:03 peer> …` in a log |
| `never` | Never |

```sh
iroh-toolbox chat connect code: --timestamps always >> chat.log
export IROH_TOOLBOX_TIMESTAMPS=never
```

Chat messages and file offers travel through an authenticated iroh connection;
file contents use native iroh-blobs on the same endpoints. Configure the same
custom iroh relay on both machines if you prefer your own infrastructure;
otherwise public iroh relays are selected automatically. Global `--relay-only`
forces relayed chat and files. A code needs a relay, so `--direct-only` applies
only to `chat serve --peers-only` and `chat connect peer:NAME`.

## Code options

The code authenticates both identities and delivers the creator's address through
iroh's relays, without a mailbox or server of yours. Once the code is used (`-v`
shows `Pairing complete.`), chat and file transfers use iroh with those same
endpoints. Use
`--code-words 8` on `chat serve` for a longer code; both machines need the same
relay selection, and relay URL/token configuration remains local to each machine.

Each terminal shows its own 6-digit confirmation number and asks for the other
terminal's: read yours to the other person by voice or in person, and type the
one they read to you. A single confirmation does not open the chat. Three wrong
numbers, an empty answer, the pairing timeout or Ctrl+C abort it. This needs
interactive terminals on both machines. See the
[pairing protocol](../reference/pairing-protocol.md). The number detects someone
between the machines, not someone who sends you their own code under another
name: know who gives you the code, and compare numbers with that person
([security model](../reference/security.md)).
Pass `--no-verify` on **both** machines to skip it; whoever uses the code first
then joins, and `chat serve` and `chat connect code:` without a terminal refuse
to start unless both get `--no-verify`.
`--pairing-timeout` defaults to 300 seconds, or 120 with `--no-verify`. When code pairing fails while
paired machines may also join, `chat serve` reports
`code pairing failed: …; paired machines may still join` and keeps waiting for
them. `--timeout` bounds iroh connection, handshake and blocked chat writes.
Idle sessions exchange heartbeat messages.

## Connection loss

The peer acknowledges every message. When the connection is lost, the machine
that ran `chat connect` dials again for up to 60 seconds, and `chat serve`
accepts only the participant it admitted: the identity that used the code, or
the same paired machine from the same endpoint. No new code is needed:

```text
! Connection lost; reconnecting for up to 60 s…
· Not connected; the message will be sent after reconnection.
✓ Reconnected; 1 pending message(s) resent.
```

Meanwhile `chat serve` shows
`! Connection lost; waiting up to 60 s for peer to reconnect…`, with the
paired machine's name instead of `peer` when it has one. A paired machine removed with `peers rm` meanwhile is refused when
it reconnects. Messages typed meanwhile, and messages the peer had not
acknowledged, are sent again after reconnection and displayed once. A file
offer, preparation or transfer in progress is cancelled by the loss; offer it
again with `/send`. If the peer cannot be reached within 60 seconds, the chat
ends with an error and lists the messages that were not delivered. The same
list follows `Peer left the chat.` (or `laptop left the chat.`) when the peer
leaves before reading them. A
silent network failure is noticed only when iroh's idle timeout expires, which
can take tens of seconds.

`/status` shows how the peer was admitted, its route (`direct` or `relay`),
both file offers, the number of unacknowledged messages and, in an interactive
terminal, the running transfer:

```text
you> /status
· Peer: admitted by paired machine nas (direct)
· Outgoing offer: none
· Incoming offer: "photos", 1.2 GiB, receiving into "./reception"
· Unacknowledged messages: 0
· Transfer: receiving and verifying BLAKE3 42% (515.0 MiB/1.2 GiB)
```

## Offer and receive a file

In A's chat, enter:

```text
/send ./photos
```

A copies and hashes the file or directory into temporary storage, then B sees
its name, payload size and number of files. B explicitly chooses a destination:

```text
/receive ./reception
```

The ticket is sent automatically after this acceptance, to the session's
participant only. The result is `reception/photos/` for a directory, or
`reception/filename` for a file. The destination's parent must exist. Files are
checked with BLAKE3 and staged before publication. Existing names are replaced
only after confirmation, as described below.
Both participants can continue chatting during preparation and transfer.

Paths are relative to the local process directory and may contain spaces:
`/send "./holiday photos"` and `/receive "./received photos"`. Quotes around the
whole path are optional, and the closing quote may be omitted; the remainder of
the line is the path. Single quotes keep the path literal. Inside double quotes,
`\"` is a quote and, on Linux and macOS, `\\` a backslash. Unquoted on Linux
and macOS, `\` escapes the next character, so a path dropped by a macOS
terminal (`My\ photos`) works. Dragging a file onto the terminal is the
quickest way to enter its path. Paths are not evaluated by a shell: variables
and `~` are not expanded.

| Command in the chat | Action |
| --- | --- |
| `/send PATH` | Prepare and propose a file or directory to the peer. |
| `/cancel` | Withdraw your own offer, during preparation, before acceptance or during transfer. |
| `/receive [--force\|--no-replace] [--hidden] DESTINATION` | Accept the pending incoming offer with an optional replacement policy; `--hidden` accepts top-level names starting with a dot. |
| `/confirm` | Approve the replacements and merges listed before a reception. |
| `/reject` | Reject the incoming offer, stop its reception, or decline a pending replacement confirmation. |
| `/status` | Show the connection, file offers and unacknowledged messages. |
| `/help` | Show the commands and path syntax. |
| `/quit` | End the conversation and cancel pending work. |
| `//text` | Send a message that begins with `/`. |

There is one pending file offer per direction. `/receive` always requires a
destination and applies to that pending offer. Received display text cannot
execute chat commands. Either participant can propose the next file once their
previous offer completes, is rejected, cancelled or expires. Both `/cancel` and
`/reject` during a transfer keep the chat open; an interrupted reception does
not claim successful publication. A file message from the peer that does not
fit the current offer is reported and ignored; only an undecodable frame ends
the session.

Messages are limited to 4,096 bytes of UTF-8. A longer or invalid line is
discarded with an error, and the chat continues.

When names of the offer already exist in the destination, `/receive` lists
them before downloading and waits for `/confirm` or `/reject`. The chat
continues to receive messages while waiting. To choose in advance, place
`--force` or `--no-replace` before the destination:

```text
/receive --force ./reception
/receive --no-replace "./received photos"
```

`--force` replaces colliding regular files without a question; unrelated files
remain. `--no-replace` refuses an existing destination. The options cannot be
combined. Symlinks, special files and file/directory type
conflicts are refused. The [file guide](files.md#replace-existing-files) explains
staging, rollback and recovery backups. A replacement confirmation is local to
the receiver; remote text cannot approve it. The transfer deadline still applies.

`--max-bytes` defaults to `10GiB` per received offer. An advertised larger offer
is rejected automatically; the receiver also enforces the advertised size and
local limit against the actual blob metadata. `--transfer-timeout` defaults to
3,600 seconds per preparation, offer wait and accepted transfer. On reception,
the deadline includes waiting for the ticket. `--no-progress` hides preparation
and transfer progress; offers, failures and final results stay visible.

The receiver reports `✓ Received NAME → PATH — SIZE, FILES, DURATION; BLAKE3
verified` after publication and sends a completion message. The sender prints
`✓ Peer confirms` (or the paired machine's name) only upon receiving that
message. `/quit` prints `Chat ended.` before leaving.
A disconnected peer cannot supply this confirmation. EOF on stdin, Ctrl+D or
`/quit` ends the session; Ctrl+C exits with status 130. Interrupted downloads do not
claim successful publication. Chat transfers cannot be resumed, and abrupt
process kills can leave temporary stores. Empty directories are omitted; an entirely
empty directory cannot be sent. See the [file limits](files.md) for details.
