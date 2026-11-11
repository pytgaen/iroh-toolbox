# Tunnels

Tunnels connect **paired machines**: pair them once with `iroh-toolbox pair`
(see [machine pairing](../reference/peers.md)). A tunnel server must name who it
serves with `--allow`. **Machine A** hosts a web server on
`127.0.0.1:3000`:

```sh
iroh-toolbox tunnel tcp serve --to 127.0.0.1:3000 --allow laptop
```

A prints `Serving to: laptop`, the machines it accepts,
and the command for B, on stderr. On
**machine B**, open a local port to that service, naming A as it appears in
B's `iroh-toolbox peers`:

```sh
iroh-toolbox tunnel tcp connect peer:nas --bind 127.0.0.1:8080
```

On B, opening `http://127.0.0.1:8080` reaches A's web server. You can also tunnel
SSH or any other TCP service; to share a folder without a web server, see
[browse a folder](#browse-a-folder). Both commands stay running and accept multiple
connections; each TCP connection gets its own iroh connection. Data flows in both
directions, with independent closure of each direction. The serving machine
handles at most 64 concurrent authenticated connections by default and refuses
further ones; change this with `tunnel tcp serve --max-connections N`.

A accepts only machines in its peer book, checked at every connection:
`peers rm` and new pairings apply without restarting the server, and
connections already open continue. `--allow NAME` (repeatable or
comma-separated) is required: it names the machines this server accepts, and
you restart the server to change the list. `--allow all` serves every machine
of the peer book, including machines paired later; it cannot be combined with
names. Without `--allow`, the server refuses to start with `name the machines
to serve: --allow NAME (repeatable), or --allow all`. It also refuses to start
without any paired machine, or with an `--allow` name that `iroh-toolbox peers`
does not list. Tickets and `id:`
addresses are not accepted.

`--to` sets the fixed destination service on the machine serving over iroh.
`--bind` sets the local TCP address on the connecting machine. With
`--bind HOST:PORT`, B listens at once and contacts A only when a local
connection arrives, so B may start before A. Without `--bind`, B first asks A
for its services and listens on the port A announces: `127.0.0.1:8080` for a
single `--to HOST:PORT`. Use port `0` to allocate a free port, printed on
stderr. Write IPv6 addresses as `[::1]:8080`.

## Browse a folder

To share a folder without running a web server, serve it with `tunnel http`
instead of `tunnel tcp`. On A:

```sh
iroh-toolbox tunnel http serve ./my-folder --allow laptop
```

A prints `Serving to: laptop`, the command for B, then:

```text
Files: /home/me/my-folder (read-only; dotfiles hidden; HTML, SVG and XML open without scripts)
```

On B:

```sh
iroh-toolbox tunnel http connect peer:nas
```

B prints `Open http://127.0.0.1:8080/ (nas is contacted when the page loads)`.
The page lists the folder, subfolders first, with sizes and dates. Files open
in the browser or download, and videos can seek. A answers HTTP itself over the
tunnel: no TCP port opens on A, and only `GET` and `HEAD` are answered, so
nothing can change the folder.

- Names starting with a dot (`.git`, `.env`) are neither listed nor served;
  `--hidden` serves them.
- Paths stay inside the folder: `..` and links that lead outside it are not
  found.
- HTML, SVG and XML files open without running their scripts. B's browser runs
  pages on its own `127.0.0.1`, whose cookies every local port shares: a page
  from the folder could otherwise act on B's other local web services.
- The default slot is `http`, so a `tunnel tcp` server can run beside it.
  `--slot`, `--allow`, `--max-connections`, `--port` and `--direct-only` work as
  for `tunnel tcp`. On B, `--bind` sets the local address; port `0` allocates a
  free one and the printed URL shows it.

A path that is not a folder is refused. A web server that already runs on A is
reached with `tunnel tcp serve --to 127.0.0.1:3000`; `tunnel http serve
127.0.0.1:3000` says so.

## Serve a static site

`--site` serves the folder as a website: a folder opens its `index.html`, a
missing page gets the site's `404.html` (or a plain 404), no listing is shown,
and pages run their scripts. To preview a documentation site built on A:

```sh
uvx zensical build
iroh-toolbox tunnel http serve ./site --site --allow laptop
```

A prints:

```text
Site: /home/me/project/site (read-only; dotfiles hidden; its pages run their scripts in the visitor's browser)
```

B connects with `tunnel http connect` as above. Serve with `--site` only a site
whose scripts B may trust: they run in B's browser on `127.0.0.1`.

## Several tunnels with slots

A tunnel runs on a **slot**, named `tunnel` by default and kept across restarts.
Give each server its own slot name to run several at once on A, each in its own
process and with its own filter:

```sh
iroh-toolbox tunnel tcp serve --to 127.0.0.1:22 --slot ssh --allow laptop
```

A prints `Serving slot ssh to: laptop`. Another paired machine is told
`this nas server (slot ssh) does not allow this machine (--allow)`.

On B, add the slot after the name:

```sh
iroh-toolbox tunnel tcp connect peer:nas/ssh --bind 127.0.0.1:2222
```

Slots need a relay to be found (public by default, or `--relay`), or a direct
address: see [without a relay](#without-a-relay). See
[machine identity and slots](../reference/slots.md) for the handshake and
security model.

## Without a relay

Once the machines are paired, a tunnel can run with no relay at all. Fix the
UDP port on A and give B the address A prints:

```sh
# Machine A
iroh-toolbox tunnel tcp serve --to 127.0.0.1:22 --slot ssh --allow laptop --direct-only --port 7777
# Serving slot ssh to: laptop
# Direct addresses: 192.168.1.10:7777

# Machine B
iroh-toolbox tunnel tcp connect peer:nas/ssh --direct-only --addr 192.168.1.10:7777 --bind 127.0.0.1:2222
```

`--addr` is repeatable. Without `--addr`, `--direct-only` fails with `the server
cannot be found without a relay; pass --addr HOST:PORT printed by the server`.
`--port` also helps with a relay when a firewall rule needs a fixed port.
Pairing itself needs a relay once.

## Several services on one slot

On A, repeat `--to` and name each service with `SERVICE=HOST:PORT`:

```sh
iroh-toolbox tunnel tcp serve --slot dev --allow laptop \
  --to web=127.0.0.1:3000 \
  --to db=127.0.0.1:5432
```

A prints each service with the client port it announces, then the command for
B:

```text
TCP service web -> 127.0.0.1:3000 (client port 3000)
TCP service db -> 127.0.0.1:5432 (client port 5432)
```

On B, connect to the slot to open the whole group on the announced ports:

```console
$ iroh-toolbox tunnel tcp connect peer:nas/dev
TCP listening on 127.0.0.1:3000 -> service web
TCP listening on 127.0.0.1:5432 -> service db
tunnel: 2 services of nas/dev ready; destinations are checked at each local connection
```

Move one service with `--bind SERVICE=HOST:PORT` (repeatable); the others keep
their announced port:

```sh
iroh-toolbox tunnel tcp connect peer:nas/dev --bind db=127.0.0.1:15432
```

To open a single service, name it after the slot. A service of the default slot
is written with the slot: `peer:nas/tunnel/web`.

```sh
iroh-toolbox tunnel tcp connect peer:nas/dev/web
iroh-toolbox tunnel tcp connect peer:nas/dev/web --bind 127.0.0.1:8080
```

A service name uses 1 to 32 lowercase letters, digits or hyphens. Without a
name, a service is named after its destination port: `--to 127.0.0.1:3000` is
service `3000`. A numeric name is also the announced client port, which lets
two hosts share a destination port:

```sh
iroh-toolbox tunnel tcp serve --allow laptop \
  --to 8001=192.168.1.20:80 \
  --to 8002=192.168.1.21:80
```

Names and announced ports must be distinct within a slot; a slot serves at most
64 services. A sole unnamed `--to HOST:PORT` announces port `8080` instead of
its own, because such a service often uses a port B cannot or should not bind
(22, 80).

B contacts A at startup only when it needs the service list: to open the group,
to apply `--bind SERVICE=HOST:PORT`, or to find the port of a named service.
`peer:nas/dev/web --bind HOST:PORT` and `peer:nas --bind HOST:PORT` start
without A. An unknown service then appears at the first local connection, as
does a slot with several services when the target names none:

```text
tunnel error: nas/dev serves several services (web, db); name one with peer:nas/dev/SERVICE or --bind SERVICE=HOST:PORT
```

Every listener binds before any readiness is printed; an occupied port releases
the listeners already opened and names its service. The service list is read
once: restart B to see services added on A. A never sends destination hosts or
ports, only service names and announced ports, and only after the slot
handshake. A paired machine cannot supply its own destination. An unavailable
destination affects that connection only; other services remain available.

Service selection travels over the slot's connection; see
[compatibility](../reference/compatibility.md) for the protocol.

## Recognize the current state

1. `TCP listening on …` on B confirms its local listener. With
   `--bind HOST:PORT`, the next message says the peer and service are unchecked
   until a local connection. Otherwise B has already listed A's services, and
   the message counts them.
2. Opening a page starts the connection to A. B reports when the peer connects
   and requests forwarding; this alone does not confirm service availability.
3. A reports `destination TCP service connected` only after its TCP connection
   to `--to` succeeds. This confirms TCP access, not a successful HTTP response.
4. The expected page in B's browser confirms the application works through the
   tunnel. Service authentication remains the application's responsibility.

An unavailable destination closes that connection with a reason visible to B.
Start the service on A and check `--to`, then try a new browser request; the
tunnel stays running. If B is refused, the message says what the handshake
established:

| B reports | Meaning |
| --- | --- |
| `nas does not know this machine` | A proved its identity, but B is not in A's peer book (never paired, or removed) |
| `does not allow this machine (--allow)` | B is paired, but outside this server's `--allow` list |
| `cannot authenticate nas` | B could not verify A's identity: its response does not verify against the key stored when pairing, or it refused before any proof (its reason is then untrusted) |
| `cannot establish an authenticated connection to slot tunnel on nas` | No authenticated answer: check that A runs the server, the network, `--addr` and the slot name |

See [troubleshooting](../reference/troubleshooting.md) for every message;
for a connection error, keep A's tunnel running and check relay configuration
on both machines. If a local port is
occupied, choose `--bind 127.0.0.1:8081`.

The serving command prints the paired machines it serves, then a complete
remote command to copy. `--no-hints` suppresses that suggestion.

**`pipe` carries stdin/stdout; `tunnel tcp` carries connections to a port.**
Applications using the tunnel do not need to handle shell redirections.

## SSH example

On the machine running an SSH server, admit only `laptop`:

```sh
iroh-toolbox tunnel tcp serve --to 127.0.0.1:22 --slot ssh --allow laptop
```

On the connecting machine (`laptop`):

```sh
iroh-toolbox tunnel tcp connect peer:nas/ssh --bind 127.0.0.1:2222
```

In another terminal on the connecting machine:

```sh
ssh -p 2222 user@127.0.0.1
```

Keep SSH authentication enabled and verify the remote host key. A paired
machine admitted by this server reaches SSH as a local client would; the
toolbox forwards TCP bytes and the service remains responsible for
authenticating users.
See the [security model](../reference/security.md) before exposing a service.
