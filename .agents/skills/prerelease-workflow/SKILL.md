---
name: prerelease-workflow
description: Validate iroh-toolbox locally before a PR or stable release with native tests, reference relay and pairing scenarios and extracted development archives. Use for local prerelease or distribution checks.
---

# iroh-toolbox local prerelease validation

Read [notes/release-workflow.md](../../../notes/release-workflow.md) and the
[development guide](../../../docs/maintainers/development.md) first, and use the
`mise` tasks below.

## Scope

These checks build disposable test infrastructure and ignored `-dev` artifacts
only. They do not bump the manifest, edit `CHANGELOG.md`, commit, tag or publish.
Reuse valid reference relays and compiled artifacts when available.

The gate is local: it never triggers a CI run. The maintainer-only rules of the
[release skill](../release-workflow/SKILL.md) apply here too: never sign with
the maintainer's key, generate keys or touch the SSH agent, and explain any
test that signs or verifies signatures before running it, then wait for
consent.

Release candidates (`rc.N`) are not published: there is no prerelease workflow
and the release validator rejects candidate tags.

## Local gate

1. Inspect the branch, the worktree and the intended binary's version and
   features. A dirty checkout gets a `-dev` label, never a stable one.
2. Verify the build:

   ```sh
   mise run test
   mise run docs:check
   ```

3. Use the reference relay pinned by CI, `iroh-relay 1.4.1` with server support
   (`mise run test:relay:install`), and set `IROH_TEST_RELAY_BINARY` to its path
   (`iroh-relay.exe` on Windows). Keep a relay provisioned for CI outside Cargo's
   cached `target/` directory.
4. Run the ignored scenarios explicitly:

   ```sh
   mise run test:relay
   mise run test:pairing
   mise run test:slots
   mise run test:pty   # Unix only
   ```

   Pairing runs serially. These scenarios use a local relay; they need neither
   public infrastructure nor production relay credentials.
5. Build and package like CI releases:

   ```sh
   mise exec -- cargo build --locked --release
   mise exec -- python scripts/package-release.py --output dist/prerelease
   ```

   Packaging produces a `vX.Y.Z-dev` host archive and checks its version,
   platform and embedded commit; rebuild after HEAD changes. Pass the archive
   path it prints to `mise exec -- python scripts/smoke-archive.py ARCHIVE_PATH`.
   Avoid `mise run dist:smoke` here: it checks every archive in `dist/`,
   including older or foreign-platform builds. The smoke check validates
   checksums, extracted provenance, a code transfer of a folder and an empty
   file, collision refusal and HTTP forwarding to a paired machine outside the
   checkout, through a local relay: set `IROH_TEST_RELAY_BINARY` first.
6. Build changed documentation with `mise run docs:build` and preview the target
   draft with `python scripts/release-notes.py vX.Y.Z --draft`.

Stop dependent checks after a failure and finish the independent ones. In the
report, separate the checks that ran, those the maintainer refused and those
that could not run, with the reason. Public-relay tests are a
separate, opt-in check (`mise run test:public-relay`).
