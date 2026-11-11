# iroh upstream requests

Two changes to request from iroh (`n0-computer/iroh`), noted on 2026-11-01
while specifying the [address takeover warning](peers-spec.md#address-takeover-warning).
Nothing is posted yet. Existing issues were searched on 2026-11-01 (latest
release: 1.4.1): the first request follows up a closed issue, the second
joins an open one. Check them again before posting. Facts below were checked
against iroh, iroh-relay and iroh-base 1.4.1 and iroh-dns 1.4.1 (the versions
pinned in `Cargo.toml`).

## 1. Relay status frames in `RelayStatus`

**Why we need it.** The address takeover warning must know when another
registration of a slot key displaces the slot server at its relay. iroh only
logs that signal, so 0.1.0 captures it with a `tracing-subscriber` layer.

**Facts.**

- iroh-relay `src/server/clients.rs`, `Clients::register`: a second
  connection with the same endpoint ID becomes active; the previous one stays
  open, is marked inactive and receives `Status::SameEndpointIdConnected`
  ("No more messages will be received"). When the active connection
  unregisters, the last inactive one is promoted and receives `Status::Healthy`.
- iroh `src/socket/transports/relay/actor.rs`, `RelayToClientMsg::Status`
  arm: the status is only passed to `info!` / `warn!`.
- `Endpoint::home_relay_status()` returns `RelayStatus` values with
  `last_error()` and `auth_denied_reason()`, nothing about this status.
  iroh's `main` branch still only logs it (checked on 2026-11-01).

**Prior art.** [#3940](https://github.com/n0-computer/iroh/issues/3940)
("Expose connection status to home relay") proposed exactly this: a
`Degraded` state with `RelayDegradationReason::OtherEndpointWithSameIdConnected`,
because "without that all we can do is log a `WARN` which is easy to miss".
Reviewers asked for a smaller API, with the remark that "the disconnected
variant can still have a reason inside it". The fix,
[#4149](https://github.com/n0-computer/iroh/pull/4149) (merged 2026-04-22),
added `home_relay_status()` with `is_connected()` and `last_error()` only,
keeping the status enum private "while allowing us to add more information in
the future". No follow-up issue exists. Open a new issue that references both,
rather than commenting on the closed one.

**When it is available:** read the status from `home_relay_status()`, remove
the `tracing` layer and the `tracing-subscriber` dependency, and update the
spec bullet that describes the capture.

### Issue draft

> **`RelayStatus`: report when another endpoint with the same ID took over the relay connection**
>
> Follow-up to #3940 / #4149. #3940 proposed exposing
> `OtherEndpointWithSameIdConnected`; #4149 shipped `home_relay_status()` with
> `is_connected()` and `last_error()` and left room for more information. This
> asks for that piece.
>
> When a second connection registers the same endpoint ID at a relay,
> iroh-relay keeps the first connection open, marks it inactive and sends it
> `Status::SameEndpointIdConnected`. From then on the endpoint receives no
> relayed traffic, yet its home relay still looks connected: iroh 1.4.1 only
> logs the frame (`socket/transports/relay/actor.rs`, `RelayToClientMsg::Status`).
> The same goes for `Healthy` when the connection is promoted back, and for
> `RateLimited`.
>
> **Use case.** We derive endpoint keys that several machines can compute: the
> key locates a service, and an application handshake authenticates it. A
> machine that lost its access can still register the same key and silently
> take the service's relay traffic. We want to tell the operator, but today
> the only way is a `tracing` subscriber that matches the log event and
> attributes it through the span active when the endpoint was bound. That
> depends on log text and span layout, which can change in any release.
>
> **Proposal.** Record the last status the relay sent on the current
> connection in `RelayStatus`, for example `RelayStatus::server_status() ->
> Option<Status>` with a re-exported or iroh-level `Status` enum, reset on
> reconnection, so it is observable through `Endpoint::home_relay_status()`.
> We do not ask iroh to react (reconnecting would make the two registrations
> alternate), only to make the state visible.

## 2. Endpoint keys built from an expanded secret

**Why we need it.** This removes the takeover itself (option A of the
analysis). With key blinding, as in Tor v3 onion addresses (rend-spec-v3,
appendix A.2), a slot's public key would be `E = h·M` with
`h = H("iroh-toolbox/slot/v2" ‖ salt ‖ M ‖ slot)`: peers compute it from what
they already store, but only the machine can compute the secret scalar
`h·m`. A removed peer could still locate a slot, never bind it, and
`id rotate --address` would only matter for privacy. The pairing payload does
not change.

**Facts.**

- iroh-base `src/key.rs`: `SecretKey(ed25519_dalek::SigningKey)`, built only
  from a 32-byte seed (`from_bytes`). A blinded scalar has no seed.
- Every signature an endpoint makes goes through `SecretKey::sign`: TLS
  (iroh `src/tls/resolver.rs`), relay authentication (iroh-relay
  `src/protos/handshake.rs`) and DNS records (iroh-dns `src/pkarr.rs`). A
  variant inside `SecretKey` would therefore cover all three without changing
  callers. `to_bytes()` and serialization cannot return a seed for it.
- Verifiers are unaffected: the signatures are ordinary Ed25519 signatures
  under `E`.

**Prior art.** [#2355](https://github.com/n0-computer/iroh/issues/2355)
("Allow using iroh with other signers"), open since 2024-06 with activity in
2026-08 (Radicle users), asks for a pluggable signer, possibly built on
`signature::Signer<ed25519::Signature>`, for HSMs, FIDO tokens and key
stores. A pluggable signer covers this request too: we would implement it with
the expanded key and `ed25519_dalek::hazmat::raw_sign`. Maintainers raised
performance (hardware access on every use), FFI support and a possible
identity redesign around certificate chains; an in-memory blinded key has none
of the hardware concerns. No issue mentions key blinding or expanded keys.
Comment on #2355 with this use case rather than opening a new issue.

**Before posting:** write the derivation precisely (scalar reduction,
signing-nonce prefix, small-order checks) and have it reviewed, as was done
for SPAKE2 ([spake2-review.md](spake2-review.md)). Moving to it changes every
slot address, so all machines must run the new version together.

### Comment draft for #2355

> Another use case for this, where the key does stay in memory: key blinding,
> as in Tor v3 onion services. Peers derive a service's public endpoint ID
> from a public key and a shared secret, `E = h·M`, while only the owner of
> `M`'s private key can compute the matching secret scalar `h·m` and bind the
> endpoint. Knowing where a service is then no longer allows registering it
> at a relay or publishing its DNS record.
>
> Such a key has no seed, so today's `SecretKey(SigningKey)` cannot hold it.
> A signer abstraction would cover it: TLS, relay authentication and DNS
> publishing all sign through `SecretKey::sign`, and the signatures stay
> ordinary Ed25519 signatures, so verifiers are unaffected. A narrower option
> would be a `SecretKey` built from an `ed25519_dalek::hazmat::ExpandedSecretKey`,
> signing with `hazmat::raw_sign`; `to_bytes()` and `Serialize` would not
> apply to it.
