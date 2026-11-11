# Spec: peers, slots, files and chat (0.1.0)

Status: development contract consolidated on **2026-11-01**. Behavior not yet
available in the binary remains to be implemented.

**This file is the only specification to read for this development.** It
covers pairing, the peer book, slots, `--allow` filters, code-based and peer
transfers, inboxes with `--once`, persistent sharing with `--share`,
`chat serve` / `chat connect`, protocols, handling of the previous access
model, and acceptance criteria.

Reading guide:

- [Shared model](#goal) and [base commands](#commands).
- [Files and chat](#files-and-chat): CLI matrix, behavior and examples.
- [Peer book](#peer-book), [transition from previous grants](#transition-from-persistent-slot-grants)
  and [slot authentication](#peer-authentication-on-slots).
- [Authentication diagnostics](#authentication-diagnostics),
  [address takeover warning](#address-takeover-warning) and
  [chat code lifetime and revocation](#chat-code-lifetime-and-revocation).
- [Verification and documentation](#documentation-and-tests).

[peer-access-decision.md](peer-access-decision.md) preserves historical
context, including the previous per-slot grant decision. It is not a second
specification to apply: the rules below supersede it on those points.
Update the user guides and references after implementation.

## Goal

No 0.1.0 command gives access to data or services to a machine that has not
been authenticated:

| Command | Who is served |
| --- | --- |
| `send` / `receive`, `chat` | Code-based sessions; the [files/chat modes](#files-and-chat) add positional `peer:NAME[/SLOT]` targets, inboxes with optional `--once`, persistent `--share` and mixed or peers-only chats |
| `tunnel tcp`, `pipe` | Paired machines, on running server slots; optionally narrowed by each server's `--allow` |
| `ping`, `bench` | Anyone holding the ticket, or a paired machine: measurements only |
| `relay check`, `ticket inspect`, `id` | Local commands |

Machine addresses (`id:MACHINE+SALT`) become internal: `pair` exchanges them
and the peer book stores them. No command prints or accepts them.

Wire compatibility with sendme and dumbpipe is not a goal.

The model has three separate responsibilities: the peer book records **who**,
the slot names **which running service**, and `--allow` restricts **who that
server accepts**. Without `--allow`, a peer-facing server accepts every paired
machine. There is no `peers access` command or persistent slot-grant list in
the target model; see [transition](#transition-from-persistent-slot-grants).

## User experience

```console
# Files and chat: code-based examples (full command matrix below)
A$ iroh-toolbox send ./photos
Code: 7-river-…
B$ iroh-toolbox receive code:7-river-… --output ./received

A$ iroh-toolbox chat serve
Code: 4-amber-… (single-use: first admitted participant takes the session)
B$ iroh-toolbox chat connect code:4-amber-…

# This machine
nas$ iroh-toolbox id
Name:     nas (announced when pairing)
Machine:  yx3c…k1oa
Peers:    none
nas$ iroh-toolbox id rename nas           # optional; default: the hostname

# Pair two machines once; servers choose their own peer filters
nas$ iroh-toolbox pair
This machine: nas
Code: 3-lamp-…
Your confirmation number: 284 913
Their confirmation number (empty to cancel): 730 165
Paired with laptop

laptop$ iroh-toolbox pair code:3-lamp-…
Your confirmation number: 730 165
Their confirmation number (empty to cancel): 284 913
Paired with nas

# Tunnels and pipes: named services, filtered per server
nas$ iroh-toolbox tunnel tcp serve --to 127.0.0.1:22 --slot ssh --allow laptop
Serving slot ssh to: laptop
laptop$ iroh-toolbox tunnel tcp connect peer:nas/ssh --bind 127.0.0.1:2222

nas$ tar c data | iroh-toolbox pipe serve --slot backup --allow laptop
laptop$ iroh-toolbox pipe connect peer:nas/backup | tar x

# Manage trust
nas$ iroh-toolbox peers
NAME    MACHINE     PAIRED
laptop  q4ma…8xne    2026-11-01
nas$ iroh-toolbox peers rename laptop work-laptop
nas$ iroh-toolbox peers rm work-laptop

# After losing a machine
nas$ iroh-toolbox peers rm laptop
nas$ iroh-toolbox id rotate --address
This machine's address changed. Pair again with: phone, desktop
```

## Commands

### `id`

| Command | Effect |
| --- | --- |
| `id` | Shows this machine: name, short machine key, paired machines (`--json`); creates the identity on first use |
| `id rename NAME` | Sets the name announced when pairing; default: the hostname |
| `id rotate --address` | New address salt; the machine key stays |
| `id rotate` | New machine key and salt |

- **Name:** stored with the identity in the configuration directory. Without
  one, the hostname is used, lowercased and reduced to the slot name rules
  (1–32 lowercase letters, digits, hyphens), else `machine`.
- **Short machine key:** the first and last four characters of the machine
  key, as in the `MACHINE` column of `peers` on the other machine.
- **`id rotate --address`** is for a removed or lost machine: it still knows
  this address and could disrupt its slots by registering their keys at the
  relay. The command lists the paired machines; pairing again with each
  refreshes the address they store. A running server
  [warns](#address-takeover-warning) when another machine takes over one of
  its slot addresses.
- **`id rotate`** is for a leaked identity file. The command lists the paired
  machines and tells the user to run `peers rm NAME` on each of them, then pair
  again: the old machine key must disappear from their books.
- Both take effect for servers started afterwards; restart running servers.

### `pair`

| Form | Effect |
| --- | --- |
| `pair` | Creates a one-use code and waits for the other machine |
| `pair code:WORDS` | Joins with the other machine's code |

Options: `--as NAME` (name announced for this pairing only), `--name ALIAS`
(local name for the other machine), `--code-words N`, `--pairing-timeout`.

- **Confirmation number:** always required, with no option to skip it: pairing
  records a trusted machine that may join peer-facing servers, and both people
  are at their machines. Each machine shows its own number and asks for the
  other's, typed by its person; a plain `yes` is never accepted. `pair`
  therefore needs interactive terminals on both sides.
- **Server filters:** pairing asks no slot-access question. Each running server
  chooses its slot and optional `--allow`; pairing does not start a service.
- **Announced name:** the machine name from `id` (`id rename`, or the
  hostname); `--as` overrides it for one pairing.
- **Local name:** `--name` if given, else the name announced by the other
  machine. Each machine chooses its own alias for the other; the announced name
  is a suggestion, the machine key is the identity.
- **Name conflict:** if the local name is already used by another machine, an
  interactive terminal asks for another one; otherwise the pairing aborts
  before completion with "name taken; retry with --name ALIAS". Nothing is
  stored on either side.
- **Same machine again:** pairing a machine whose key is already in the book
  replaces its address and keeps its local name (unless `--name`). This is how
  a peer is refreshed after `id rotate --address`. After `id rotate`, the key differs:
  the old entry must be removed with `peers rm` first, never replaced silently.
- `--direct-only` is refused, as for every code.

### `peers`

| Command | Effect |
| --- | --- |
| `peers` | Lists paired machines: local name, short machine key and pairing date (`--json`) |
| `peers rename OLD NEW` | Changes a local name |
| `peers rm NAME` | Removes a machine; its next peer-authenticated connection here is refused (tunnel, pipe, files, chat and peers-only diagnostics) |

Every setting is local. Removing a machine here does not remove this machine
from the other's book. A server's `--allow` applies only to that server, not to
the other machine or another local slot.

### `tunnel tcp` and `pipe`

- **Servers** always run on a slot: `--slot NAME`, or the command name by
  default (`tunnel`, `pipe`). A connection is accepted only when the client is
  in the peer book and, with `--allow NAME` (repeatable or comma-separated),
  listed by that server. No separate slot grant is required.
- A server refuses to start with no paired machine or when an `--allow` name
  is unknown. The diagnostic points to pairing or the peer list, not a grant.
- The server reads the peer book on every incoming connection: `peers rm`
  and new pairings apply without a restart. Connections already open continue.
  Changing the `--allow` filter requires restarting that server.
- **Clients** accept only `peer:NAME[/SLOT]` targets. Tickets are refused:
  "tunnel and pipe connect to paired machines only; pair with
  `iroh-toolbox pair`, then use peer:NAME".
- The server prints the client command with its announced name:
  `iroh-toolbox tunnel tcp connect peer:nas/ssh` ("use the name this machine
  has in your `peers` list").
- Slot names `all` and `none` are reserved.
- Multiport tunnels keep `--to PORT=HOST:PORT` and
  `--bind HOST:PORT=REMOTE_PORT`; selector `0` designates a single destination.

### Direct connections without a relay

Access control does not depend on the path: the slot handshake proves both
machines whether packets go through a relay or not. A relay only helps the
client find the server, so it can be replaced by a known address:

```console
nas$ iroh-toolbox tunnel tcp serve --to 127.0.0.1:22 --slot ssh --allow laptop --direct-only --port 7777
Serving slot ssh to: laptop
Direct addresses: 192.168.1.10:7777
laptop$ iroh-toolbox tunnel tcp connect peer:nas/ssh --direct-only --addr 192.168.1.10:7777
```

- **`--port PORT`** on `tunnel tcp serve`, `pipe serve`, `ping serve` and
  `bench serve` fixes the UDP port of the server endpoint (iroh `bind_addr`),
  with or without a relay; useful for a firewall rule. A port already in use
  is an error.
- **`--direct-only` is accepted on slot servers:** they register with no
  relay, publish nothing, and print their direct addresses.
- **`--addr HOST:PORT`** (repeatable) on `tunnel tcp connect`, `pipe connect`,
  `ping connect` and `bench connect` adds direct addresses to a `peer:` target.
  With a relay they are a shortcut to the direct path; with `--direct-only`
  at least one is required ("the server cannot be found without a relay; pass
  --addr HOST:PORT printed by the server").
- `pair` still needs a relay, like every code: machines pair once with a relay
  (public, or a local one), then tunnels and pipes work without one.

### `ping` and `bench`

Diagnostics only: ticket mode stays, without client authentication. With
`--slot`, anyone who knows the slot address can use the server (it only
authenticates itself to the client); paired machines reach it through
`peer:NAME/SLOT`.

`--peers-only` makes the server authenticate its clients too. It implies a
slot (`ping` or `bench` unless `--slot NAME`), asks for the client proof and
serves any machine of the peer book. `--allow NAME` (repeatable or
comma-separated) implies `--peers-only`
and narrows it to those machines. Refusals use close codes 30 (not paired),
32 (not listed by `--allow`) and 33 (invalid client proof). Clients need no flag: the server's proof says
whether it expects the client frame, and only the server decides. Without
these flags nothing changes. `id:` targets are removed.

### Targets

| Target | Accepted by |
| --- | --- |
| `code:WORDS`, `code:` (prompt) | `receive`, `chat connect`, `pair` |
| `peer:NAME[/SLOT]` | `send PATH` (default remote slot `inbox`), `receive` (`send`), `chat connect` (`chat`), `tunnel tcp connect`, `pipe connect`, `ping connect`, `bench connect` |
| ticket | `ping connect`, `bench connect`, `ticket inspect` |

Targets are positional. `send` takes the path first and an optional peer target
second. `receive --inbox` and `chat serve` take no target; `chat connect`
requires one. See [option conflicts](#options-and-combinations).

## Files and chat

### User model

Three file workflows coexist:

1. **By code:** the sender creates a code and the receiver enters it.
2. **To an inbox:** the receiver opens an inbox; the sender initiates a
   transfer to it.
3. **From a share:** the sender makes content available; receivers fetch it
   while the command is running.

Sharing does not replace the inbox. It pushes nothing to other machines and
requires no inbox on the receiving machines.

Targets use one positional grammar: `code:WORDS` or `peer:NAME[/SLOT]`,
depending on the command. `NAME` is the local peer-book alias; `SLOT`
identifies the service running on that machine. Examples without `/SLOT`
use the service's default slot.
For `send`, the path is the first argument and the optional target is the
second: `send PATH peer:NAME`. There is no `--peer NAME` option.

Chat roles are explicit, as with the other services: `chat serve` opens a
session and waits; `chat connect TARGET` joins one. The role is never inferred
from the presence or absence of a target.

### Code-based modes

- `send PATH` without a target or `--peers-only` prints a code; `--code`
  disappears, while `--code-words` and `--verify` remain available.
- `receive code:WORDS` uses the supplied code; `receive code:` and
  `receive` alone ask for the words at the `Code:` prompt. Process arguments
  are visible to other local users; entering the code at the prompt avoids
  exposing it in those arguments.
- `chat connect code:` and `pair code:` prompt for the code in the same way.
  Without a terminal, refuse these forms and point to `code:WORDS`.
- `chat serve` without `--peers-only` creates a code and also accepts peers;
  `chat connect code:WORDS` joins using the code. `chat` alone shows help,
  and `chat connect` requires a target. Implicit roles are removed.
- Resuming a code-based reception is unchanged: the receiver reuses the iroh
  key saved in `.iroh-partial-…`, and the sender serves only that identity.
- Chat file offers remain inside the authenticated session and are accessible
  only to its participant.

### Command matrix

#### Files

| Command | Behavior | Normal completion |
| --- | --- | --- |
| `send PATH` | Prints a one-use code and waits for its receiver | After the transfer |
| `send PATH peer:NAME` | Sends to NAME's inbox | After the transfer |
| `send PATH --peers-only` | Waits for the first authorized peer to fetch the content | After one confirmed reception |
| `send PATH --peers-only --share` | Makes the same content available to multiple receivers | Ctrl-C |
| `receive` or `receive code:` | Prompts for the code and fetches the content | After reception |
| `receive code:WORDS` | Fetches using the code supplied as an argument | After reception |
| `receive peer:NAME` | Fetches the content offered by NAME's `send --peers-only` | After reception |
| `receive --inbox` | Accepts incoming transfers from peers | Ctrl-C |
| `receive --inbox --allow nas` | Accepts only NAS among the peers | Ctrl-C |
| `receive --inbox --once` | Accepts one successful reception | After that reception |
| `receive --inbox --allow nas --once` | Accepts one successful reception from NAS | After that reception |

`--allow` can also restrict `send PATH --peers-only`, with or without
`--share`; it implies `--peers-only`, so `send PATH --allow laptop` needs no
other flag. `--output` remains available on receptions, with its current
default: the working directory.

#### Chat

| Command | Behavior |
| --- | --- |
| `chat serve` | Opens a chat, prints a code and also accepts peers |
| `chat serve --peers-only` | Opens a chat for peers only, without a code |
| `chat serve --allow laptop` | Opens a chat for laptop only, without a code |
| `chat connect code:` | Prompts for the code and joins the chat |
| `chat connect code:WORDS` | Joins the chat with the supplied code |
| `chat connect peer:nas` | Joins the chat running on NAS, without a code |

**`--allow` implies `--peers-only`.** `chat serve --allow laptop` creates
no code: laptop alone can join. A filtered peer path next to a code is not
offered.

`chat` alone shows subcommand help and opens no session.
`chat connect` requires a target; `code:` then prompts for the words.
These subcommands replace the old implicit forms, with no alias that silently
chooses a role. This changes the syntax of code-based chat commands while
preserving their protocol and session behavior.

Chat remains a two-person conversation. It has neither `--share` nor `--once`.

### Options and combinations

| Option | Meaning | Scope |
| --- | --- | --- |
| `--peers-only` | Accept only peers, without creating a code | Server-mode `send` and `chat serve` |
| `--share` | Keep serving after each download | `send --peers-only` |
| `--inbox` | Wait for transfers initiated by peers | `receive` without a target |
| `--once` | Exit after one successful reception | `receive --inbox` |
| `--allow NAME` | Restrict incoming peers; implies `--peers-only` | Server-mode `send`, `receive --inbox`, `chat serve` |
| `--slot [NAME]` | Choose the served slot, using the existing naming rules | `send --peers-only`, `receive --inbox`, `chat serve` |

- `--allow` is optional, repeatable and accepts comma-separated names.
  Without it, the server accepts all paired machines. With it, this server
  accepts only the named machines among those peers, and never creates a
  code: `--allow` implies `--peers-only` wherever both exist (`send`,
  `chat serve`, `ping serve`, `bench serve`).
- `--allow` creates no pairing. An unknown alias is a local error.
- `--share` requires `--peers-only` or `--allow`. It does not apply to
  code-based sending or `send PATH peer:NAME`.
- `--once` requires `--inbox`. It works with or without `--allow`.
- `send --peers-only` already exits after one transfer without `--share`:
  no additional `--once` option is needed for `send`.
- A `send` target conflicts with `--peers-only`, `--share`, `--slot`
  and `--allow`. A `receive` target conflicts with `--inbox`, `--once`,
  `--slot` and `--allow`.
- `chat serve` accepts no target. `chat connect` requires a target of
  `code:WORDS`, `code:` or `peer:NAME[/SLOT]`, and refuses server options
  `--peers-only`, `--allow`, `--slot` and `--port`. `--addr` belongs
  to the peer client, not to `chat serve`.
- `--slot` does not implicitly turn a code-based send into a peer share:
  on `send`, it requires `--peers-only` or `--allow`; on `receive`, it
  requires `--inbox`.
- Code-specific options do not apply to modes without a code. Preserve their
  current scope on code-based paths. In a mixed chat, `--verify` does not
  confirm peer connections: its help must state that it applies to code pairing.
- Reject invalid combinations before any network activity.
- Do not introduce `--shared`, `--peer-once`, `--all-peers`, a `share`
  command or a second target syntax using `--peer`.
- `send PATH code:WORDS` is out of scope: in code mode, the sender always
  creates the code.

### Inbox and targeted sending

```console
laptop$ iroh-toolbox receive --inbox --output ~/Downloads
nas$    iroh-toolbox send ./photos peer:laptop

# Variant: receive once from NAS, then exit
laptop$ iroh-toolbox receive --inbox --allow nas --once --output ~/Downloads
nas$    iroh-toolbox send ./photos peer:laptop
```

- The inbox stays open until Ctrl-C, except with `--once`.
- It receives one transfer at a time. A second sender waits its turn, with
  a bounded wait and a visible status; a busy server must not be reported
  as absent.
- Each transfer is limited by `--max-bytes`, verified with BLAKE3 and
  published into `--output` only after validation.
- The inbox never overwrites or merges into an existing destination.
  A collision refuses the transfer and informs the sender. Reject replacement
  options that conflict with this rule.
- Each transfer shows the sender's local name.
- A refusal, an unauthenticated attempt or an interruption does not consume
  `--once`. After the first success, no new reception starts, and other
  senders are told that the inbox is closing.
- Without `--once`, a transfer-specific failure does not close the inbox.
- If the inbox is unavailable, `send PATH peer:NAME` retries until
  `--timeout`, then reports:
  `NAME is not receiving; run 'iroh-toolbox receive --inbox' on NAME`.
  For a named slot, the hint includes the corresponding `--slot`.
  Authorization refusals are reported as such, without these repeated attempts.
- After an interruption, repeating the same send resumes already verified
  data if the inbox and the same content are available.

### Receiving from send and persistent sharing

```console
# One receiver
nas$    iroh-toolbox send ./photos --peers-only
laptop$ iroh-toolbox receive peer:nas --output ~/Downloads

# Multiple receivers; none needs an inbox
nas$     iroh-toolbox send ./photos --peers-only --share
laptop$  iroh-toolbox receive peer:nas
desktop$ iroh-toolbox receive peer:nas

# Restrict receivers
nas$ iroh-toolbox send ./photos --peers-only --share --allow laptop,desktop
```

Without `--share`, the first authenticated and authorized receiver is pinned
for one logical reception. A refused or incompletely authenticated attempt
does not reserve that transfer. Other receivers are refused during reception
and resumption. The command succeeds after the receiver confirms publication.

With `--share`:

- Prepare an immutable offer for the lifetime of the share. Later changes to
  the source do not change the active offer.
- Accept multiple sequential and simultaneous receptions with bounded
  concurrency. An interrupted client blocks neither other clients nor the
  availability of the share.
- Each reception is independent. Finishing a download does not close the
  server; a peer can request the content again.
- Report results per receiver. Never claim that "all peers received":
  only peers that fetch the content receive it.
- The process stays open until Ctrl-C. Connection and transfer deadlines
  still apply, without expiring the whole share after the first download.
  Ctrl-C closes connections and releases the slot.

`receive peer:NAME` exits after its reception even if the share stays open.
It retains the existing reception limits, validation, publication protections
and replacement options; no silent overwrite is allowed.
Repeating the command resumes interrupted content while the same offer remains
available.

### Chat session

```console
# Open a mixed chat and join by code
nas$    iroh-toolbox chat serve
laptop$ iroh-toolbox chat connect code:

# Open a peer-only chat for laptop and join by name
nas$    iroh-toolbox chat serve --allow laptop
laptop$ iroh-toolbox chat connect peer:nas
```

- `chat serve` opens both code and peer paths; `chat serve --peers-only`
  and `chat serve --allow NAME` open only the peer path. An empty peer book
  must not prevent the code-based behavior of `chat serve`.
- The first participant admitted after authentication, by code or as a peer,
  becomes the session's only counterpart. Admission is atomic across both
  paths: no race may accept two participants.
- Once the session is assigned, close the other admission path and refuse
  subsequent participants. An unauthenticated connection cannot take the place.
- `chat connect peer:NAME` waits within `--timeout` when the server is
  unavailable. Distinguish unavailability, refusal and a busy session.
- After a connection loss, only the identity pinned to this session may
  reconnect. The session does not reopen to all peers.
- Preserve existing messages, acknowledgements, interactive commands and
  file offers. Files are accessible only to the session's counterpart.

### Chat code lifetime and revocation

A chat code is a separate admission credential: its holder can complete code
pairing without a machine pairing in the peer book. `peers rm` refuses that
machine's next peer connection but **does not revoke an unused, still-valid
chat code**. `chat serve --allow laptop` creates no code, so no code path
bypasses its filter.

Single-use limits successful admission to one participant, not necessarily
the intended participant. It is not, by itself, a time limit: the existing
code deadline still applies. Once either a code participant or a peer is
admitted, the session is pinned and the other admission path closes. Preserve
the existing treatment of failed code-pairing attempts; an unused code is
not promised to survive every failed attempt.

- On the human-facing code announcement, add the reminder:
  `Code: 4-amber-… (single-use: first admitted participant takes the session)`.
  Keep this status on stderr; any existing code-only stdout output must remain
  code-only so scripts and copyable targets are not changed.
- The connection banner identifies the actual admission path:
  `Chat ready; admission: code`, or
  `Chat ready; admission: paired machine NAME`, using the counterpart's local
  alias only when its machine identity was verified through the peer path.
  A code participant must not be labelled as a verified paired machine merely
  because it claims a familiar name. Do not label it as necessarily unpaired
  either: the code path did not establish that relationship.
- If a code was disclosed to an unwanted participant, stop the server with
  Ctrl-C and restart it to create a new code. If a session is already active,
  stopping the server also ends it; removing a peer does not disconnect an
  already open session.
- To expose no code path, use `chat serve --peers-only`, or
  `chat serve --allow NAME` to restrict which paired machines may join.

This version adds documentation and visible reminders, not a separate command
to revoke only the code while keeping the peer listener running. Such a
feature could invalidate the code's rendezvous and pairing state, including
in-flight admissions, without requiring durable machine pairing or replacing
the code flow with the slot handshake. It is deferred for scope and simplicity,
not because code revocation would inherently require machine pairing.

### Authorization

Authentication is mandatory on every peer path. Both machines prove their
machine keys; an EndpointId or knowledge of an address is not authorization.

One rule applies to **inboxes, `send --peers-only`, the peer path of chat,
tunnels and pipes**:

1. The client proves its machine identity.
2. That machine appears in the server's peer book.
3. If the server uses `--allow`, that machine appears in its filter.

The book records known machines; the slot identifies the service;
`--allow` selects who can use that server. No additional per-slot permission
is stored in the book. **`peers access` is removed from the target model**,
along with `pair --access`, the access question during pairing and the
`access` field in peer records. This shared rule resolves the previously
open policy choice for `send --peers-only`.

Without `--allow`, any paired machine can access the running server.
Knowing a slot name replaces neither pairing nor this filter. The filter
belongs to the running server, not to every service on the machine: another
slot can be opened with a different list. To change that list, restart the
server with the new `--allow`.

`--allow` closes the code path of chat: a mixed chat (code and peers) has
no filter. The `ping` and `bench` diagnostics retain their
distinction between open mode and `--peers-only` mode.

On peer paths, the server rereads the book on each new connection, including
reconnections: a removed peer is refused. Existing connections follow the
current policy and are not implicitly disconnected. Removing a peer from
the book does not revoke a still-valid chat code; see
[code lifetime and revocation](#chat-code-lifetime-and-revocation).

This decision also changes authorization for `tunnel` and `pipe`: their
slot and `--allow` options remain, but persistent grants are removed.
The [transition section](#transition-from-persistent-slot-grants) describes
how old peer books are handled.

### Slots

The new services reuse existing slot management without an additional
permission registry. `--slot NAME` selects a named service;
`peer:NAME/SLOT` reaches it. Without an explicit name, the defaults are:

| Server | Default slot | Client without a suffix |
| --- | --- | --- |
| `receive --inbox` | `inbox` | `send PATH peer:NAME` |
| `send PATH --peers-only`, with or without `--share` | `send` | `receive peer:NAME` |
| `chat serve`, with or without `--peers-only` | `chat` | `chat connect peer:NAME` |

```console
# Named share restricted to laptop
nas$    iroh-toolbox send ./photos --peers-only --share --slot photos --allow laptop
laptop$ iroh-toolbox receive peer:nas/photos

# Named inbox
laptop$ iroh-toolbox receive --inbox --slot depot --allow nas --once
nas$    iroh-toolbox send ./photos peer:laptop/depot

# Named chat
nas$    iroh-toolbox chat serve --slot equipe --allow laptop
laptop$ iroh-toolbox chat connect peer:nas/equipe
```

Distinct slots can run on the same machine. A slot name does not change the
expected protocol: `receive peer:nas/equipe` cannot download from a chat
server. Existing slot rules apply to naming, locking and diagnostics for an
incorrect slot or protocol. In a mixed chat, the code joins the session opened
by that server without the client needing to know its slot.

### Protocol, resumption and networking

| Service | Server process | Client | File provider |
| --- | --- | --- | --- |
| `inbox` | `receive --inbox` | `send PATH peer:NAME` | The `send` client |
| `send` | `send PATH --peers-only`, with or without `--share` | `receive peer:NAME` | The `send` server |
| `chat` | `chat serve`, with or without `--peers-only` | `chat connect peer:NAME` | Each participant for its own offers |

- Reuse the slot handshake; add the required protocols through the
  `iroh-toolbox/slot/2/<protocol>` ALPN. The code path keeps its protocol.
- The server determines whether client proof is required; the client's
  announcement cannot disable it. Content is inaccessible before
  authentication and admission.
- For files, after `ready`, the provider announces the offer: collection hash,
  format, names and size. The receiver validates names and limits, then
  downloads through iroh-blobs on the authenticated connection.
- The receiver confirms success only after verification and publication.
  The sender distinguishes confirmed success, refusal and an unknown outcome
  after a connection loss. Bytes served do not prove that files were published.
- Resumption identity binds the machine, offer and destination; a local alias
  alone is insufficient. Never mix two different offers after a restart.
- Handle the case where files are published but confirmation is lost:
  repeating the command must not overwrite the files or claim an unverified
  outcome. The confirmation mechanism and recognition of resumed transfers
  are implementation choices and require a dedicated test.
- Preserve existing protections against path traversal, special files,
  destinations changed during reception, excessive sizes and stalled
  connections. Escape remote text and never expose secrets in output.
- Peer servers support `--port` and `--direct-only`; peer clients support
  `--addr` and `--direct-only`, following existing slot rules. In direct-only
  mode, the client must supply the server's address.
- Mixed chat creates a code and therefore retains the relay requirement of
  codes. Use `chat serve --peers-only` to open a chat without a relay.
- Respect the existing lock: one server per slot and local identity.
  Report a conflict rather than silently replacing the server.

## Peer book

`peers.toml` in the configuration directory (`identity::home()`, next to the
identity file), created with mode 0600 on Unix and rewritten atomically under a
lock:

```toml
[peers.laptop]
machine = "q4mabk2r…8xne"   # machine public key M, z-base-32
salt = "t9c1wd5y…"          # address salt
announced = "laptop"        # name announced at pairing time
paired = "2026-11-01T10:12:00Z"
```

A machine key appears at most once. Names follow the slot name rules.

## Transition from persistent slot grants

This target spec removes `peers access`, `pair --access`, the access question
at pairing, the `MAY REACH HERE` column / JSON access property, and the peer
record's `access` field. Existing source code and user documentation still
describe the previous model until this change is implemented.

The earlier decision remains in [peer-access-decision.md](peer-access-decision.md)
as historical context. The current decision is paired identity plus each
server's slot and optional `--allow`. A slot is a service address, not an
authentication mechanism or a persistent permission entry.

Old peer books are not converted. Per-slot access only existed in
unreleased development builds, so a `peers.toml` that still holds an `access`
field is refused by every command that reads the book, with an explicit
message: delete that file and pair again with each machine. The message also
explains that a server without `--allow` now accepts every paired machine.
Nothing is rewritten or discarded silently, and no old restriction is
claimed to be enforced.

## Pairing exchange

`pair` reuses the [code pairing protocol](../docs/reference/pairing-protocol.md)
with the new purpose `pair`: rendezvous, SPAKE2 and key confirmation are
unchanged, and the confirmation number is mandatory. The payload frames change:

```text
10. A → B  payload  { peer: { machine: MA, salt: sA, name: nameA, signature: Sign_MA(S_A) } }
11. B → A  payload  { peer: { machine: MB, salt: sB, name: nameB, signature: Sign_MB(S_B) } }
12. A → B  done     { mac: HMAC(done, "host" || transcript) }
13. B → A  done     { mac: HMAC(done, "joiner" || transcript) }

S_X = "iroh-toolbox/pair/v1" || len(role_X) || role_X || T || machine || salt || len(name) || name
T   = transcript after the confirmation-number ready frames
```

- The signature proves that each side holds the machine key it announces, for
  this session only: a paired machine cannot register someone else's key.
- Each side checks the other's signature and its local name before sending
  `done`. There is no access question; a machine writes the
  record only after it has verified the other side's `done`.
- New failure reason: 13 name taken.

## Peer authentication on slots

The slot handshake moves to version 2 (ALPN `iroh-toolbox/slot/2/<protocol>`),
on the first stream of every connection:

```text
1. C → S  hello   { version: 2, slot, nonce_c }
2. S → C  proof   { signature: Sign_MS(server statement), nonce_s }
          C verifies the signature against the machine key MS stored in its peer book
3. C → S  client  { machine: MC, signature: Sign_MC(client statement) }   peer-authenticated services
          S verifies the signature, then checks that MC is in the peer book
          and that --allow lists it when this server has a filter
4. S → C  ready   { }

server statement = "iroh-toolbox/slot-auth/v2" || len(protocol) || protocol || len(slot) || slot || E || client endpoint ID || nonce_c
client statement = "iroh-toolbox/peer-auth/v1" || len(protocol) || protocol || len(slot) || slot || E || client endpoint ID || nonce_s
```

- **The server alone decides whether step 3 is required**, from the protocol
  it serves. Tunnel, pipe, inbox, send and chat peer servers require a valid
  client frame. The ALPN or slot announced by the client never waives it.
  `ping` and `bench` require it only in peers-only mode (also implied by `--allow`).
- **The server does not send its machine key**: the client verifies against the
  key it paired with, so a stranger who reaches an endpoint and guesses a slot
  name learns nothing that links the machine's slots. A client translates the
  close codes 30, 32 and 33 only after this verification.
- **The signature is verified before the peer book is read**, so a client
  that does not hold a machine key learns neither whether that key is paired
  nor whether this server would admit it.
- The client endpoint ID is authenticated by iroh's TLS, so a proof cannot be
  relayed to another connection; `nonce_s` prevents replay.
- Signatures are verified with Ed25519 strict verification; every
  variable-length signed field is prefixed with its length.
- No service data is exchanged on a peer connection before `ready`.
- Close codes: 30 not paired, 32 not allowed by `--allow`, 33 invalid client
  proof. Code 31 (former slot-grant refusal) is retired, not reassigned to a
  new meaning. The diagnostics and evidence required to report them follow.

### Authentication diagnostics

Keep server authentication, client authentication, admission and service
availability distinct. The local alias `NAME` and requested `SLOT` below come
from the client's configuration, never from an untrusted remote label.

| Case | Evidence | Client message |
| --- | --- | --- |
| Server proof fails verification | Step 2 does not verify against the stored machine key and the expected signed context | `Cannot authenticate NAME: its response does not match the expected identity or handshake context; check the target and whether NAME rotated its identity` |
| Server verified, client not paired | Code 30 after a verified step 2; the server checked a valid client signature before looking up its key | `NAME does not know this machine; run iroh-toolbox pair on both machines` |
| Server verified, client excluded by its filter | Code 32 after a verified step 2 | `This NAME server (slot SLOT) does not allow this machine (--allow)` |
| Server verified, client proof invalid | Code 33 after a verified step 2; the server rejected the client proof before reading its peer book | `NAME rejected this machine's authentication proof (invalid client proof); check the local identity and protocol implementation` |
| Service unavailable or no usable server proof | Connection failure, ALPN incompatibility, handshake deadline or missing/malformed server proof | `Cannot establish an authenticated connection to slot SLOT on NAME: check that the server is running, the network and address, the slot name and compatible versions` |
| Peer-book refusal claimed before server verification | A remote sends 30, 32 or 33 before a verified step 2 | `Cannot authenticate NAME: the server did not provide a valid proof; its refusal reason is untrusted` |

The first case is a local verification failure, not a trusted remote close
code. `hello` has already been sent, but the client sends neither its step-3
machine proof nor service data after that failure. Do not replace the local
verification error with a close reason supplied by the unauthenticated peer.

An invalid server signature does **not** prove that a different machine key
was received: the server sends no machine key. A changed identity, an incorrect
slot/protocol/nonce context, malformed implementation or forged response can
all fail verification. The message must not assert "different machine key"
nor identify a specific cause as established. If an identity rotation is
confirmed, follow the existing `peers rm NAME` and re-pairing procedure;
do not automatically replace the stored identity.

Code 33 describes a failure to validate the supplied client proof, not an
unknown machine. A client key/signature encoding that cannot be validated or
a signature that does not verify gets the same code and fixed reason
`invalid client proof`, regardless of whether the claimed key is paired.
The server performs no peer-book lookup on this path and sends no `ready`.
A missing proof or an expired deadline is an incomplete handshake, not
evidence that a received signature was invalid. Failure to read the peer book
also must not be reported as an invalid proof or an unknown peer.

Rotating or regenerating a client identity does not inherently produce code
33: a valid signature under the new key reaches the peer-book lookup and gets
30 if that key is absent. A corrupt identity file may instead fail locally
before any connection. Do not prescribe re-pairing as the automatic remedy
for code 33. This code refines an existing rejection; it is a diagnostic
improvement, not a new authentication protection.

An availability failure does not prove that the server stopped, that the slot
is wrong or that the peer refused access. A transport failure can occur before
step 2 because iroh also checks the endpoint identity. If the server proof
was already verified and a later step times out, report an incomplete
authenticated handshake with `NAME`, not an access refusal or a bad signature.
Keep retries within existing deadlines; do not retry a verified refusal or
a local proof-verification failure as if it were temporary unavailability.

Diagnostics, including verbose output, must not print received machine keys,
derived slot keys or addresses, signatures or nonces. Use local names and
locally generated explanations; do not echo arbitrary remote close text.
These error-path rules do not remove the explicit local identity displays
of `id` and `peers`.

## Address takeover warning

Every holder of a machine's address salt can compute a slot's key `E` and bind
an endpoint with it: current peers, removed or lost ones, and anyone with a
copy of the identity file. A relay delivers a key's traffic to its newest
registration only, and n0's DNS keeps the newest record signed by that key.
Another holder can therefore take a running slot's traffic without any proof;
clients then fail server authentication. The server cannot prevent this. It
detects it and tells the user what to do:

```console
nas$ iroh-toolbox tunnel tcp serve --to 127.0.0.1:22 --slot ssh --allow laptop
Serving slot ssh to: laptop
warning: another machine has taken slot ssh's address at the relay; paired machines cannot reach this slot through it
  It holds this machine's address: a paired, removed or lost machine, or a copy of this machine's identity file.
  If you removed a machine, run `iroh-toolbox id rotate --address`, then restart this server; `--direct-only` servers are not affected.
note: slot ssh is reachable through the relay again
```

- **Servers concerned:** every slot server that registers at a relay: tunnel,
  pipe, inbox, `send --peers-only`, `chat serve` (its slot endpoint, not a code
  rendezvous) and `ping`/`bench` with `--slot`. Not `--direct-only` servers,
  which neither register nor publish, nor clients, nor code endpoints.
- **Relay takeover** (public relays and `--relay`): the relay sends
  `SameEndpointIdConnected` to the registration it displaces and stops
  delivering to it, then `Healthy` once the other registration leaves. iroh
  1.4.1 exposes both only as `tracing` events of its relay actor, not in
  `RelayStatus`. Capture them with a `tracing` layer limited to these events,
  independent of the verbosity, and attribute them to the slot endpoint through
  the span active when it is bound (iroh parents its socket span to it). iroh
  is pinned exactly; the local-relay test catches a change on upgrade.
  This adds one dependency, `tracing-subscriber` with `default-features =
  false` and the `registry` feature only (no formatting, no `RUST_LOG`
  filter); `tracing` is already built through iroh. Both go away once iroh
  reports the status in `RelayStatus` (see later work).
- **DNS record replaced** (public relays only, where the slot publishes `E`
  through n0's DNS): one TTL after the endpoint is online, so that its first
  publication has landed, and then at iroh's republication interval, resolve
  the slot's own record. Warn ("another machine
  has replaced slot ssh's address record; paired machines may reach it instead
  of this server") when a record that verifies under `E` differs from what this
  endpoint publishes on two checks at least one TTL apart, so that a change of
  home relay is not mistaken for a takeover. Only a holder of `E`'s key can sign
  such a record.
- **Output:** on the human status output (stderr; stdout contracts such as a
  pipe's data are unchanged), once per episode and per signal, followed by the
  recovery note when the relay reports `Healthy` or the record matches again.
  It names the slot only: no key, address, relay URL, record content or remote
  text. The causes are listed, never a specific machine: the server cannot know
  who holds the address, and current peers know it too.
- **No reaction:** the server keeps running and does not register again to win
  the address back; that would make the relay alternate between the two.
- **No false positive** on start, restart of the same slot (an exiting process
  stays silent), relay reconnection or home-relay change of the process
  itself, connections of clients, or another slot of the same machine.
- Local only: no wire change.

## Security properties

| Threat | Outcome |
| --- | --- |
| Relay operator, or anyone who learns an address or endpoint ID | Refused by peer-authenticated services: not in the peer book. Open ping/bench (a ticket, or `--slot` without `--peers-only`) serve measurements to anyone who knows the address, by design |
| Paired machine reaching a running peer service without `--allow` | Admitted after authentication; no additional slot permission exists |
| Paired machine outside `--allow` | Refused |
| Removed peer | Refused from its next peer-authenticated connection; existing sessions continue. Open ping/bench and a valid chat code are separate admission paths |
| Chat code disclosed before use | Its holder can join first while the code remains valid, even after `peers rm`; single-use prevents a second admission, not an unwanted first participant. Use `--allow`, or stop and restart the server |
| Pairing code overheard before use | Pairing fails or attaches the eavesdropper; the mandatory confirmation number must be compared over a trusted channel |
| Pairing code sent by an impostor and confirmed | You pair with the impostor; it can reach running peer services whose `--allow` filter does not exclude it. Pairing itself starts no service |
| Peer announcing someone else's machine key | Rejected: the record is signed with that key over the session transcript |
| Replayed or relayed client proof | Rejected: server nonce and client endpoint ID in the signed statement |
| Invalid client signature for a paired or unknown key | Same code 33 before any peer-book lookup; no information about book membership is disclosed |
| Forged code 30, 32 or 33 before server authentication | Treated as an untrusted refusal, never as an authenticated statement by the named peer |
| Client posing as `ping` on a tunnel slot | Rejected: the server requires the client proof from its own protocol |
| Identity file of a paired machine leaked | The holder impersonates that machine to its peers until they remove it; the machine runs `id rotate` and pairs again |
| Peer book modified locally | Needs write access to the user account; file 0600 |
| Address known to a removed or lost machine | Addresses are never displayed, so only current and former peers know them. A former peer can disrupt slots by registering `E` at the relay or replacing its DNS record (denial of service); the server [warns](#address-takeover-warning); `id rotate --address`, then pair again |
| Party that knows a slot's endpoint and name (former peer, relay operator, anyone reaching a direct `--port`) sends valid `hello`s without a key and stalls | Degrades but cannot block service: past-`hello` handshakes are at most half of the pending budget and 8 per direct source address (per /64 for IPv6), a random one gives way, and the client frame must follow the proof within max(1 s, 3 × RTT). A displaced client is closed with code 23 and retries within `--timeout`, so it is kept out only while valid `hello`s keep arriving faster than it can complete its own few round trips. Mitigations: a non-default slot name on an exposed `--port`; `id rotate --address` |
| Paired machine misusing access | Reaches running peer services whose local filter admits it; forwarded services keep their own authentication |

## Wire changes

- Added: pairing purpose `pair`, slot handshake v2, close codes 13, 30, 32 and 33.
  The old slot-grant refusal code 31 is retired.
- Removed: `DUMBPIPEV0` and its `hello` handshake, the legacy single-destination
  mode, ticket-mode `tunnel` and `pipe` servers (`iroh-toolbox/tunnel-tcp/1`),
  blob and endpoint tickets for `send` and `chat`, `id:` targets.
- Unchanged: iroh-blobs ALPN for files (served only to the paired identity),
  chat ALPN, ping and bench ALPNs.

## Documentation and tests

- Docs: README, `security.md` (simplified), `slots.md`, `pairing-protocol.md`,
  new `peers.md`, `cli.md`, `compatibility.md` (sendme and dumbpipe cited
  factually after checking their upstream sources), guides (quick start, files,
  chat, streams, tunnels), troubleshooting, the transfer demo assets (they show
  `send --code`), and `notes/changelog-0.1.0.md`. The identity file must be
  documented as a secret: never synced, shared or committed.
- Tests (local relay): unpaired machine refused; paired machine accepted
  without a separate grant; paired machine outside `--allow` refused;
  distinct slots enforce their own filters; `peers rm` and new pairings apply
  without a server restart; an old book with `access` is refused with the
  delete-and-pair-again message and left unchanged; replayed proof
  refused; forged pairing record refused; a tunnel server refuses a client
  that skips step 3 or announces `ping`; name conflict leaves both books
  unchanged; tickets refused by tunnel and pipe; a peer-only server with no
  paired machine refuses to start; send and chat refuse tickets; `id rotate --address` makes a
  former peer's slot registrations useless; after `id rotate`, peers refuse the
  old machine key.
- Address takeover (local relay): a copy of the identity serving the same slot
  at the same relay makes the server print the warning once, and its departure
  the recovery note, after which clients get through again; the copy itself and
  a restarted slot print nothing, nor do a relay restart and client
  connections; the warning contains no key, salt or relay URL. Unit tests cover
  the span chain, the status texts and the DNS comparison with signed records.
  No automated end-to-end DNS scenario: it would need a second holder
  publishing another relay through n0's DNS.

### Documentation for diagnostics and chat-code revocation

Update these existing documents when implementing the behavior:

- `docs/reference/peers.md`: explain that `peers rm` blocks future peer
  connections but neither terminates existing sessions nor revokes a still-valid
  unused chat code; point to `chat serve --peers-only`.
- `docs/guides/chat.md`: show the code reminder, admission-path banners,
  single-use behavior and that `--allow` creates no code; advise Ctrl-C and a
  restart after code disclosure, or `chat serve --allow NAME`.
- `docs/reference/security.md`: distinguish code possession, machine pairing,
  session pinning and the diagnostics' evidence boundaries. Do not claim that
  a bad signature establishes a changed machine key or that identity rotation
  inherently yields code 33.
- `docs/reference/troubleshooting.md`: include the diagnostics table and an
  "An unexpected participant joined" entry. Code disclosure before admission
  is a possible cause; show the admission path and how to restart or restrict
  the next session. Do not claim disclosure was proved by an unexpected join.
- Address takeover: the warning and its remedy in `docs/reference/peers.md`
  (incident response) and `docs/reference/slots.md` (threats); in
  `docs/reference/troubleshooting.md`, an entry for the warning, and under the
  failed server proof, the possibility that another holder of `NAME`'s address
  answered: ask `NAME` to check its server output.

### Acceptance criteria: authentication diagnostics

- A process at the expected slot endpoint presents a server proof signed by
  another machine key: the client reports local server-authentication failure,
  not code 30, and sends no step-3 proof or service data.
- The expected machine signs for a different slot, protocol, nonce or client
  endpoint: verification fails against the requested context.
- An address or endpoint mismatch that fails before step 2 remains a transport
  or availability failure. Simply changing an IP address is not a sufficient
  fixture for testing invalid server-proof verification.
- A verified server receives a valid proof from a key absent from its book:
  code 30. Repeat with a correctly regenerated/rotated client identity to
  verify that the result is 30, not 33.
- A paired client excluded by `--allow` receives code 32 after server
  verification; the message names the locally requested slot.
- Invalid client signatures for both paired and unknown claimed keys get
  code 33 with the same reason. Verify that the peer book is not read and that
  `ready` is never sent. Missing proofs and deadlines remain distinguishable.
- Forged early 30, 32 and 33 refusals never become trusted authorization
  messages. A remote refusal cannot override a local server-proof failure.
- Unreachable service, no server proof, incompatible ALPN and a late handshake
  timeout produce bounded, evidence-appropriate diagnostics rather than an
  invented authorization refusal. Code 31 remains retired.
- Error and verbose output contains no received or derived keys, signatures,
  nonces or remote-provided identity labels; malicious close text is not echoed.

### Acceptance criteria: chat-code lifetime and revocation

- Emitting a chat code shows the single-use reminder on the human-facing
  status output while preserving any code-only stdout contract.
- After the server runs `peers rm`, the removed machine's peer connection
  is refused, but it can still complete code pairing with an unused, valid
  chat code. The banner reports code admission, not verified peer admission.
- After either code or peer admission wins, a further use of the code is
  refused; concurrent attempts cannot create a second participant. Reconnection
  is restricted to the established session identity.
- Peer admission displays the counterpart's verified local alias. A participant
  joining by code does not acquire that label by claiming a familiar name.
- `chat serve --allow laptop`, like `chat serve --peers-only --allow laptop`,
  emits no code, opens no code rendezvous and provides no code path around
  the filter.
- Ctrl-C followed by a restart produces a new code and invalidates the old
  rendezvous and any pending admission to the stopped session. An old code
  cannot join the restarted server. Existing code deadlines remain effective.

### Acceptance criteria: files and chat

- Verify every form in the command matrix and its conflicts; do not accept
  the previously proposed `--peer NAME` syntax.
- Chat: `chat` alone opens nothing; `chat connect` without a target fails
  with clear help. Refuse the old `chat code:WORDS`, `chat peer:NAME` and
  `chat --peers-only` forms, naming the required subcommand.
  `chat serve` refuses a target, and `chat connect` refuses server options.
- Targeted sending to a persistent inbox, followed by another transfer.
- Inbox `--once`, with and without `--allow`: exit after one success;
  refusals and interruptions do not consume the expected reception.
- Two senders to one inbox: waiting, bounded deadlines and a clear outcome,
  including when the inbox closes after its single successful reception.
- Reception from `send --peers-only`: one receiver, then exit; a refused
  client does not reserve the session.
- Sharing: two sequential receptions and two simultaneous ones, failure
  isolation, continued availability after success, another download by the
  same peer, clean shutdown on Ctrl-C and subsequent reuse of the slot.
- Shared authentication and peer-book rules, `--allow` filtering and
  revocation by peer removal; no `peers access` step is required.
- Default and named slots for inbox, send and chat; two distinct slots with
  different filters; refusal of slot conflicts and incorrect protocols.
- Files and directories: limits, collisions, resumption, a changed offer,
  a changed destination and confirmation lost after publication.
- Chat: code and peer admission, races between both paths, `--allow`
  filtering, peer-only mode without a code, refusal of a third participant
  and reconnection by the same identity; messages and files remain functional.
- Regression coverage for code-based behavior, including the new chat
  subcommands and prompts; direct peer connections and local-relay connections.
  No public-relay checks without an explicit request.

For implementation, use the `mise` tasks: focused checks, `lint`, `test`,
relevant `test:pairing` and `test:slots` scenarios with the local relay, then
`docs:cli`, `docs:check` and `docs:build`. List ignored or unexecuted
scenarios precisely.

Then update the affected guides and references and
`notes/changelog-0.1.0.md`, without presenting this specification alone as a
working feature delivery. Do not change `CHANGELOG.md` or the version for
this specification.

## Out of scope and later work

Automatic broadcasting to every peer, a daemon, deferred delivery to offline
machines, directory synchronization, group chat and a separate live command
to revoke only a chat code are out of scope. Codes
remain single-use. The data flows of `tunnel` and `pipe` remain unchanged;
their authorization follows the shared model above. `ping` and `bench`
retain their open and peer-only modes.

Other later work: slot keys derived by key blinding, so that knowing an
address no longer allows binding it (needs an iroh endpoint key built from a
scalar, not a seed), the address takeover state in iroh's `RelayStatus`
instead of its `tracing` events (both noted in
[iroh-upstream-requests.md](iroh-upstream-requests.md)), notifying peers
after address rotation, local discovery
(mDNS) for pairing and connections without `--addr`, remembering direct
addresses in the peer book, and the compatibility modes considered in the
historical note.
