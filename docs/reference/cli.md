# CLI reference

Generated from the native CLI's `--help` output. Global options are shown
first; place them before the command in portable examples. See
[configuration](configuration.md) for environment variables and precedence.

Use `iroh-toolbox --version` for the build commit, target platform, project
license, and locked versions of directly used iroh components and of the
SPAKE2 implementation used by code pairing.
`iroh-toolbox -V` prints only the application name and version.

## iroh-toolbox

```text
Transfer files, chat, stream data and reach remote TCP services; diagnose connections

Usage: iroh-toolbox [OPTIONS] <COMMAND>

Commands:
  send     Send a file or directory: print a one-use code, or push it to a paired machine's inbox
  receive  Receive files with the code printed by `send`, or as an inbox for paired machines; verify BLAKE3 integrity
  chat     Open a private two-person chat with /send and /receive: `chat serve` opens it, `chat connect` joins
  pipe     Connect stdin/stdout to a peer
  tunnel   Forward TCP services, or serve a folder over HTTP, through iroh
  relay    Verify registration with a public relay or the selected custom relay
  ping     Measure round trips between peers
  bench    Measure application throughput
  ticket   Inspect endpoint or blob tickets offline
  pair     Pair with another machine once, with a one-use code: no code creates one, code:WORDS joins
  id       Show this machine's name, short key, protection and paired machines; rename, rotate or protect it
  peers    List and manage the machines paired with this one
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## send

```text
Send a file or directory: print a one-use code, or push it to a paired machine's inbox

Usage: iroh-toolbox send [OPTIONS] <PATH> [TARGET]

Arguments:
  <PATH>    File or directory to send
  [TARGET]  peer:NAME[/SLOT] of a paired machine running `receive --inbox` (default slot: inbox); omit to print a one-use code

Options:
      --addr <HOST:PORT>
          Direct address HOST:PORT of the inbox, as printed by it; repeat for several. Required with --direct-only
      --peers-only
          Offer the files to paired machines, which fetch them with `receive peer:NAME`; no code. Ends after one reception
      --share
          With --peers-only or --allow: keep serving the same files to every receiver until Ctrl-C
      --allow <NAME>
          Offer the files to these paired machines only (repeatable or comma-separated), or `all`; implies --peers-only
      --slot [<NAME>]
          With --peers-only or --allow: slot to serve (default: send); receivers use peer:NAME/SLOT
      --port <PORT>
          With --peers-only or --allow: fix the UDP port of the server endpoint (firewall rules, --direct-only)
      --code-words <CODE_WORDS>
          Number of random words from the EFF word list in the code (default: 6)
      --pairing-timeout <PAIRING_TIMEOUT>
          Total pairing deadline, including human confirmation (default: 300 seconds, 120 with --no-verify)
      --no-verify
          Skip the 6-digit confirmation number: whoever uses the code first is served. Needed on both machines
      --transfer-timeout <TRANSFER_TIMEOUT>
          Deadline in seconds for the receiver to connect and finish once paired [default: 3600]
      --no-progress
          Hide periodic progress and transfer phases on stderr
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## receive

```text
Receive files with the code printed by `send`, or as an inbox for paired machines; verify BLAKE3 integrity

Usage: iroh-toolbox receive [OPTIONS] [TARGET]

Arguments:
  [TARGET]  code:WORDS printed by `send` (`code:` or nothing asks for the words on the terminal), or peer:NAME[/SLOT] of a paired machine running `send PATH --peers-only` (default slot: send)

Options:
      --addr <HOST:PORT>
          With peer:NAME: direct address HOST:PORT of the sender, as printed by it; repeat for several. Required with --direct-only
      --inbox
          Receive what paired machines push with `send PATH peer:NAME`, one transfer at a time, until Ctrl-C; never replaces existing files
      --once
          With --inbox: exit after one successful reception
      --allow <NAME>
          With --inbox: accept only these paired machines (repeatable or comma-separated), or `all`
      --slot [<NAME>]
          With --inbox: slot to serve (default: inbox); senders use peer:NAME/SLOT
      --port <PORT>
          With --inbox: fix the UDP port of the server endpoint (firewall rules, --direct-only)
      --pairing-timeout <PAIRING_TIMEOUT>
          Total pairing deadline, including human confirmation (default: 300 seconds, 120 with --no-verify)
      --no-verify
          Skip the 6-digit confirmation number: whoever uses the code first is served. Needed on both machines
      --output <OUTPUT>
          Destination directory; received files and folders are published inside it [default: .]
      --max-bytes <MAX_BYTES>
          Payload limit: bytes or integer B, kB, MB, GB, TB, KiB, MiB, GiB, TiB (default: 10GiB, 1GiB per offer with --inbox)
      --transfer-timeout <TRANSFER_TIMEOUT>
          Deadline in seconds for each transfer; an interrupted one resumes with the same command [default: 3600]
      --no-progress
          Hide periodic progress and transfer phases on stderr
      --hidden
          Accept top-level names starting with a dot, such as .bashrc or .ssh/
      --force
          Replace existing regular files without asking; preserve unrelated files in existing directories
      --no-replace
          Refuse an existing destination instead of asking before replacing or merging
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## chat

```text
Open a private two-person chat with /send and /receive: `chat serve` opens it, `chat connect` joins

