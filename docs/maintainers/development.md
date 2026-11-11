# Development and verification

The GitHub Actions workflow runs on demand (manual runs) and for release tags. It reads the development Rust version from `mise.toml` and the
minimum version from `Cargo.toml`. Jobs check formatting, Clippy, dependency
advisories/licenses/sources, the minimum Rust version, strict documentation
builds, the generated CLI reference, and tests on Linux and Windows.
Builds then use Zig and cargo-zigbuild for `x86_64-unknown-linux-musl` and native
MSVC for `x86_64-pc-windows-msvc`, with Cargo caching. macOS is neither tested
nor built until the project is stable or public: its runners cost ten times the
minutes of a private repository.

After checks pass, CI uploads the versioned archives and SHA-256 checksums to the
run's **Artifacts** section for 14 days. Pushing a stable `vX.Y.Z` tag runs those
same checks and publishes a GitHub Release with the archives, checksum manifest,
and notes from `CHANGELOG.md`. The tag must match `Cargo.toml` and have non-empty
changelog notes finalized in a dedicated release commit. During development,
use the target `notes/changelog-X.Y.Z.md` draft and preview it with
`python scripts/release-notes.py vX.Y.Z --draft`; the public changelog is reserved
for released versions. See the [release preparation guide](contributing.md#release-preparation).
Linux and Windows test jobs also build the pinned official
`iroh-relay 1.4.1` and run the private-relay scenario as a required separate step.

```sh
mise run test
mise run lint
mise run outdated
# Install the minimum toolchain once (currently 1.99):
rustup toolchain install 1.99 --profile minimal
mise run lint:msrv
mise run release
mise run docs:build
```

Tests exercise the compiled CLI over real local sockets: direct ping, benchmarks,
size limits, binary streams, unreachable peers, interrupted/stalled blob peers,
offline ticket inspection, configuration precedence and secret redaction. Slot
scenarios pair machines by writing both peer books and run tunnels and pipes over
direct addresses, without a relay. The same direct-only setup drives files and
chat between paired machines: `tests/support/peer_files.rs` covers inboxes
(targeted sends, `--once`, waiting senders, named slots, existing and identical
files, resumption) and `send --peers-only` (one pinned receiver, `--share` with
concurrent receivers, resumption, absent senders and option conflicts);
`tests/support/peer_chat.rs` covers `chat serve --allow` without a code,
a busy third machine, file offers in both directions, reconnection and
`peers rm` during a session. Both run with `mise run test`. Code transfers need
the local relay below.

The ordinary suite reports the private-relay scenario as **ignored**, even if
`IROH_TEST_RELAY_BINARY` is set. Build the official server, then run it explicitly:

```sh
mise run test:relay:install
IROH_TEST_RELAY_BINARY="$PWD/target/test-relay/bin/iroh-relay" mise run test:relay
```

On Windows PowerShell:

```powershell
$env:IROH_TEST_RELAY_BINARY = (Resolve-Path ./target/test-relay/bin/iroh-relay.exe).Path
mise run test:relay
```

This test creates a temporary relay with test credentials, forces relay routing
with `--relay-only`, verifies registration, ping, a folder transfer with a code
and HTTP over a TCP tunnel between two paired machines, then checks missing and
incorrect credentials. Explicit execution
fails when the binary is absent or invalid. CI runs this command separately on
all three operating systems.

For a native development archive with checksums, offline instructions and
provenance, run `mise run dist`. Then run `mise run dist:smoke` to exercise the
extracted executable outside the checkout. The smoke check starts a loopback
relay from `IROH_TEST_RELAY_BINARY`, sends a folder with a one-use code, then pairs
two machines with separate `IROH_TOOLBOX_HOME` directories (it writes both
`peers.toml`, since `pair` needs a terminal) and fetches a page through
`tunnel tcp connect peer:a/web`. CI uses the same packaging and smoke scenarios on
Linux musl and Windows MSVC before uploading artifacts; each build job
installs the pinned relay first.

```sh
mise run dist
IROH_TEST_RELAY_BINARY="$PWD/target/test-relay/bin/iroh-relay" mise run dist:smoke
mise run dist:install
```

`dist/iroh-toolbox-v0.1.0-dev-<host-target>.tar.gz` (or `.zip` on Windows) and
its `.sha256` contain the development build; `-dev` is not a release tag. The
host GNU/Linux archive is distinct from the portable musl release archive.
`dist:install` serves that archive from a local server shaped like GitHub release
downloads and installs it with `scripts/install.sh` (or `scripts/install.ps1` on
Windows): latest-release resolution, an explicit version, replacement, and a
refused tampered archive. CI runs it on every platform, with Windows PowerShell
and PowerShell 7 on Windows, after `shellcheck`. Releases publish both scripts
with the archives and list them in the checksum manifest.

To develop against a private relay, put its settings in the optional `.env`:
mise loads it and redacts its values in task output, for example with
`mise run relay-check`. Prefix ad hoc commands with
`mise exec -- cargo run --locked --`. The standalone binary never loads `.env`;
it reads environment variables or an explicitly selected TOML file.
`mise run lint:rust` provides the Rust
format/Clippy part of `lint`; `mise run docs:check` verifies the generated CLI
reference.

See [contribution guide](contributing.md) for development and release preparation.

Code pairing is part of every build. Its end-to-end scenarios run through the
same local official relay, without any public relay, DNS or mailbox:

```sh
IROH_TEST_RELAY_BINARY="$PWD/target/test-relay/bin/iroh-relay" mise run test:pairing
IROH_TEST_RELAY_BINARY="$PWD/target/test-relay/bin/iroh-relay" mise run test:slots
```

The slot scenarios restart a `--slot` tunnel and check that it keeps its address
and serves the same paired client again, refuse a second process on the same slot,
use the default slot, admit every paired machine without a grant, enforce
`--allow` per server, apply `peers rm` and new pairings without a restart,
refuse to start without peers or with an unknown `--allow` name, check that
`--peers-only` ping/bench follow the book while open ones do not, and report
wrong commands, unknown slots and other salts. Their ignored counterparts repeat
this through the local relay.
Every test process uses a temporary `IROH_TOOLBOX_HOME`, never your identity.

The ordinary suite ignores them; CI runs them in a separate Linux/Windows
step, and an explicit run without the binary fails. They cover file transfers with refusal and forced replacement, reused, wrong and
expired codes, credentials kept local with a private relay, code-paired chat
with `chat serve` and `chat connect` (messages and files in both directions,
rejection, collisions, payload limits, expiry and EOF), and the mixed chat of
`tests/support/pairing_chat.rs`: a paired machine or a code holder admitted
first, a paired machine told the chat is busy, a removed machine still joining
by code without the peer label, and a restart voiding the old code. Unix
additionally exercises the 6-digit confirmation number (acceptance, refusal,
timeout and Ctrl+C) on actual pseudo-terminals for files and chat, and runs
`pair` end to end (confirmation, typed `Code:` prompt, no access question,
pairing again, the book lock) the same way. `pair` needs interactive terminals
on both machines, so it is tested end to end only through Unix PTYs, not on
Windows. Unit tests run the pairing handshake over local sockets: matching and
wrong codes, mismatched commands or `--no-verify` settings, confirmation numbers
and rejection of invalid SPAKE2 points. Unit tests of the slot handshake cover
codes 30, 32 and 33 (an invalid signature for a paired or unknown key gets 33
before the book is read), a rotated client identity (30, not 33), forged
refusals before the server proof, an impostor holding the slot key, and remote
close text that is never shown.

The ordinary CLI suite also covers named TCP services sharing a slot (group,
override and single-service clients), rejection of unknown services, all-or-nothing local listener startup,
forced file replacement and chat confirmation while messages continue. Unit
checks inject publication errors to verify restoration of originals and retained
recovery backups when restoration is blocked. The explicit private-relay scenario
includes multiport forwarding. On Unix, also run the real-terminal checks
(Python is test infrastructure only):

```sh
mise run test:pty
```

CI runs these checks on Linux. File replacement confirmation covers acceptance,
refusal, expiry and Ctrl+C without changing existing files before confirmation;
the chat line editor test covers bursts of keys, messages above a partial line,
Tab completion, Ctrl+C and terminal restoration.

Public relays are the runtime default. Ordinary local tests use `--direct-only`
or a local private relay, and the archive smoke check uses the local relay. This
keeps the suite independent of public infrastructure.
To explicitly exercise the real public production relays, without any custom
relay configuration or token:

```sh
mise run test:public-relay
```

These tests fail when the internet or the public relays are unavailable. They
verify public registration, ping by ticket, and chat/files with codes and direct
transports disabled.

## Run the Linux CI locally with act

From a clean checkout, with Docker available:

```sh
mise exec act@0.2.89 -- act workflow_dispatch -W .github/workflows/ci.yml \
  --matrix os:ubuntu-latest --matrix target:x86_64-unknown-linux-musl \
  -P ubuntu-latest=ghcr.io/catthehacker/ubuntu:act-24.04 \
  --concurrent-jobs 1 --env-file /dev/null --secret-file /dev/null \
  --var-file /dev/null --input-file /dev/null \
  --artifact-server-path /tmp/iroh-act-artifacts
```

This runs the Linux jobs and stores uploaded artifacts locally. It excludes
the project's relay environment and secrets. Windows jobs need their native
runners.
Stable archive packaging requires a clean checkout and a binary whose embedded
commit matches it. Use a disposable checkout to validate uncommitted changes.
The pairing scenarios run serially: their short deadlines exercise timeout
handling and must not be consumed by competing test processes.
GitHub uses `upload-artifact@v7`. Local act runs use v4 for the same payload,
because act 0.2.89 has [known artifact protocol incompatibilities](https://github.com/nektos/act/issues/6114)
with v6/v7. The archive contents and smoke checks are identical in both modes.
The CI reference relay is installed outside Cargo's cached `target/` directory
so cache cleanup cannot corrupt its executable.

## Record the README demo

The animated demo in the README and on the documentation home,
`docs/assets/pair-tunnel-demo.gif`, and its transcript are generated. Re-record
them when the output of `pair` or `tunnel http` changes, and before a release:

```sh
mise run demo:record
```

This builds the Linux musl binary (`release-local`), then needs only Docker. Two
containers, `nas` and `laptop`, run on separate bridge networks that cannot
reach each other, so they meet through iroh's **public relays** and n0's DNS:
the recording needs the internet. A recorder container runs
[VHS](https://github.com/charmbracelet/vhs) and tmux, and `drive.sh` types into
each pane once the expected output appears, including the code that `nas`
prints. Each machine starts with a fresh identity in its container; your own
identity and peers are not used.

The recorder mounts the Docker socket to open a shell in each machine, which
gives it control of the Docker daemon: run it only from a trusted checkout. Base
images are pinned by digest in `scripts/demo/Dockerfile`. The transcript
records the binary, its SHA-256 and the route measured after the recording.
Review the GIF before committing it. To record another binary, run
`scripts/demo/record.sh PATH_TO_MUSL_BINARY`.
