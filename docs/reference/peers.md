# Machine pairing and peers

Peer services serve **paired machines** only: tunnels, pipes, inboxes
(`receive --inbox`), `send PATH --peers-only`, the peer path of `chat serve`
and `--peers-only` ping/bench servers. Pair two machines once with a one-use
code. Afterwards three things are separate: the peer book says **who** is
paired, a [slot](slots.md) names **which running service**, and each server's
`--allow` says **whom that server accepts**. Tunnel and pipe servers, which
expose a local service, refuse to start without `--allow`; the other servers
accept every paired machine without it. Pairing is a one-time act between two people at
their machines, and everything afterwards is local and revocable.

```console
nas$ iroh-toolbox pair
This machine: nas
Code: amber-falcon-tide-rocket-maple-orbit
On the other machine (keep the code private):
  iroh-toolbox pair code:
  then type the code at the prompt (it stays out of the process list)
waiting for the other machine (keep the code private)
Your confirmation number: 284 913
Read it aloud to the other person, and type the number they read to you.
Their confirmation number (empty to cancel): 730 165
Paired with laptop (machine q4ma…8xne)

laptop$ iroh-toolbox pair code:
Code:
This machine: laptop
Your confirmation number: 730 165
Read it aloud to the other person, and type the number they read to you.
Their confirmation number (empty to cancel): 284 913
Paired with nas (machine yx3c…k1oa)
```

