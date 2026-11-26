## [0.2.0] — YYYY-MM-DD

### Dependencies

- Compatible updates of `anyhow`, `clap`, `hyper`, `sha2`, `toml`, `url`,
  `walkdir` and `zeroize`; `url` 2.5.9 fixes RUSTSEC-2026-0592.
- Deferred: `iroh` 1.5.0 and `iroh-blobs` 0.105.0. Both are pinned exactly and
  move together with the reference `iroh-relay`, in their own change.
- `chrono` is reported unmaintained (RUSTSEC-2026-0543), with no known flaw;
  it stays until its replacement with the `iroh` 1.5 update.
