# Binary streams

Pipes connect stdin/stdout of two **paired** machines (see
[machine pairing](../reference/peers.md)). A pipe server must name who it
serves with `--allow NAME` (or `--allow all` for every paired machine); its
default slot is `pipe`.

On **machine A**, receive into a file:

```sh
iroh-toolbox pipe serve --allow laptop --recv-only > received.bin
```

A prints `Serving to: laptop`, the machines it accepts, and
a complete `pipe connect` command, on **stderr**; stdout contains only payload
data. Use global `--no-hints` to suppress the command suggestion, and never
merge stderr into stdout. A refuses to start without any paired machine. On
**machine B**, name A as it appears in `iroh-toolbox peers`:

```sh
iroh-toolbox pipe connect peer:nas < file.bin
```

Use another slot name to run several pipes, each with its own filter:

```sh
# Machine A: only laptop may connect to this pipe
tar c data | iroh-toolbox pipe serve --slot backup --allow laptop
# Machine B (laptop)
iroh-toolbox pipe connect peer:nas/backup --recv-only | tar x
```

`--allow NAME` is required, repeatable or comma-separated, and applies to this
server only; restart it to change the list. `--allow all` serves every paired
machine, including those paired later, and cannot be combined with names.
Without `--allow` the server refuses to start. Another paired machine is told
`this nas server (slot backup) does not allow this machine (--allow)`.

To send in the other direction, give the file to the server and put
`--recv-only` on the receiving client:

```sh
# Machine A
iroh-toolbox pipe serve --allow laptop < file.bin
# Machine B
iroh-toolbox pipe connect peer:nas --recv-only > received.bin
```

Put `--recv-only` on whichever side only receives: its outgoing direction closes
at once instead of waiting for terminal input. Without `--recv-only`, streams
are bidirectional. A local EOF closes only the outgoing direction; receiving
continues until the remote EOF.

Each server serves one client and then exits. Failed attempts (another command,
a wrong slot, an unpaired machine, a broken handshake) do not stop it; with `-v` it reports each
ignored connection. A client arriving during the exchange is told that the
server is already serving another client. The same applies to `bench serve`.

## Windows binary redirection

The examples above use a POSIX shell. On Windows, use **Command Prompt** for
binary file redirection; PowerShell does not support the `< file.bin` syntax.
With relay environment variables set in PowerShell, you can invoke `cmd`:

```powershell
cmd /c 'iroh-toolbox pipe serve --allow laptop --recv-only > received.bin'
```

On the other peer, replace `nas` with the paired machine's name:

```powershell
cmd /c 'iroh-toolbox pipe connect peer:nas < file.bin'
```

The child process inherits your environment variables. Keep diagnostic output
on stderr and avoid merging it into stdout when carrying binary data.
