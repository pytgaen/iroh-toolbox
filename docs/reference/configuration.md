# Configuration

| Variable | Purpose | Default |
| --- | --- | --- |
| `IROH_TOOLBOX_RELAY` | Custom relay URL replacing the public set | iroh production public relays |
| `IROH_TOOLBOX_TOKEN` | Relay access token | No token |
| `IROH_TOOLBOX_CONFIG` | Path to a TOML file | No file |
| `IROH_TOOLBOX_TIMEOUT` | Connection timeout in seconds | `15` |
| `IROH_TOOLBOX_HOME` | Directory of the machine identity, name and peer book (`identity`, `peers.toml`): a secret, never to be synced or shared. Mode 0700 on Unix | Per-user configuration directory |
| `IROH_TOOLBOX_TIMESTAMPS` | When chat messages show their time: `auto`, `always` or `never` | `auto` (interactive terminal only) |

To use a file instead of relay environment variables:

```toml
# iroh.toml
[relay]
url = "https://relay.example.com"

[access]
shared_token = ["your-shared-token"]
```

```sh
export IROH_TOOLBOX_CONFIG="/path/to/iroh.toml"
iroh-toolbox ping serve
```

You can also read the relay server's TOML directly and provide its public URL:

```sh
iroh-toolbox --config /path/to/relay.toml --relay https://relay.example.com ping serve
```

Place global options **before** the command. Precedence, highest first:

| Setting | Precedence |
| --- | --- |
| Relay | `--relay` → `IROH_TOOLBOX_RELAY` → `IROH_RELAY_URL` → TOML `relay.url` → public defaults |
| Token | `IROH_TOOLBOX_TOKEN` → `IROH_RELAY_ACCESS_TOKEN` → TOML `access.shared_token` |
| File | `--config` → `IROH_TOOLBOX_CONFIG` |
| Timeout | `--timeout` → `IROH_TOOLBOX_TIMEOUT` → 15 seconds |
| Chat timestamps | `chat --timestamps` → `IROH_TOOLBOX_TIMESTAMPS` → `auto` |

Code pairing (`send`, `chat`, `pair`) needs no extra configuration: the machines
meet through the selected relays (public by default, or the same `--relay` on
both machines). The machine identity and the paired machines are not part of the
TOML file: they live in `IROH_TOOLBOX_HOME`, see [machine pairing](peers.md#peer-book).
Relay credentials stay local to each machine. See the
[pairing protocol](pairing-protocol.md) for lookup and deadlines.

The legacy `IROH_RELAY_*` variables are still accepted. If the TOML contains
multiple tokens, select one for the client with `IROH_TOOLBOX_TOKEN`. A token
requires an explicit custom relay URL, so a private token can never be attached
to the default public relays. That URL must use `https://`, so the token is never
sent in clear text; `http://` is accepted only for loopback test relays such as
`http://127.0.0.1:3340`. Open relays need no token.

| Selection | Behavior |
| --- | --- |
| No custom URL | Use iroh's built-in production public relay map, with direct UDP available. |
| `--relay URL` or relay environment/TOML | Use only this custom relay, with direct UDP available. |
| `--relay-only` | Disable direct UDP; require registration with the selected custom or public relays. |
| `--direct-only` | Disable iroh relays and ignore relay/token settings from the environment or TOML. |

`--direct-only` conflicts with explicit CLI `--relay` and `--relay-only`.
It requires direct addresses: `--addr HOST:PORT` for a `peer:` target, or a
ticket with usable addresses for `ping` and `bench`; public relay hints in a
ticket cannot enable the disabled relay transport. Codes (`send`, `receive`,
`chat`, `pair`) need a relay to meet and are refused with `--direct-only`.

Startup waits up to `--timeout` for relay registration. If public registration
fails, ordinary peer operations can still continue with direct addresses;
`--direct-only` skips that wait. An explicitly selected custom relay failure is
reported without switching to public relays. `relay check` always requires an
actual registration and reports the selected URL and `mode=public` or `custom`
in JSON; it is unavailable with `--direct-only`.
