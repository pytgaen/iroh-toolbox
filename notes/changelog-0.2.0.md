## [0.2.0] — YYYY-MM-DD

### Features

- **pairing:** explain why a code finds nobody. A timeout now tells to compare
  the `Commit` line of `iroh-toolbox --version` on both machines and ends with
  the last failed attempt; with `-v`, both machines show the rendezvous endpoint
  and the joining one shows why each attempt failed. The creating machine warns
  when its code's address is not readable on `dns.iroh.link` after 10 seconds.
- **chat:** keep the lines you type across chats. With `--history`, or
  `IROH_TOOLBOX_CHAT_HISTORY=on` as a lasting default, ↑ recalls the last 100
  lines typed in earlier chats; `--no-history` overrides the variable. They go
  to `chat_history` in the configuration directory, mode 0600: your own lines
  and commands only, never the other participant's. As in shells, a line typed
  after a space is not kept. Off by default: chat still writes nothing to disk
  unless asked.
- **chat:** `/stats` shows what the chat has exchanged since it started, file
  transfers and reconnections included: bytes on the wire, split between the
  direct path and the relay, the current route with its round-trip time,
  packet loss and route changes. The chat also tells when its connection
  switches route, as in `Route: now direct (UDP), 1.8 s after connecting`.
- **stats:** `--stats` and `--stats-json` on `send`, `receive`, `chat`, `pipe`,
  `tunnel`, `ping` and `bench` report the same traffic on stderr when the
  command ends, or at Ctrl+C. Servers and tunnels add a line per closed
  connection. Only the connections that carry the command's data count, from
  their start: neither pairing nor refused connections.
- **chat:** `--auto-cmd` answers file offers from a form instead of typing
  `/receive`: a destination field (`.` by default), a `--force` box (unticked)
  and `accept`/`refuse` buttons. Messages keep arriving above the form, the line
  being typed comes back after it, and Esc leaves the offer pending. It needs an
  interactive terminal.

### Bug Fixes

- **chat:** a line typed after a space is sent without the leading spaces; it
  was silently dropped.

### Documentation

- **troubleshooting:** add "Both machines wait, then the pairing times out".

### Dependencies

- Compatible updates of `anyhow`, `clap`, `hyper`, `sha2`, `toml`, `url`,
  `walkdir` and `zeroize`; `url` 2.5.9 fixes RUSTSEC-2026-0592.
- Deferred: `iroh` 1.5.0 and `iroh-blobs` 0.105.0. Both are pinned exactly and
  move together with the reference `iroh-relay`, in their own change.
- `chrono` is reported unmaintained (RUSTSEC-2026-0543), with no known flaw;
  it stays until its replacement with the `iroh` 1.5 update.