(The words above are an example, not a usable code; the typed code is not echoed.
`pair code:WORDS` also works but shows the words to other local users while the
command runs.) Pairing needs a relay,
like every code: public relays by default, or the same `--relay` on both
machines. Afterwards [peer services can run without one](slots.md#without-a-relay).

## `pair`

| Form | Effect |
| --- | --- |
| `pair` | Creates a one-use code and waits for the other machine |
| `pair code:` | Joins with the other machine's code, typed at a `Code:` prompt (echo off; echo stays on Windows) |
| `pair code:WORDS` | Same, with the words as an argument (visible to other local users while it runs) |

| Option | Effect |
| --- | --- |
| `--as NAME` | Name announced to the other machine for this pairing only |
| `--name ALIAS` | Local name for the other machine |
| `--code-words N` | Words in the code when creating it (default 6) |
| `--pairing-timeout SECONDS` | Total deadline, including both people's answers (default 300) |

- **Confirmation number.** Always required, with no option to skip it: pairing
  records a trusted machine that may join your peer services. Each machine
  shows its own 6-digit number and asks for the other machine's: each person
  reads their number to the other, by voice or in person, and types the number
  they hear. Spaces are optional. After three wrong numbers, an empty answer,
  an interruption or the deadline, the pairing is cancelled on both sides. `pair` therefore needs an interactive terminal on both
  machines. Read [what the number proves](security.md#confirmation-number-6-digits).
- **No access question.** Pairing starts no service and asks nothing about
  slots: each server chooses its slot and, optionally, `--allow`.
- **Announced name.** The name set with [`id rename`](#id), or the hostname;
  `--as` overrides it for one pairing.
- **Local name.** `--name ALIAS` if given, else the name the other machine
  announces. Each machine chooses its own name for the other: the announced
  name is a suggestion, the machine key is the identity. Names are 1 to 32
  lowercase letters, digits or hyphens; `all` and `none` are reserved.
- **Name conflict.** If the local name already belongs to another machine, an
  interactive terminal explains and asks for another name; an empty answer
  cancels. Cancelling aborts the pairing with `name taken; retry with --name
  ALIAS`, and the other machine reports that the machine "has no free name for
  this one". Nothing is stored on either side.
- **Pairing the same machine again.** A machine already in the book keeps its
  local name (unless `--name` is given) and gets the address it announces now.
  This is how a peer refreshes the address it stores after `id rotate
  --address`. After `id rotate`, the machine key differs: the old entry must be
  removed with `peers rm` first, and a new pairing never replaces it silently.
- `--direct-only` is refused: the machines meet through a relay.

Nothing is written before both people confirmed, both machines checked each
other's signed record and `done` frames were exchanged (see the
[protocol](pairing-protocol.md#machine-pairing-pair)). If the deadline leaves
too little time to finish, the pairing is abandoned and nothing is stored. If
the second machine could not confirm that the first stored its entry, it prints
a warning naming `peers rm` so that you can check and repeat the pairing.

## Peer book

Pairings live in `peers.toml` next to the identity file (see
[identity file](slots.md#identity-file) for the directory). It is created with
mode 0600 on Unix and rewritten atomically under an exclusive lock
(`peers.lock`), so a server never reads a half-written book:

```toml
[peers.laptop]
machine = "q4mabk2r…8xne"   # machine public key, z-base-32 (52 characters)
salt = "t9c1wd5y…"          # address salt
announced = "laptop"        # name announced at pairing time
paired = "2026-11-01T10:12:00Z"
```

A machine key appears at most once, and names follow the slot name rules. On
Unix the file is refused if it belongs to another user or is writable by
others (`chmod 600` fixes it); a book that does not parse is an error, never an
empty book. Edit it with the commands below rather than by hand. The book
stores no per-slot permission.

**Books from development builds.** Development builds stored per-slot access
in an `access` field. Every command that reads the book refuses such a file,
leaves it unchanged and says: `peer book PATH holds per-slot access from a
development build, which this version no longer uses; delete it and pair again
with each machine (servers now accept every paired machine unless started with
--allow NAME)`. It is never converted, and no old restriction is enforced.

Pairing and `peers` commands take the lock. While another pairing (which holds
it while the other machine confirms) or `peers` command is running, a second one
prints `waiting for another pairing or peers command to finish` and continues
when the lock is free.

## `peers`

| Command | Effect |
| --- | --- |
| `peers` | Lists paired machines: local name, short machine key, pairing date (`--json`) |
| `peers rename OLD NEW` | Changes a local name |
| `peers rm NAME` | Removes a machine; its next peer connection here is refused |

```console
nas$ iroh-toolbox peers
NAME    MACHINE    PAIRED
laptop  q4ma…8xne  2026-11-01
nas$ iroh-toolbox peers rename laptop work-laptop
laptop is now named work-laptop
nas$ iroh-toolbox peers rm work-laptop
removed work-laptop; its next peer connection here is refused (tunnels, pipes, files, chat and --peers-only ping/bench)
Open sessions continue, and a chat code it already holds stays valid: restart that chat to create a new code
```

`peers --json` prints `{"peers": [{name, machine, announced, paired}]}`, with
the short machine key and the RFC 3339 pairing time. With an empty book,
`peers` prints ``No paired machines. Pair one with `iroh-toolbox pair`.``

Every setting is local. Removing a machine here does not remove this machine
from the other's book. Running servers read the book on **every incoming
connection**, chat reconnections included: `peers rm` and new pairings apply
without a restart.

### What `peers rm` does not revoke

`peers rm` refuses the removed machine's next peer connection. It does not:

- **end what is already open.** Tunnel connections, a transfer in progress and
  an open chat session continue until they end; stop the server to cut them.
  A chat participant admitted as a paired machine passes the handshake again
  after a connection loss, and is then refused.
- **revoke a chat code.** A chat code is a separate admission credential:
  whoever holds an unused, still-valid code can join `chat serve` by code, even
  after `peers rm`. It joins as a code participant (`Chat ready; admission:
  code`), never as the paired machine it was. If a code may have been
  disclosed, stop the server with Ctrl-C and start it again: the new server
  prints a new code, and the old one cannot join it. To expose no code at all,
  use `chat serve --peers-only`, or `chat serve --allow NAME` to name the
  machine.
- **stop open ping and bench servers.** Without `--peers-only` they do not read
  the book; only [`id rotate --address`](#after-an-incident) stops a former
  peer.

## `id`

| Command | Effect |
| --- | --- |
| `id` | Shows this machine: name, short machine key, whether the key is protected, paired machines (`--json`); creates the identity on first use |
| `id protect` | Encrypts the machine key with a passphrase typed twice; on a protected key, asks the current one and sets a new one |
| `id unprotect` | Asks the passphrase and stores the machine key unprotected again |
| `id rename NAME` | Sets the name announced when pairing; default: the hostname |
| `id rotate --address` | New address salt; the machine key stays |
| `id rotate` | New machine key and salt; a protected key stays protected, with the same passphrase |

```console
nas$ iroh-toolbox id
Name:     nas (announced when pairing)
Machine:  yx3c…k1oa
Key:      not protected (iroh-toolbox id protect)
Peers:    laptop
```

The short machine key (first and last four characters) is the `MACHINE` column
that the other machine shows in its `peers` list, which lets you check that
two entries designate the same machine. Without `id rename`, the name is the
hostname: its first DNS label, lowercased and reduced to the slot name rules, or
`machine`.

### After an incident

Both rotations take effect for servers started afterwards: restart running
servers, which keep the previous identity. If the key is
[protected](security.md#the-identity-file-is-a-secret), a rotation asks the
passphrase first.

| Situation | Do |
| --- | --- |
| A paired machine is lost, stolen or retired | On every machine that paired with it: `peers rm NAME`, then `id rotate --address`. The lost machine still knows their addresses and could disrupt their slots by registering them at the relay; the new address makes that useless. Then pair again with each remaining peer, which refreshes the address it stores. |
| The identity file of a machine leaked | On that machine: `id rotate`. It lists the paired machines. On each of them, run `peers`, remove the entry whose `MACHINE` is the old key with `peers rm NAME`, then pair again. |

After `id rotate --address` every peer must pair again, and nobody is notified:
tell them. Until then their stored address no longer reaches this machine.

`id rotate --address` prints `Pair again with: NAME…`. `id rotate` prints that
the old machine key is void and what to do on the other machines.

A running server warns when another machine takes one of its slot addresses:
`warning: another machine has taken slot SLOT's address at the relay…`, or with
public relays `warning: another machine has replaced slot SLOT's address
record…`. It then prints `note: slot SLOT is reachable through the relay again`
or `note: slot SLOT's address record points to this server again` once the slot
is back. The server cannot tell which machine it is: a paired, removed or lost
machine, or a copy of its identity file. If you removed a machine, follow the
first row above. `--direct-only` servers register nowhere and are not affected.

## Admission rules

A peer service accepts a connection only when **all** of these hold:

1. the client proves, with a signature, that it holds a machine key;
2. that key is in the server machine's peer book, read for this connection;
3. the server has no `--allow` (not possible for tunnels and pipes), or serves
   `--allow all`, or lists the client (`--allow NAME`).

- **No per-slot grant.** Being paired is enough to join any running peer
  service of the other machine that does not exclude you with `--allow`;
  tunnels and pipes name their machines, or `all`, at start. Pairing
  starts nothing: a service is reachable only while its server runs.
- **`--allow` belongs to one running server.** It is repeatable or
  comma-separated and narrows the paired machines that this server accepts.
  `tunnel tcp serve`, `tunnel http serve` and `pipe serve` require it
  (``name the machines to serve: --allow NAME (repeatable), or --allow all``).
  `--allow all` stands for every machine of the peer book, read at each
  connection like the filter of a server without `--allow`: later pairings and
  `peers rm` apply without a restart. `all` is a reserved name and cannot be
  combined with names (``--allow all cannot be combined with machine names``).
  Every command that takes `--allow` accepts `all`, with the meaning of
  `--peers-only` where `--allow` implies it;
  another slot can run with a different list. Names are resolved to machine
  keys when the server starts, so a later `peers rename` cannot redirect them;
  restart the server to change the list. A server refuses to start without
  any paired machine (``no paired machine; pair one with `iroh-toolbox pair`
  first``) or with a name that is not in the book (`` --allow NAME: no paired
  machine named NAME; see `iroh-toolbox peers` ``).
- **Per direction.** Servers on `nas` that accept `laptop` say nothing about
  what `nas` may join on `laptop`.
- **Chat.** `chat serve` admits one participant, by code or as a paired
  machine. `--allow` implies `--peers-only`: such a chat creates no code. With
  an empty book, `chat serve` is code-only. See the
  [chat guide](../guides/chat.md).
- **`ping` and `bench` slots** carry measurements only and are open by default:
  anyone who knows the slot address reaches them, a removed peer included, with
  no client proof. `--peers-only` (implies a slot) asks for the client proof and
  serves any machine of the book; `--allow NAME` implies `--peers-only` and
  narrows it.
- **Clients.** Tunnel and pipe clients accept only `peer:NAME[/SLOT]` targets;
  tickets and `id:` addresses are refused. `send PATH`, `receive` and
  `chat connect` take `peer:NAME[/SLOT]` or, for `receive` and `chat connect`,
  a code.

A server prints the paired machines it serves (`Serving slot ssh to: laptop`,
or `Serving slot ssh to: all paired machines (now: laptop, nas)` with
`--allow all`) and a ready-to-copy client command. A paired machine refused by `--allow` is
printed (`refused laptop on slot ssh: not listed by --allow`); unpaired machines
and invalid proofs are only visible with `-v`. See the
[slot handshake](slots.md#slot-handshake) for the exact checks and
[troubleshooting](troubleshooting.md#a-tunnel-or-pipe-is-refused) for the client
messages.

## Threats

| Threat | Outcome |
| --- | --- |
| Relay operator, or anyone who learns an address or endpoint ID | Refused by peer services: not in the peer book |
| Paired machine reaching a running peer service without `--allow` | Tunnels and pipes cannot run without `--allow`. Other services admit it after authentication; `--allow all` admits every paired machine; no other slot permission exists |
| Paired machine outside `--allow` | Refused |
| Removed peer | Refused from its next peer connection; open sessions continue. Open ping/bench and an unused chat code are separate admission paths |
| Pairing code overheard before use | Pairing fails or attaches the eavesdropper; detected by the confirmation number, always required |
| Pairing code sent by an impostor, number confirmed | You pair with the impostor; it can join the peer services you run whose `--allow` does not exclude it |
| Peer announcing someone else's machine key | Rejected: its record is signed with that key over the session transcript |
| Replayed or relayed client proof | Rejected: server nonce and client endpoint ID in the signed statement |
| Identity file of a paired machine leaked | The holder impersonates that machine to its peers until they remove it; the machine runs `id rotate` and pairs again |
| Peer book modified locally | Needs write access to the user account; the file is 0600 |
| Address known to a removed or lost machine | Addresses are never displayed, so only current and former peers know them. A former peer can disrupt slots by registering their keys at the relay or replacing their DNS records (denial of service); the server [warns](#after-an-incident). `id rotate --address`, then pair again |
| Paired machine misusing its access | Reaches the running peer services that admit it, as a local client of a forwarded service would |

Forwarded services keep their own authentication, and the
[security model](security.md) explains what pairing proves and does not.