Usage: iroh-toolbox chat [OPTIONS]
       iroh-toolbox chat <COMMAND>

Commands:
  serve    Open a chat and print a one-use code: the first machine that uses it joins
  connect  Join a chat with the code printed by `chat serve`
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## chat serve

```text
Open a chat and print a one-use code: the first machine that uses it joins

Usage: iroh-toolbox chat serve [OPTIONS]

Options:
      --code-words <CODE_WORDS>
          Number of random words from the EFF word list in the code (default: 6)
      --peers-only
          Open the chat to paired machines only: no code, no relay needed
      --allow <NAME>
          Admit only these paired machines (repeatable or comma-separated), or `all` for every paired machine; implies --peers-only: no code
      --slot [<NAME>]
          Slot that paired machines join with peer:NAME/SLOT (default: chat)
      --port <PORT>
          Fix the UDP port of the slot endpoint (firewall rules, --direct-only with --peers-only or --allow)
      --pairing-timeout <PAIRING_TIMEOUT>
          Total deadline in seconds for pairing with the code (default: 300, 120 with --no-verify)
      --no-verify
          Skip the 6-digit confirmation number when pairing with the code: whoever uses the code first is served. Needed on both machines
      --max-bytes <MAX_BYTES>
          Maximum received payload per offer; accepts sizes such as 10GiB [default: 10GiB]
      --transfer-timeout <TRANSFER_TIMEOUT>
          Preparation/transfer deadline in seconds, per file offer [default: 3600]
      --no-progress
          Hide file preparation and transfer progress on stderr
      --timestamps <WHEN>
          When messages show their time; default from IROH_TOOLBOX_TIMESTAMPS, else auto [possible values: auto, always, never]
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## chat connect

```text
Join a chat with the code printed by `chat serve`

Usage: iroh-toolbox chat connect [OPTIONS] <TARGET>

Arguments:
  <TARGET>  code:WORDS printed by `chat serve` (`code:` asks for the words on the terminal), or peer:NAME[/SLOT] of a paired machine running `chat serve` (default slot: chat)

Options:
      --addr <HOST:PORT>
          With peer:NAME: direct address HOST:PORT of the chat, as printed by it; repeat for several. Required with --direct-only
      --pairing-timeout <PAIRING_TIMEOUT>
          Total deadline in seconds for pairing with the code (default: 300, 120 with --no-verify)
      --no-verify
          Skip the 6-digit confirmation number when pairing with the code: whoever uses the code first is served. Needed on both machines
      --max-bytes <MAX_BYTES>
          Maximum received payload per offer; accepts sizes such as 10GiB [default: 10GiB]
      --transfer-timeout <TRANSFER_TIMEOUT>
          Preparation/transfer deadline in seconds, per file offer [default: 3600]
      --no-progress
          Hide file preparation and transfer progress on stderr
      --timestamps <WHEN>
          When messages show their time; default from IROH_TOOLBOX_TIMESTAMPS, else auto [possible values: auto, always, never]
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## pipe serve

```text
Wait for a paired machine and connect it to stdin/stdout

Usage: iroh-toolbox pipe serve [OPTIONS]

Options:
      --recv-only      Only receive: close the outgoing direction at once instead of reading stdin
      --slot [<NAME>]  Slot to serve; paired machines reach it with peer:NAME/SLOT [default: pipe]
      --allow <NAME>   Paired machines to serve (repeatable or comma-separated), or `all` for every paired machine. Required
      --port <PORT>    Fix the UDP port of the server endpoint (firewall rules, --direct-only)
  -h, --help           Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## pipe connect

```text
Connect stdin/stdout to a paired machine running `pipe serve`

Usage: iroh-toolbox pipe connect [OPTIONS] <TARGET>

Arguments:
  <TARGET>  peer:NAME[/SLOT] of a paired machine (default slot: pipe)

Options:
      --addr <HOST:PORT>  Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only
      --recv-only         Only receive: close the outgoing direction at once instead of reading stdin
  -h, --help              Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## tunnel tcp serve

