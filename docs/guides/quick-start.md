# Get your first result

Install `iroh-toolbox` on **A and B** using the [installation guide](installation.md),
then check `iroh-toolbox --version` on each. The commands below work in POSIX
shells and PowerShell. Keep the serving command running until the receiving
command finishes. You can first try two terminals with separate working
directories on one machine; that does not validate routing between machines.

No custom relay or token is required. Both peers use iroh's public relays by
default and can establish a direct connection when possible. Run
`iroh-toolbox relay check` to verify public-relay registration. You can optionally
[select the same custom relay and its token on both machines](private-relay.md).
To force relayed data, add `--relay-only` before the command on both peers.
`--direct-only` disables iroh relays; codes need a relay, so it only applies
between machines that are already paired (see
[tunnels](tunnels.md#without-a-relay)). Suggested commands preserve these flags and a custom `--relay` URL, but never a relay token:
configure the other machine's own token first.

## Send a folder with a code

Create a non-empty `my-folder` (the next section includes sample commands),
then run:

```sh title="Machine A — keep this running"
iroh-toolbox send ./my-folder
```

A prints a one-use code of six words. On B, type the complete code at the prompt
(no echo, and it stays out of the process list):

```sh title="Machine B — type A's actual code at the prompt"
iroh-toolbox receive code: --output ./received
```

On B, expect `received my-folder → …; BLAKE3 verified`, with the published
path, size, file count and duration. Open a file in
`received/my-folder/` to check its content. Existing names are replaced only
after confirmation. Keep A running until B finishes; A's final message alone
does not confirm publication.
The code exchanges identities and the ticket through iroh's relays, without
any server of yours; files then travel over iroh, and only B is served. Each
machine shows a 6-digit confirmation number: read yours to the other person and
type theirs. `--no-verify` on both machines skips it, and then whoever uses the
code first is served. See the [code guide](files.md#use-a-code) for the
verification and pairing deadlines.

With the default public relays, their operator sees endpoint IDs, IP addresses
and traffic volume, never content. To avoid that, run
[your own relay](private-relay.md).

## Send a small folder

Choose a writable working directory on each machine. On A, create a folder
containing a small text file (or use a non-empty folder of your own):

=== "Linux / POSIX"

    ```sh
    mkdir -p my-folder
    printf 'hello from A\n' > my-folder/hello.txt
    ```

=== "Windows / PowerShell"

    ```powershell
    New-Item -ItemType Directory -Path my-folder
    Set-Content -Path my-folder/hello.txt -Value 'hello from A'
    ```

On A, run `iroh-toolbox send ./my-folder`. A shows preparation, its code and the
complete `receive` command to copy. On B, run it and type the code at the
`Code:` prompt:

```sh
iroh-toolbox receive code: --output ./received
```

B shows connection, transfer with BLAKE3 checking, staging and publication.
Success is the final **`received my-folder → …; BLAKE3 verified`** message on B.
Open `received/my-folder/hello.txt` and check `hello from A`. A's
`transfer finished; the receiver verifies integrity` does not confirm B's result.

If `received/my-folder` already exists, choose a new `--output` directory,
such as `./received-again`. For existing names, `receive` asks before replacing
or merging; the [file guide](files.md#replace-existing-files) describes the options.
The output parent must exist and support hard links (for example ext4 or NTFS).
If the reception is interrupted, run the same `receive` command again while A
keeps running: verified data is kept and the transfer resumes.
See [files](files.md) for temporary disk space and limits.

## Start a chat and offer a file

Run `iroh-toolbox chat serve` on A, then `iroh-toolbox chat connect code:` on B,
typing A's complete code at the prompt. The code admits a single participant:
the first one to use it. After both terminals report
`Chat ready; admission: code.`, type a message and press Enter.

In A's chat, type `/send ./my-folder`. B accepts the offer with
`/receive ./received-chat`; check the received file and B's `BLAKE3 verified`
result. Use `/quit` to leave. The [chat guide](chat.md) covers acceptance,
replacement and peer identity checks.

## Pair two machines

Tunnels and pipes work only between **paired** machines, which can also send
files and chat without a code. Pair A and B once, with a code, in two
interactive terminals:

```sh title="Machine A — the one that runs the service"
iroh-toolbox pair
```

```sh title="Machine B — type A's complete code at the prompt"
iroh-toolbox pair code:
```

Each screen shows its own 6-digit confirmation number and asks for the other
machine's. Read yours to the other person, and type the number they read to
you. Expect `Paired with NAME (machine xxxx…xxxx)` on both, with the other
machine's name and short key. Pairing asks no access question and starts no
service: each tunnel or pipe server you run later must name the machines it
serves with `--allow NAME`, or `--allow all` for every paired machine.

`iroh-toolbox peers` lists the machines paired with this one (`NAME`, `MACHINE`,
`PAIRED`), and A's `id rename` sets the name B sees (the hostname by default).
Below, A is named `nas` and B `laptop`; use the names your `peers` lists show.
See [machine pairing](../reference/peers.md).

### Protect the machine key with a passphrase

The machine key proves your machine to every peer and sits in a file on disk.
On a laptop, you can encrypt it:

```sh
iroh-toolbox id protect     # asks a passphrase twice, nothing is echoed
```

From then on, every command that uses the key (`pair`, `tunnel`, `pipe`,
`send --peers-only`, `id`…) asks `Passphrase for the machine key:` once, before
anything else (a tunnel client asks it before it announces that it is ready).
It is read from the controlling terminal (`/dev/tty`), so a pipe on standard
input is fine: `tar c . | iroh-toolbox pipe serve --allow nas` still asks on
the terminal. It cannot be given in arguments, environment variables, files or
standard input, so a protected key stops unattended servers (systemd, cron)
from starting: leave the key unprotected on those machines. `id protect` again
changes the passphrase,
`id unprotect` removes the protection. See the
[security model](../reference/security.md#the-identity-file-is-a-secret).

## Send to a paired machine

Between paired machines, files need no code. On B, open an inbox:

```sh title="Machine B — keep this running"
iroh-toolbox receive --inbox --output ./inbox
```

```sh title="Machine A"
iroh-toolbox send ./my-folder peer:laptop
```

A ends with `laptop received my-folder: BLAKE3 verified and published`; B keeps
its inbox open for further transfers until Ctrl-C. An inbox never replaces
existing files. To let B fetch instead, run `send ./my-folder --peers-only` on
A and `receive peer:nas` on B. See [files](files.md#send-to-an-inbox).

## Browse a remote folder

Prerequisite: A and B are paired. On A, serve the sample folder above; no web
server of your own is needed:

```sh title="Machine A — keep this running"
iroh-toolbox tunnel http serve ./my-folder --allow laptop
```

A prints `Serving to: laptop`, the command to copy, and
`Files: …/my-folder (read-only; dotfiles hidden; …)`. On B, use A's name from
`iroh-toolbox peers`:

```sh title="Machine B — keep this running"
iroh-toolbox tunnel http connect peer:nas
```

B prints `Open http://127.0.0.1:8080/`; A is contacted when the page loads.
Open **`http://127.0.0.1:8080/` on B**: the page lists `hello.txt`, which shows
`hello from A`. Nothing on B can write to A's folder, and names starting with a
dot stay hidden.

If port 8080 is occupied, use `--bind 127.0.0.1:8081` and open port 8081.
If `tunnel http serve` reports `no paired machine`, pair A and B first. If B is
refused, read the message: [troubleshooting](../reference/troubleshooting.md)
explains each one. To serve a static site, or to reach a web server or SSH
that already runs on A with `tunnel tcp`, see [tunnels](tunnels.md).

## Diagnose the route

On A, run `iroh-toolbox ping serve`; on B, copy the suggested `ping connect`
command with its ticket (a ping server needs no pairing). Replies show RTT, `route=direct` or `route=relay`, and the carrier.
See [diagnostics](diagnostics.md) for JSON output and benchmarks.
