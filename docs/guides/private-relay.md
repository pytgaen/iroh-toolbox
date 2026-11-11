# Private relays

This setup is optional: the toolbox uses public iroh relays when no custom URL
is configured. Selecting your own relay replaces the public set; a failed
private registration never sends your token to public infrastructure.

Run your own relay with the official `iroh-relay` server. **iroh-toolbox is the
client and diagnostic tool**; `iroh-toolbox relay check` does not start a relay.
If you already have a relay, skip to [Connect both clients](#connect-both-clients).

Code pairing works through this relay too: with the same `--relay` on both
machines, codes need no other service.

## What you need

- A Linux server with Docker Engine and the Docker Compose plugin (`docker compose`).
  The relay image below supports Linux amd64 and arm64.
- A domain such as `relay.example.com`, with its DNS A record pointing to the
  server's public IPv4 address. Publish an AAAA record only if IPv6 also reaches it.
- Inbound **TCP 80 and 443** allowed by the host/provider firewall and, at home,
  forwarded from the router. A connection behind CGNAT needs another way to
  accept inbound traffic, such as a VPS with a public address.
- Those ports available for Caddy. If you already run Traefik, nginx, or Caddy,
  use [your existing reverse proxy](#use-an-existing-reverse-proxy) instead.
- Outbound DNS and HTTPS for image downloads and certificate issuance, plus
  `openssl` on the host to generate a token.

The example pins `n0computer/iroh-relay:v1.4.1` and `caddy:2.11.7-alpine`.
Caddy obtains and renews the public TLS certificate. The relay accepts HTTP only
on a private Docker network; it publishes no host port. QUIC address discovery
and metrics are disabled, so **no inbound UDP port is needed for this relay**.
Clients may still use UDP for direct peer connections.

## Create the server files

On the server, use a regular user with Docker access and create a dedicated
directory outside your source checkout:

```sh
mkdir -p "$HOME/iroh-relay"
cd "$HOME/iroh-relay"
umask 077
printf 'RELAY_UID=%s\nRELAY_GID=%s\n' "$(id -u)" "$(id -g)" > .env
```

The `.env` file contains only your user and group IDs. Compose uses them to run
the relay without root and let it read your private configuration file.

Create `relay.toml` with a fresh token. Run this once; rerunning it replaces the
token and requires updating both clients:

```sh
cat > relay.toml <<EOF
http_bind_addr = "0.0.0.0:3340"
enable_quic_addr_discovery = false
enable_metrics = false

[access]
shared_token = ["$(openssl rand -hex 32)"]
EOF
chmod 600 relay.toml
```

Keep this file private and out of Git. The relay checks `access.shared_token`
during registration; no separate authentication service is required. See the
[upstream access-control documentation](https://github.com/n0-computer/iroh/blob/v1.4.1/iroh-relay/README.md#access-control).

Create `compose.yaml`:

```yaml
services:
  relay:
    image: n0computer/iroh-relay:v1.4.1
    restart: unless-stopped
    user: "${RELAY_UID:?Set RELAY_UID in .env}:${RELAY_GID:?Set RELAY_GID in .env}"
    read_only: true
    cap_drop: [ALL]
    security_opt:
      - no-new-privileges:true
    command: ["--config-path", "/etc/iroh-relay/relay.toml"]
    environment:
      RUST_LOG: info
    volumes:
      - ./relay.toml:/etc/iroh-relay/relay.toml:ro
    networks:
      - backend
    logging:
      driver: local

  caddy:
    image: caddy:2.11.7-alpine
    restart: unless-stopped
    depends_on:
      - relay
    ports:
      - "80:80/tcp"
      - "443:443/tcp"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
      - caddy_config:/config
    networks:
      - default
      - backend
    logging:
      driver: local

networks:
  backend:
    internal: true

volumes:
  caddy_data:
  caddy_config:
```

The relay runs with a read-only filesystem, no Linux capabilities, and no
privilege escalation. Its token file remains readable only by your host user.
Caddy keeps writable volumes for certificate renewal.

Create `Caddyfile`, replacing `relay.example.com` with your domain:

```caddyfile
relay.example.com {
    reverse_proxy relay:3340
}
```

Caddy [enables HTTPS for the domain](https://caddyserver.com/docs/quick-starts/reverse-proxy#https-from-client-to-proxy)
and [forwards WebSockets and the Authorization header](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy).
Keep the `/data` volume: it contains certificate and ACME account state.

## Start and check the server

From the directory containing `compose.yaml`:

```sh
docker compose config --quiet
docker compose pull
docker compose up -d
docker compose ps
docker compose logs --tail=100 relay caddy
```

Both services should stay running. On first start, allow time for Caddy to obtain
the certificate. If issuance fails, check DNS (including AAAA), port forwarding,
firewalls, and whether another service already owns ports 80/443.

From a client machine, check the anonymous probes over HTTPS:

```sh
curl --fail --silent --show-error -o /dev/null -w '%{http_code}\n' https://relay.example.com/ping
curl --fail --silent --show-error -o /dev/null -w '%{http_code}\n' https://relay.example.com/generate_204
```

Expect `200` and `204`, respectively, without a token or a TLS verification
bypass. These probes check reachability, not authenticated registration.

## Connect both clients

On **both machines**:

```sh
export IROH_TOOLBOX_RELAY="https://relay.example.com"
export IROH_TOOLBOX_TOKEN="your-shared-token"
```

Replace the URL and token with your domain and the token generated in
`relay.toml`. Transfer the token securely to each client. It is sent in the
`Authorization: Bearer …` header and is never included in endpoint tickets or
the relay URL. Do not put the token in a URL query string.

In PowerShell:

```powershell
$env:IROH_TOOLBOX_RELAY = "https://relay.example.com"
$env:IROH_TOOLBOX_TOKEN = "your-shared-token"
```
Alternatively, use a protected [client TOML file](../reference/configuration.md).
The following examples use the installed native command.

Verify registration on each machine:

```sh
iroh-toolbox relay check
```

Expect `status=online`. To verify access control, run once with an intentionally
wrong token; expect an authentication-denied error:

```sh
IROH_TOOLBOX_TOKEN=invalid-test-token iroh-toolbox relay check
```

That temporary assignment is POSIX shell syntax and leaves the configured token
unchanged for later commands. A successful HTTP `101` WebSocket upgrade alone
does not prove authentication: the relay checks access during registration.

Then test a real exchange through the relay. On **machine A**:

```sh
iroh-toolbox --relay-only ping serve
```

Copy its `endpoint…` ticket to **machine B**:

```sh
iroh-toolbox --relay-only ping connect 'endpoint…' --count 3
```

Expect replies with `route=relay` and `carrier=WebSocket/TCP`. Both commands use
`--relay-only` so a direct connection cannot mask a relay problem. See
[troubleshooting](../reference/troubleshooting.md) if registration or ping fails.
Relay access does not restrict access to individual peer services; see the
[security model](../reference/security.md).

## Use an existing reverse proxy

Keep the `relay` service and its configuration, omit the example Caddy service,
and attach the relay to a Docker network shared with your proxy. Route your
dedicated HTTPS hostname to `http://relay:3340`. If the proxy runs on the host,
publish only `127.0.0.1:3340:3340` and use that loopback address as its upstream.
Keep the backend HTTP port inaccessible from the public network.

Private reverse proxies must allow unauthenticated **GET `/ping`** and
**GET `/generate_204`**, without query strings, for iroh's HTTP probes.
**GET `/relay`** carries `Authorization: Bearer …`. The native iroh client
adds that header to its WebSocket request when a token is configured; its probes
do not carry the bearer. Enable WebSocket forwarding and preserve Authorization
on `/relay`. The relay's own `access.shared_token` validates registration. Avoid
a browser login or global Basic Auth middleware that blocks the probes or
replaces that header. The toolbox disables captive portal checks over plain HTTP.

## Maintain the relay

- Keep `relay.toml`, `.env`, `compose.yaml`, and `Caddyfile`, and back up Caddy's
  `/data` volume securely. If moving to another host, regenerate `.env` with that
  user's IDs and ensure they own `relay.toml`. The relay does not store
  transferred files.
- To rotate a token, edit `access.shared_token` and run
  `docker compose up -d --force-recreate relay`. Update both clients and repeat
  the registration and relay-only ping checks. The list may temporarily contain
  old and new tokens during migration; remove the old token and recreate the
  relay again to revoke it. Restarting disconnects active relay sessions.
- To upgrade, choose explicit image versions, update `compose.yaml`, then run
  `docker compose pull` and `docker compose up -d`. Repeat the same checks.
- Keep `RUST_LOG=info` for routine operation; debug configuration dumps can
  expose secrets. Review logs before sharing them.
- `docker compose down` stops the stack and retains its named volumes. Avoid
  `down -v` when you want to preserve certificate state.
