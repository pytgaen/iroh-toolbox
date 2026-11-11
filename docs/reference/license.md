# License

iroh-toolbox is licensed under the Apache License 2.0, for its sources and for
every build, including release archives. All dependencies use permissive
licenses checked by `cargo deny` (see `deny.toml`); code pairing relies on the
RustCrypto `spake2` crate (MIT OR Apache-2.0).

Pairing codes use the [EFF Large Wordlist for Passphrases](https://www.eff.org/dice),
created by the Electronic Frontier Foundation and licensed under
[CC BY 3.0 US](https://creativecommons.org/licenses/by/3.0/us/). The list is
embedded unmodified in `src/eff_large_wordlist.txt`; the attribution is recorded
in `NOTICE`, which every release archive includes as `NOTICE.txt`.

The complete Apache license follows.

```text
--8<-- "LICENSE.txt"
```