```text
Forward connections of paired machines to local TCP services

Usage: iroh-toolbox tunnel tcp serve [OPTIONS] --to <TO>

Options:
      --to <TO>
          Destination HOST:PORT or SERVICE=HOST:PORT; repeat to share several services
      --max-connections <MAX_CONNECTIONS>
          Concurrent peer connections served; further connections are refused [default: 64]
      --slot [<NAME>]
          Slot to serve; paired machines reach it with peer:NAME/SLOT [default: tunnel]
      --allow <NAME>
          Paired machines to serve (repeatable or comma-separated), or `all` for every paired machine. Required
      --port <PORT>
          Fix the UDP port of the server endpoint (firewall rules, --direct-only)
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## tunnel tcp connect

```text
Reach a paired machine's TCP services through local ports

Usage: iroh-toolbox tunnel tcp connect [OPTIONS] <TARGET>

Arguments:
  <TARGET>  peer:NAME[/SLOT[/SERVICE]] of a paired machine (default slot: tunnel); without SERVICE, every service of the slot

Options:
      --addr <HOST:PORT>            Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only
      --bind <[SERVICE=]HOST:PORT>  Local HOST:PORT for the slot's sole or selected service, or SERVICE=HOST:PORT to move one service of the group (repeatable). Without it, each service listens on 127.0.0.1 at the port its server announces
  -h, --help                        Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## tunnel http serve

```text
Serve a folder, read-only, to paired machines

Usage: iroh-toolbox tunnel http serve [OPTIONS] <PATH>

Arguments:
  <PATH>  Folder to serve, read-only; to forward a running web server, use `tunnel tcp serve`

Options:
      --site
          Serve a static site: folders open their index.html, pages run their scripts, 404.html answers missing pages
      --hidden
          Also serve files and folders whose name starts with a dot
      --max-connections <MAX_CONNECTIONS>
          Concurrent peer connections served; further connections are refused [default: 64]
      --slot [<NAME>]
          Slot to serve; paired machines reach it with peer:NAME/SLOT [default: http]
      --allow <NAME>
          Paired machines to serve (repeatable or comma-separated), or `all` for every paired machine. Required
      --port <PORT>
          Fix the UDP port of the server endpoint (firewall rules, --direct-only)
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## tunnel http connect

```text
Open a paired machine's folder in your browser through a local address

Usage: iroh-toolbox tunnel http connect [OPTIONS] <TARGET>

Arguments:
  <TARGET>  peer:NAME[/SLOT] of a paired machine (default slot: http)

Options:
      --addr <HOST:PORT>  Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only
      --bind <BIND>       Local HOST:PORT that the browser opens [default: 127.0.0.1:8080]
  -h, --help              Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## relay check

```text
Register with the relay and report the result, including authentication errors

Usage: iroh-toolbox relay check [OPTIONS]

Options:
      --json  Print the result as JSON
  -h, --help  Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## ping serve

```text
Answer pings: print a ticket, or serve a slot of paired machines

Usage: iroh-toolbox ping serve [OPTIONS]

Options:
      --max-connections <MAX_CONNECTIONS>
          Concurrent clients served; further connections are refused [default: 64]
      --slot [<NAME>]
          Serve a slot that paired machines reach with peer:NAME/SLOT, with no ticket; default name: the command
      --peers-only
          Serve only paired machines (any in the peer book; measurements only). Implies a slot
      --allow <NAME>
          With --peers-only, narrow to these paired machines (repeatable or comma-separated), or `all`; implies --peers-only
      --port <PORT>
          Fix the UDP port of the server endpoint (firewall rules, --direct-only)
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## ping connect

```text
Ping a server and report round trips and the connection path

Usage: iroh-toolbox ping connect [OPTIONS] <TARGET>

Arguments:
  <TARGET>  Ticket printed by the server, or peer:NAME[/SLOT] (default slot: ping)

Options:
      --addr <HOST:PORT>     Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only
      --count <COUNT>        Number of pings [default: 5]
      --interval <INTERVAL>  Seconds between pings [default: 1]
      --json                 Print the results as JSON
  -h, --help                 Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## bench serve

```text
Answer throughput measurements: print a ticket, or serve a slot of paired machines

Usage: iroh-toolbox bench serve [OPTIONS]

Options:
      --max-bytes <MAX_BYTES>
          Per-direction limit: bytes or integer B, kB, MB, GB, TB, KiB, MiB, GiB, TiB [default: 1GiB]
      --transfer-timeout <TRANSFER_TIMEOUT>
          Deadline in seconds for each measurement [default: 60]
      --no-progress
          Hide periodic progress on stderr
      --slot [<NAME>]
          Serve a slot that paired machines reach with peer:NAME/SLOT, with no ticket; default name: the command
      --peers-only
          Serve only paired machines (any in the peer book; measurements only). Implies a slot
      --allow <NAME>
          With --peers-only, narrow to these paired machines (repeatable or comma-separated), or `all`; implies --peers-only
      --port <PORT>
          Fix the UDP port of the server endpoint (firewall rules, --direct-only)
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## bench connect

