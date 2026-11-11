# File transfers

Three workflows move files and folders:

| Workflow | Started first | Then | Requires |
| --- | --- | --- | --- |
| [By code](#send-and-receive) | `send PATH` prints a code | `receive code:` | Nothing in advance |
| [To an inbox](#send-to-an-inbox) | `receive --inbox` | `send PATH peer:NAME` | Machines paired once |
| [From an offer](#offer-files-to-paired-machines) | `send PATH --peers-only [--share]` | `receive peer:NAME` | Machines paired once |

The last two use no code: both machines prove the keys they exchanged with
`iroh-toolbox pair` (see [machine pairing](../reference/peers.md)). Every
reception verifies BLAKE3 hashes, applies `--max-bytes` and stages files before
publication.

## Send and receive

On **machine A**:

```sh
iroh-toolbox send ./my-folder
# Or: iroh-toolbox send ./archive.zip
```

A shows a one-use code of six words as `Code: WORDS`, followed by the complete
command for B, on stderr. On **machine B**, copy that command and type the code
at the prompt:

```sh
iroh-toolbox receive code:
# Code: (typed with echo off, outside the process list)
# Or choose a destination: iroh-toolbox receive code: --output ./received
```

`code:WORDS` also works, but the words are then visible to other local users
while the command runs, and the shell may keep them in its history.

Only B is served: the code is the only way in, and a ticket is not accepted.
See [use a code](#use-a-code) for verification and deadlines.

The content arrives as `./my-folder/` or `./archive.zip` in the current directory.
Use `--output` to choose an existing or new destination directory; its parent must
exist. If files or directories with the same names already exist, the receiver
asks before replacing or merging them, as described below. Each sender serves
one receiver, including its resumed connections, and then exits. An interrupted
transfer resumes, as described [below](#resume-an-interrupted-transfer).

The sender copies files into a temporary native iroh-blobs store, computes
BLAKE3 hashes, and sends the collection ticket to B inside the code's encrypted
channel. The receiver verifies the blobs before publishing them. The sender reports closure of the connection;
only the receiver reports successful verification and publication.

By default stderr shows only the necessary steps, as lines that stay on screen:

```text
Code: 7-river-…
On the receiving machine:
  iroh-toolbox receive code: --output ./received
  then type the code at the prompt (it stays out of the process list)
waiting for the other machine (keep the code private)
sending report.pdf 45% (11.8 MiB/26.3 MiB)
```

On a terminal, only the transfer line is rewritten in place; it keeps its final
count once the transfer ends. Redirected stderr records each step once, without
counters. The receiver counts blob data including collection metadata. The sender
counts served blob data including metadata and repeated requests; its percentage
is capped at 100% and does not claim remote success.

Global `-v` adds internal steps: scanning, copying and BLAKE3 hashing (per file),
endpoint registration, pairing phases, collection metadata, checking
paths/staging and publication, plus connection diagnostics. `-vv` prints every
periodic update as a separate line with raw byte counts.

Use `--no-progress` on `send` or `receive` to suppress phase/progress messages.
The receiver's final result and errors remain visible. Add global `--no-hints`
to suppress the suggested remote command; the `Code:` line stays. When stdout is
redirected (or with `--no-hints`), the sender's stdout also carries the bare
code on a single line for scripts; on a terminal, stdout stays empty. The
receiver's stdout stays empty. `pipe` keeps all diagnostics
on stderr and only binary data on stdout. Avoid merging stderr into stdout.

`receive --max-bytes 10GiB` limits the payload (default 10 GiB; an inbox
defaults to 1 GiB per offer, see [send to an inbox](#send-to-an-inbox)). Size options
accept positive integers in bytes or with case-sensitive suffixes: `B`, `kB`,
`MB`, `GB`, `TB` (powers of 1000), `KiB`, `MiB`, `GiB`, `TiB` (powers of 1024).
For example `64MiB` = `67108864` bytes; fractions and spaces are not accepted.
Use `--transfer-timeout SECONDS` for the transfer deadline on either side;
increase it deliberately for large transfers. A timeout or interruption never
confirms successful verification or publication.

Empty files are supported. Empty directories are omitted because standard blob
collections contain files, not directory entries; a completely empty directory
cannot be sent. No TAR archive or custom SHA-256 transfer protocol is used.

See [compatibility & limits](../reference/compatibility.md) for destination
filesystem requirements and metadata limitations.

## Resume an interrupted transfer

Keep `send` running. If the connection drops, the receiver reconnects by
itself. If the receiver stops (Ctrl+C, sleep, crash), run **the same command**
again: already verified data is kept and only the rest is downloaded.

```text
$ iroh-toolbox receive code:WORDS --output ./received
receiving and verifying BLAKE3 (8.0 MiB)
^C
partial transfer kept in ./.iroh-partial-018fd5… (3.8 MiB verified); run the same command again to resume, or delete it to discard

$ iroh-toolbox receive code:WORDS --output ./received
resuming a partial transfer: 3.8 MiB already verified
received my-folder → /home/me/received/my-folder — 8.0 MiB, 12 files, 41 s; BLAKE3 verified
```

Repeat `receive code:WORDS` with the same words. The code is not used again;
the receiver returns with the identity the sender already accepted. The sender waits for that receiver only, until it reports
publication or a deliberate stop, or until `--transfer-timeout` expires, which
also bounds every reconnection.

Partial data stays in `.iroh-partial-…` next to the destination: in `--output`
when that directory exists, otherwise in its parent. It holds the verified
blobs, the sender's ticket and the receiver's iroh key for this transfer,
readable by the current user only. The directory is removed after success and
after a deliberate stop: a declined replacement, `--no-replace`, a size limit or
invalid names. An interruption before any file content was downloaded leaves
nothing. If the sender has stopped, the same command fails with
`cannot reach the sender` and keeps the directory; delete it and start a new
transfer. Chat receptions are not resumable.

Transfers between paired machines resume too, from a `.iroh-partial-…`
directory bound to the sending machine, the offered files and the destination:
see the [inbox](#send-to-an-inbox) and [offers](#offer-files-to-paired-machines).

## Replace existing files

The receiver reads the collection's names first, before downloading any file
contents. When some names already exist in the destination, it lists them and
asks on the **receiving** machine. This applies to `receive code:` and
`receive peer:NAME`; an [inbox](#send-to-an-inbox) never asks and never
replaces anything:

```text
$ iroh-toolbox receive code:WORDS --output ./received
Existing items in "/home/me/received":
  replace  "my-folder/report.pdf"
  merge    "my-folder"/
Replace or merge these existing items? Type yes to confirm: yes
received my-folder → /home/me/received/my-folder — 8.0 MiB, 12 files, 3.2 s, 2.5 MiB/s; BLAKE3 verified
```

Type `yes` to approve; any other answer or EOF declines, and nothing is
downloaded. The list shows the first ten items for large sets. Existing
directories are merged: files present in the incoming collection replace files
of the same name, and unrelated files are kept. This does not delete and
recreate the whole destination directory. Symlinks, special files and
file/directory type conflicts are errors in every mode, also reported before
download. The transfer deadline also bounds the question.

The question needs an interactive stdin. Without one (a pipe, `/dev/null`,
cron, `ssh host command` without `-t`), an existing destination is refused
before download. Choose the policy explicitly in scripts:

```sh
# Replace existing regular files without a question.
iroh-toolbox receive code:WORDS --output ./received --force

# Refuse an existing destination, even in a terminal.
iroh-toolbox receive code:WORDS --output ./received --no-replace
```

The options are mutually exclusive.
A tool that allocates a terminal (for example `docker run -t` or `expect`) gets
the question and waits until it is answered or the transfer deadline expires.

Files are fully verified and staged before publication. If the destination
changed after the question (an item approved earlier was edited or disappeared,
or a new one appeared), nothing is published and local edits are kept. With
`--force`, the destination is checked against its state when the reception
started. Destinations are rechecked again during publication. Existing files are temporarily backed up during
replacement; a filesystem error triggers rollback of published files and
restoration of those originals. If rollback cannot restore a path, the error
identifies a retained `.iroh-backup-*` directory for recovery instead of deleting
the backups. That directory preserves the original paths relative to the output
directory, so each backup can be matched to its destination. Publication of an
entire directory is not atomic; abruptly killing
the process during publication can leave partial results and backups.

A received name starting with a dot at the top level, such as `.bashrc` or
`.ssh/`, is refused before download: a sender cannot add startup files or keys
to the destination, such as a home directory. Accept such names with
`--hidden` (`/receive --hidden DESTINATION` in a chat). Names starting with a
dot inside a folder, such as `my-project/.git/`, need nothing.

In the [chat](chat.md), `/receive DESTINATION` asks with `/confirm` or `/reject`
inside the conversation so message input remains available; use
`/receive --force DESTINATION` or `/receive --no-replace DESTINATION` to choose
in advance.

## Use a code

Keep the sender running.

```sh
# A: prints a private one-use code of six words.
iroh-toolbox send ./my-folder

# B: type the code at a prompt (echo off, outside the process list).
iroh-toolbox receive code: --output ./received
# Code:

# Or pass it directly; other local users can read it while the command runs.
iroh-toolbox receive code:<code-from-A> --output ./received
```

`receive` alone and `receive code:` ask `Code:` on the terminal with echo off
(on Windows the echo stays on). A's suggested command uses this form. Without a
terminal, pass `code:WORDS`, an argument that other local users of B can read
in the process list while the command runs.

Successful reception prints `BLAKE3 verified` and its destination on B. A reports
that the transfer finished; this does not prove publication on B. A shows the
code on stderr (`Code: WORDS`), and alone on stdout when stdout is redirected
or with `--no-hints`. File limits, the question before replacing, progress
options and the standard iroh-blobs collection format are described above and in
[compatibility](../reference/compatibility.md).

The machines meet through iroh's relays, with no mailbox or server of yours:
public relays by default, or the same `--relay` on both machines. Codes need a
relay to meet, so `--direct-only` is refused. Words are case-insensitive and may
be separated by `-` or spaces; a misspelled word is rejected before any
connection. Use `--code-words 8` on A for a longer secret.
`--pairing-timeout` bounds pairing separately from `--transfer-timeout`; it defaults to 300 seconds with the confirmation number and 120 with `--no-verify`.
After a wrong, expired or reused code, start a new sender and exchange a new code.

Each machine shows its own 6-digit confirmation number and asks for the other
machine's, so both commands need an interactive terminal: read yours to the other person by voice or in person, and
type the one they read to you. Three wrong numbers, an empty answer or the
timeout stop pairing before the ticket is delivered. The sender authorizes only
the receiver's paired iroh identity. See the
[pairing protocol](../reference/pairing-protocol.md) for its guarantees.
The number detects someone between the machines, not someone who sends you
their own code under another name: know who gives you the code, and compare
numbers with that person ([security model](../reference/security.md)).

To skip the number, pass `--no-verify` on **both** commands (the hint printed by
`send` includes it). Whoever uses the code first is then served, and nothing
tells you if the code was seen by someone else. Without a terminal, `send` and
`receive code:` refuse to start unless both get `--no-verify`; with it on one
machine only, both refuse with a mismatch message.

## Send to an inbox

Pair the machines once with `iroh-toolbox pair`. In the examples below, A is
named `nas` and B `laptop` in each other's `iroh-toolbox peers` list.

On **B**, open an inbox and keep it running:

```sh
iroh-toolbox receive --inbox --output ~/Downloads
```

B prints `Serving to:` followed by the paired machines it accepts,
the `send` command for A and `Receiving into … until Ctrl-C`. `--output`
defaults to the current directory. On **A**, send to B by name:

```sh
iroh-toolbox send ./photos peer:laptop
```

A ends with `laptop received photos: BLAKE3 verified and published`. B reports
`receiving photos from nas`, then `received photos from nas`, and keeps waiting
for the next transfer until Ctrl-C. To accept a single transfer from `nas`, then
exit:

```sh
iroh-toolbox receive --inbox --allow nas --once --output ~/Downloads
```

- **Size limit:** an inbox accepts at most 1 GiB per offer by default, because
  paired machines fill it unattended. Raise it deliberately with
  `--max-bytes 10GiB`. Code receptions and `receive peer:NAME` keep 10 GiB.
- **Who may send:** every machine in B's peer book. `--allow NAME` (repeatable
  or comma-separated) narrows this inbox, and `--allow all` means every paired
  machine, as without it; a machine outside it is told
  `this laptop server (slot inbox) does not allow this machine (--allow)`.
  `peers rm` and new pairings apply from the next connection, without a
  restart.
- **`--once`:** exits after one published transfer. A refusal, an unpaired
  machine or an interruption does not count. A sender still waiting is told
  `laptop's inbox is closing: it already received its single transfer (--once); nothing was sent`.
- **One transfer at a time:** another sender waits its turn and shows
  `laptop's inbox is receiving another transfer; waiting for its turn`. At most
  8 senders wait; the next one is told
  `laptop's inbox has too many senders waiting; try again later`.
- **Never replaces nor merges:** other contents under an existing name are
  refused before file contents are downloaded, and A reports
  `laptop refused photos: its inbox already holds files with these names; nothing was replaced`.
  Files that are already there and identical are recognized:
  `laptop already holds identical files; nothing was sent again`. `--force` is
  refused with `--inbox`.
- **Limits:** an offer larger than B's `--max-bytes` (default 10 GiB) is
  refused before download:
  `laptop refused photos: larger than its inbox accepts (--max-bytes)`.
  `--transfer-timeout` on A bounds the transfer.
- **Interruption:** B keeps the verified data in `.iroh-partial-…`; sending the
  same files again resumes. If the connection drops before B confirms, A says
  that the outcome is unknown: check B, then send again. Identical files are
  recognized, so nothing is published twice.

If no inbox runs on B, A keeps trying within `--timeout`, then reports:

```text
error: laptop is not receiving; run 'iroh-toolbox receive --inbox' on laptop
```

## Offer files to paired machines

Here the sender starts first and the receivers fetch; they need no inbox. On
**A**, offer the files and keep the command running:

```sh
iroh-toolbox send ./photos --peers-only
```

A prints `Serving to:` with the paired machines it accepts, the
`receive` command for B and
`Offering photos (…) to the first paired machine that receives it`. On **B**:

```sh
iroh-toolbox receive peer:nas --output ~/Downloads
```

Without `--share`, the first paired machine admitted is the only receiver: it
may reconnect and resume, and `send` exits once it confirms publication. Another
machine is told:

```text
error: nas is sending to another machine; ask nas to share with `send PATH --peers-only --share`
```

To serve several machines, add `--share`; `--allow` restricts the receivers:

```sh
iroh-toolbox send ./photos --peers-only --share
iroh-toolbox send ./photos --peers-only --share --allow laptop,desktop
```

With `--share`, A prepares the files once: later changes to `./photos` do not
change what it serves. Up to 8 machines receive at once; the next one is told
`nas serves too many receivers at once; try again later`. A machine may download
again, and an interrupted receiver blocks nobody. A reports each receiver's
result, such as `laptop received photos: BLAKE3 verified and published`, and runs
until Ctrl-C. Only the machines that fetch the files receive them.

`receive peer:NAME` exits after its own reception. It follows the
[replacement rules](#replace-existing-files) of a code reception: it lists
existing names and asks, or follows `--force` or `--no-replace`. Identical files
at the destination are recognized (`identical files are already in …; nothing changed`).
An interrupted reception resumes when you run the same command again while A
still offers the same files. If A offers nothing:

```text
error: nas is not sending; run 'iroh-toolbox send PATH --peers-only' on nas
```

## Slots, filters and direct connections

Each paired-machine server runs on a [slot](../reference/slots.md): `inbox` for
`receive --inbox`, `send` for `send --peers-only`. Give one a name with `--slot`
to run several, each with its own `--allow`; the other machine adds `/SLOT` to
the target:

```sh
# nas: a named share for laptop only
iroh-toolbox send ./photos --peers-only --share --slot photos --allow laptop
# laptop
iroh-toolbox receive peer:nas/photos

# laptop: a named inbox for one transfer from nas
iroh-toolbox receive --inbox --slot depot --allow nas --once
# nas
iroh-toolbox send ./photos peer:laptop/depot
```

One server runs per slot on a machine. A server refuses to start without any
paired machine, or with an `--allow` name that `iroh-toolbox peers` does not
list. To change the filter, restart the server. The code options
`--code-words`, `--no-verify` and `--pairing-timeout` are refused in these modes:
the pairing already authenticates both machines.

Without a relay, fix the server's UDP port with `--port` and give the other
machine the address the server prints:

```sh
# laptop
iroh-toolbox receive --inbox --direct-only --port 7777
# Direct addresses: 192.168.1.20:7777

# nas
iroh-toolbox send ./photos peer:laptop --direct-only --addr 192.168.1.20:7777
```

`receive peer:NAME` takes `--addr` in the same way; the sender then runs
`send PATH --peers-only --direct-only --port PORT`.
