# `spake2` 0.4.0 — security review

Date: 2026-10-27. Scope: the crate source as published on crates.io
(`src/lib.rs`, `src/ed25519.rs`, `src/group.rs`, `src/error.rs`), its
dependencies in this repository's `Cargo.lock`, and public advisories. This is a
maintainer code review with targeted checks, **not** an independent audit; the
crate README states that it has never received one.

## Summary

The crate is a small, readable implementation of SPAKE2 over edwards25519,
compatible with python-spake2 and magic-wormhole. No defect found allows an
offline dictionary attack or key recovery. Two deviations from RFC 9382 and two
hygiene points are mitigated in iroh-toolbox's protocol layer (see
[pairing-protocol.md](https://github.com/pytgaen/iroh-toolbox/blob/main/docs/reference/pairing-protocol.md)). Suitable for use with those
mitigations.

## Facts checked

| Item | Result |
| --- | --- |
| License | MIT OR Apache-2.0 |
| Maintenance | RustCrypto/PAKEs; 0.4.0 (2023-07-28) is the latest stable, 0.5.0-pre.0 (2026-01-25); commits in July 2026 |
| `unsafe` | `#![forbid(unsafe_code)]` |
| Arithmetic | curve25519-dalek 4.1.3 (constant-time scalar multiplication); RUSTSEC-2024-0344 is fixed in this version |
| Advisories | None for `spake2` in RustSec; `cargo deny check advisories` passes |
| Constants M, N, S | Decompress to valid points, **torsion-free**, not small-order (checked) |
| Reflection | Messages carry a side byte (`A`/`B`/`S`); a reflected message is rejected (`BadSide`) |
| Password to scalar | HKDF-SHA256(salt = "", info = "SPAKE2 pw"), 48 bytes reduced mod ℓ: negligible bias |
| Key | SHA-256 over six fixed-length 32-byte fields: H(pw), H(idA), H(idB), X, Y, K; unambiguous encoding |
| Randomness | `OsRng` (getrandom) by default |

## Findings

| # | Finding | Severity | Mitigation in iroh-toolbox |
| --- | --- | --- | --- |
| 1 | No cofactor clearing and no subgroup check in `finish()`: a peer message with a small-order (torsion) component is accepted and changes the derived key (checked: honest message + 8-torsion point → keys differ). RFC 9382 requires either multiplying by the cofactor or rejecting such elements. An attacker who does not know the password cannot exploit the difference, but it is a deviation and leaks low bits of the ephemeral scalar into a key the attacker cannot compute. | Low | Decode the peer element before `finish()` and reject it unless it is torsion-free and not the identity. |
| 2 | The identity point is accepted as a peer message (checked). | Low | Same check as #1. |
| 3 | No built-in key confirmation; the returned key is not split into encryption and confirmation keys (as in RFC 9382). | By design (documented) | HKDF-derived confirmation keys and MACs in both directions, bound to the transcript. |
| 4 | The password and ephemeral scalar stay in memory after use; no zeroization. | Informational | Known limit: the 0.4 state (`Spake2`) does not implement `Zeroize` and its scalar cannot be wiped from outside. iroh-toolbox wraps what it owns in `zeroize::Zeroizing` (the code password it passes in, the shared key returned by `finish()`, the derived confirmation keys), and drops the state promptly after `finish()` in a short-lived process. |
| 5 | Point decompression accepts non-canonical encodings (curve25519-dalek behavior). Both sides hash the exact bytes exchanged, so it causes no key mismatch or attack. | Informational | None needed. |
| 6 | Constants follow python-spake2 / magic-wormhole rather than RFC 9382's published parameters; their seed-based generation was not regenerated here. | Informational | Torsion-freeness checked; interoperability with RFC 9382 is not a goal. |

## Follow-up

- Track `spake2` 0.5 (curve25519-dalek 5) and re-run these checks when upgrading.
- Keep the torsion and identity rejection under a unit test.
- Frozen test vectors (code to rendezvous ID, HKDF keys, commitment, confirmation numbers, signed `pair` statement, both slot statements) live next to the code in `src/pairing/{code,handshake,record}.rs` and `src/slot.rs`; deterministic robustness tests for every frame parser and `validate_name` use the seeded generator in `src/fuzz.rs`. A change to a vector is a wire-format change.
- Seek an independent review of the full pairing protocol before 1.0.