```text
Measure upload and download throughput with a server

Usage: iroh-toolbox bench connect [OPTIONS] <TARGET>

Arguments:
  <TARGET>  Ticket printed by the server, or peer:NAME[/SLOT] (default slot: bench)

Options:
      --addr <HOST:PORT>
          Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only
      --bytes <SIZE>
          Per-direction size: bytes or integer B, kB, MB, GB, TB, KiB, MiB, GiB, TiB [default: 64MiB]
      --direction <DIRECTION>
          Direction to measure [default: both] [possible values: upload, download, both]
      --transfer-timeout <TRANSFER_TIMEOUT>
          Deadline in seconds for each direction [default: 60]
      --json
          Print the results as JSON
      --no-progress
          Hide periodic progress on stderr
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## ticket inspect

```text
Decode a ticket printed by `ping` or `bench` without connecting

Usage: iroh-toolbox ticket inspect [OPTIONS] <VALUE>

Arguments:
  <VALUE>  The ticket to decode

Options:
      --json  Print the result as JSON
  -h, --help  Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## pair

```text
Pair with another machine once, with a one-use code: no code creates one, code:WORDS joins

Usage: iroh-toolbox pair [OPTIONS] [TARGET]

Arguments:
  [TARGET]  code:WORDS to join the other machine's code (`code:` asks for the words on the terminal); omit to create a code

Options:
      --as <NAME>
          Name announced to the other machine for this pairing only (default: the name from `id`)
      --name <ALIAS>
          Local name for the other machine (default: the name it announces)
      --code-words <CODE_WORDS>
          Number of random words from the EFF word list when creating a code (default: 6)
      --pairing-timeout <PAIRING_TIMEOUT>
          Total pairing deadline in seconds, including both confirmations (default: 300)
  -h, --help
          Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## id

```text
Show this machine's name, short key, protection and paired machines; rename, rotate or protect it

Usage: iroh-toolbox id [OPTIONS]
       iroh-toolbox id <COMMAND>

Commands:
  rename     Set the name announced when pairing (default: the hostname)
  protect    Encrypt the machine key with a passphrase typed twice (on a protected key: change the passphrase)
  unprotect  Write the machine key back unprotected, after asking its passphrase
  rotate     Replace the identity: paired machines must be paired again; a protected key stays protected
  help       Print this message or the help of the given subcommand(s)

Options:
      --json  Print the result as JSON: name, machine, protected, peers
  -h, --help  Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## id rotate

```text
Replace the identity: paired machines must be paired again; a protected key stays protected

Usage: iroh-toolbox id rotate [OPTIONS]

Options:
      --address  Change only the address salt and keep the machine key (for a removed or lost machine)
  -h, --help     Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## peers

```text
List and manage the machines paired with this one

Usage: iroh-toolbox peers [OPTIONS]
       iroh-toolbox peers <COMMAND>

Commands:
  rename  Change the local name of a paired machine
  rm      Remove a paired machine; its next peer connection here is refused (open sessions and chat codes are unaffected)
  help    Print this message or the help of the given subcommand(s)

Options:
      --json  Print the list as JSON: {"peers": [{name, machine, announced, paired}]}
  -h, --help  Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## peers rename

```text
Change the local name of a paired machine

Usage: iroh-toolbox peers rename [OPTIONS] <OLD> <NEW>

Arguments:
  <OLD>  Current local name
  <NEW>  New local name: 1 to 32 lowercase letters, digits or hyphens

Options:
  -h, --help  Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```

## peers rm

```text
Remove a paired machine; its next peer connection here is refused (open sessions and chat codes are unaffected)

Usage: iroh-toolbox peers rm [OPTIONS] <NAME>

Arguments:
  <NAME>  Local name of the machine to remove

Options:
  -h, --help  Print help

Global options:
      --config <CONFIG>    TOML configuration file (or IROH_TOOLBOX_CONFIG)
      --relay <RELAY>      Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY)
      --timeout <TIMEOUT>  Connection timeout in seconds (default: 15)
      --relay-only         Disable direct UDP transports; use relays exclusively (public by default)
      --direct-only        Disable iroh relays; require reachable direct addresses in the ticket
      --no-hints           Hide copyable remote commands on stderr; ticket/data outputs are unchanged
  -v, --verbose...         More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters
```
