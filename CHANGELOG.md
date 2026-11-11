# Changelog

Notable published changes to iroh-toolbox are recorded here.

## [0.1.0] — 2026-11-11

First public release: one native executable for Linux and Windows that
sends files, chats and reaches TCP services between two machines over
[iroh](https://iroh.computer), without a server of your own.

### Highlights

- **Files and chat with a one-use code** — Send a folder, start a private
  conversation and accept file offers from one native executable; only the
  machine that types the code is served, after both people compared a 6-digit
  confirmation number.
- **Pair once, then no more codes** — `pair` links two machines with a code and
  a mandatory confirmation number. Paired machines push files to an inbox, fetch
  what another offers or shares, and chat by name; tunnels and pipes serve only
  the machines named by `--allow` (or `all`), other servers can narrow who they
  accept with it, and `peers rm` cuts a machine off.
- **Remote services for paired machines only** — Reach HTTP, SSH and other TCP
  services through a local port with `peer:NAME/SLOT`, or browse a folder or
  static site without a web server, over public relays, a relay you control or,
  once paired, direct addresses without any relay.

### Features

- **codes:** type a code outside the process list: `receive` (no target),
  `receive code:`, `chat connect code:` and `pair code:` ask `Code:` on the
  terminal with echo off, and the printed hints use this form. `code:WORDS` still
  works but is visible to other local users while the command runs. The code
  itself is shown on stderr as `Code: WORDS`; stdout carries it alone for scripts.
- **files:** `send PATH` shares a file or folder as a native iroh-blobs
  collection and prints a one-use code of six EFF words; `receive code:WORDS`
  downloads it, verifies BLAKE3 and publishes it only once complete. There is no
  ticket mode. Payloads are limited to 10 GiB by default (`--max-bytes`). An
  interrupted reception resumes when the same `receive` command runs again while
  the sender is still running; it pairs nothing again, so it asks for no
  confirmation number and needs no terminal. The final line names what was published, its
  absolute path, size, file count, duration and, from 1 MiB, the rate.
- **files:** existing names trigger a question before any download; `--force`
  replaces files and merges folders, `--no-replace` always refuses. Replacements
  keep backups and roll back on errors, and files changed during the download
  are never overwritten.
- **files:** a received top-level name starting with a dot (`.bashrc`, `.ssh/`)
  is refused before download unless the receiver passes `--hidden`
  (`/receive --hidden` in a chat), so a sender cannot add startup files or keys
  to a home directory. Such names inside a received folder are accepted.
- **files:** `receive --inbox` receives what paired machines push with
  `send PATH peer:NAME[/SLOT]`, one transfer at a time (other senders wait and
  are told so), until Ctrl-C, or `--once` for a single successful transfer. An
  inbox never replaces or merges: other contents under existing names are
  refused, identical files are recognized and left untouched. An inbox accepts
  at most 1 GiB per offer by default, because paired machines fill it
  unattended (`--max-bytes` raises it); code receptions and `receive peer:NAME`
  keep 10 GiB. An interrupted send
  resumes when it is sent again.
- **files:** `send PATH --peers-only` offers files to the first paired machine
  that runs `receive peer:NAME`, which alone may resume; `--share` keeps a
  snapshot available to every receiver until Ctrl-C, up to 8 at once, with one
  result line per receiver. `--allow NAME` narrows it and implies
  `--peers-only`. Resumption is bound to the machine, the offer and the
  destination.
- **codes:** SPAKE2 authenticates both machines without any mailbox server and
  only the machine that typed the code is served. The first three words locate
  the creator and the rest form the password; `--code-words` lengthens the
  password only. By default, each machine shows its own 6-digit number and asks
  for the other's, typed by its person; `--no-verify` on both machines skips it
  (whoever uses the code first is then served), and without a terminal the
  command refuses to start unless `--no-verify` is given. The pairing deadline
  is 300 seconds with the number and 120 without.
- **pair:** `pair` creates a code and `pair code:WORDS` joins it. The
  confirmation numbers are always exchanged: each person types the number the
  other machine shows; pairing starts no service and asks no access question. `--name` and `--as` choose local and announced names.
- **peers:** `peers` lists paired machines (name, short machine key, pairing
  date), `peers rename` and `peers rm` manage the book. Settings are local; a
  removal applies to the next connection of every peer-authenticated server
  without a restart, but neither ends an open session nor voids a chat code.
- **id:** `id protect` encrypts the machine key with a passphrase typed twice
  (Argon2id and XChaCha20-Poly1305, parameters and header authenticated in the
  file, identity file version 2); run again, it changes the passphrase, and `id
  unprotect` removes it. Any command that loads a protected key asks `Passphrase
  for the machine key:` once, on the controlling terminal (`/dev/tty`, so
  standard input may be a pipe) with echo off, before it announces readiness or
  opens a listener, and fails with a clear message without a controlling
  terminal; there is no passphrase in arguments, environment variables, files or
  standard input, and a protected key stops unattended servers. A fatal signal
  at the prompt restores the terminal.
  `id rotate` keeps the protection, `id` shows whether the key is protected, and
  version 1 files keep loading. Passphrases, derived keys, the machine key
  bytes, the code password and pairing keys are zeroized after use, except the
  `spake2` 0.4 state, a known limit. Only `id protect`, `id unprotect` and `id
  rotate` keep the passphrase, for the length of the command.
- **id:** `id` shows this machine's name, short key and paired machines;
  `id rename` sets the announced name, `id rotate --address` renews this
  machine's address after a peer is lost and `id rotate` replaces a leaked identity.
- **chat:** `chat serve` opens a two-person chat with a code and for paired
  machines; the other side joins with `chat connect code:` or `chat connect
  peer:NAME`. The first admitted participant takes the session, and the banner
  says how it was admitted. `--peers-only` opens no code and needs no relay;
  `--allow NAME` admits only those machines and implies `--peers-only`. Messages survive connection losses
  of up to 60 seconds. `/send` and `/receive` exchange files that the receiver
  accepts explicitly, alongside `/status`, `/cancel`, `/reject` and `/quit`.
- **chat:** an interactive line editor keeps incoming messages above the line
  being typed, with history, Tab completion of commands and paths, and
  `--timestamps auto|always|never`.
- **chat:** status lines stand apart from the conversation with a mark: `·` for
  information, `!` for a refusal, failure or mistake, `✓` for a completed
  outcome. The editor colors marks and sender names; the standard `NO_COLOR`
  environment variable turns every color off, the prompt included. A paired
  machine is named as in the peer book (`laptop> …`, `laptop offers photos`); a
  chat opened by code shows `peer`. A mistyped command suggests the closest one,
  and a command missing its path shows its usage.
- **pipe:** `pipe serve` and `pipe connect peer:NAME[/SLOT]` connect
  stdin/stdout between two paired machines, closing each direction independently.
  `pipe serve` requires `--allow NAME` or `--allow all`, like the tunnels.
- **tunnel:** `tunnel tcp serve --to HOST:PORT` exposes a TCP service on a slot
  and `tunnel tcp connect peer:NAME[/SLOT]` opens it on a local port. Servers
  accept paired machines only, must name them with `--allow NAME` (repeatable)
  or `--allow all` (every paired machine, read at each connection), refuse to
  start without `--allow` or without any paired machine, and print the
  machines served and the command for the client. One slot
  can carry up to 64 named services (`--to SERVICE=HOST:PORT`): the client opens
  the whole group on the ports the server announces, one service with
  `peer:NAME/SLOT/SERVICE`, or moves a service with `--bind SERVICE=HOST:PORT`;
  servers accept 64 concurrent connections by default (`--max-connections`).
- **tunnel:** `tunnel http serve PATH --allow NAME|all` serves a folder to the
  browsers of paired machines and `tunnel http connect peer:NAME[/SLOT]` prints the local URL to
  open, with no web server and no TCP port on the serving machine. The folder is
  read-only (`GET`/`HEAD`, single ranges), confined to its root, with dotfiles
  hidden unless `--hidden`; it shows a listing, and HTML, SVG and XML open
  without scripts (`Content-Security-Policy: sandbox`). `--site` serves a static
  site instead: `index.html`, `404.html` and scripts. Default slot: `http`.
  A request naming a domain in `Host` gets `421 Misdirected Request`, so a page
  rebound to `127.0.0.1` (DNS rebinding) cannot read the folder.
- **slots:** `--slot [NAME]` names what a server offers (default: the command,
  such as `tunnel`, `pipe`, `inbox`, `send` or `chat`). A slot keeps a stable
  address across restarts, derived from the machine identity, and the client
  proves its machine key on every connection. Refusals say what the handshake
  proved: not paired, not listed by `--allow`, or an invalid client proof.
  A server warns when another machine takes its address at the relay or
  replaces its DNS record, and notes when the slot is back. A server on its
  default slot prints `Serving to: NAME` and a client command without `/SLOT`.
- **direct:** once paired, `--port` fixes the UDP port of a server, `--direct-only`
  runs a slot without any relay and `--addr HOST:PORT` gives the client the
  address the server printed.
- **diagnostics:** `relay check` verifies relay registration, `ping` measures
  round trips and shows whether the path is direct or relayed, and `bench`
  measures throughput in both directions. They connect with a ticket or
  `peer:NAME[/SLOT]` (the server needs `--slot` or `--peers-only`). Servers are
  open by default; `ping serve` and `bench serve` accept `--peers-only` (implies
  a slot, serves any paired machine) and `--allow NAME` (implies `--peers-only`).
- **security:** both sides of a code pair from relay-only endpoints (no UDP
  socket). A third party that derives the rendezvous ID from words 1-3 and
  connects learns none of the creator's IP addresses, and someone who squats the
  rendezvous ID learns none of the joiner's: only its temporary endpoint ID and
  home relay. n0's DNS record already held the relay URL only. The joiner
  announces the endpoint it transfers from inside the authenticated channel (the
  `payload` frame, covered by both `done` MACs) and the sender or chat creator
  serves only that endpoint. The transfer endpoints keep direct paths, reached
  only after authentication.
- **security:** the configuration directory is private (0700), lock files 0600,
  and the identity file must belong to you and be unreadable by others, with a
  `chmod 600` hint; on filesystems without POSIX modes a warning is printed and
  only the owner is checked. Remote text is escaped before reaching the
  terminal. Slot handshakes resist floods (address validation under load, no
  eviction after `hello`, deadline `max(2 s, 4 × RTT)` capped at 5 s, retries
  within `--timeout`), and a refusal before the server proof is one generic
  `unknown slot or protocol`.
- **network:** iroh's public relays are used by default. `--relay URL` selects
  your own relay, with an access token sent only to that relay; `--relay-only`
  and `--direct-only` force a single path.
- **cli:** a machine joining with a code says after 5 seconds that it is still
  waiting for the code's creator, and how long it will wait. `--help` lists
  global options under their own heading, and file errors name the path.
- **cli:** servers print the complete command to run on the other machine;
  `-v`/`-vv` show more connection detail and `--version` the build commit,
  platform and iroh versions. `ticket inspect` decodes ping and bench tickets offline, and
  settings can come from a TOML file or environment variables.
- **install:** `curl -fsSL …/install.sh | sh` (Linux) or
  `irm …/install.ps1 | iex` (Windows) installs or updates the latest release over
  HTTPS after checking its SHA-256. Archives for Linux x86_64 and Windows x86_64
  include an offline quick start and build provenance.

### Known limitations

- No prebuilt macOS archive: build from source. CI does not test macOS yet.
- Pairing and codes need a relay (public by default, or your own) once. Without a
  relay, tunnels and pipes between paired machines need `--direct-only --port`
  on the server and `--addr` on the client. There is no local network discovery.
- `ping` and `bench` servers are open by default: anyone holding the ticket or
  the slot address is served, a removed peer included, and `peers rm` does not
  stop them. Only `--peers-only` / `--allow` restrict them to paired machines, or
  `id rotate --address` cuts a former peer. They only measure the connection.
  Relay tokens control relay access, not access to your services.
- `pair` needs an interactive terminal on both machines.
- Connections already open survive `peers rm`; restart the server to cut them.
- A chat code admits whoever uses it first, even a machine removed with
  `peers rm`, while it is valid. Use `chat serve --allow NAME`, which creates
  no code, or stop the server to void its code.
- One `chat serve` runs per slot and machine; a second one needs `--slot`.
- Peer books written by development builds with per-slot access are refused:
  delete `peers.toml` and pair again.
- A former or lost peer knows the addresses of your slots and can disrupt them
  until you run `id rotate --address`; the server warns when that happens. After
  the rotation every peer must pair again, and nobody is notified.
- The identity file in the configuration directory is a secret: whoever copies it
  acts as this machine. After a leak, run `id rotate`, remove the machine with
  `peers rm` on each paired machine and pair again.
- A paired machine learns this machine's public key and can tell that several
  slots belong to the same machine until `id rotate --address`.
- The `spake2` crate has no independent audit.
- Codes passed as `code:WORDS` are visible in the process list and may stay in
  shell history; the `Code:` prompt avoids both. On Windows the prompt keeps the
  echo on, so the code stays visible on screen.
- File transfers need temporary disk space and a destination filesystem with hard
  links (not FAT/exFAT). Empty directories, permissions and timestamps are not
  preserved, and a folder is not published atomically as a whole. Chat receptions
  do not resume. There is no delivery to offline machines: an inbox or a share
  must be running.
- Standalone sendme and dumbpipe binaries cannot exchange data with this tool.
