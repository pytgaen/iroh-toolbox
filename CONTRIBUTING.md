# Contributing

## Local development

Install [mise](https://mise.jdx.dev/), then run `mise trust` and `mise install`.
The optional `.env` supplies local relay settings; never commit it or relay tokens.
Python 3.11 or newer is used by the documentation, minimum-Rust task, and
release-note validator.

```sh
mise run build
mise run test
mise run lint
mise run outdated
```

`lint` includes `cargo deny` checks for advisories, removed crate versions,
licenses, and dependency sources. Direct dependency updates are reviewed through
weekly Dependabot PRs. Review paired iroh/iroh-blobs updates for API and wire
compatibility; rerun transfer tests before changing the pinned versions.

Install the Rust version declared by `package.rust-version` in `Cargo.toml`, then
run `mise run lint:msrv`. The CI reads that declaration too, so a new compiler
requirement must update it deliberately.

Tests exercise real local sockets. Scenarios that need a relay or a real
terminal are ignored by the ordinary suite and run separately, locally and in
Linux/Windows CI. Build the pinned official relay once, then run them:

```sh
mise run test:relay:install
export IROH_TEST_RELAY_BINARY="$PWD/target/test-relay/bin/iroh-relay"
mise run test:relay
mise run test:pairing
mise run test:slots
mise run test:pty   # Unix only; also needs IROH_TEST_RELAY_BINARY
```

On Windows, set `$env:IROH_TEST_RELAY_BINARY` to the absolute path of
`target/test-relay/bin/iroh-relay.exe`. The relay scenarios fail if the binary
is absent or invalid. `pair` needs interactive terminals on both machines and is
tested end to end only through Unix PTYs (inside `test:pairing`), not on Windows.

## Documentation

The site uses Zensical, pinned in `docs/requirements.txt`. Its configuration is
`zensical.toml`; guides and reference pages live in `docs/`. Contribution and
changelog pages include the root files so their content stays in one place.

```sh
mise run docs:serve
mise run docs:build
```

The preview runs at `http://127.0.0.1:8000/`. A strict build writes `site/` and
fails on documentation warnings. CI runs it before building release archives and
uploads the site as the `documentation-site` artifact, retained for 14 days.

After changing CLI help or options, regenerate and verify the reference:

```sh
mise run docs:cli
python scripts/generate-cli-reference.py --check
```

Linux and Windows CI check this reference against the compiled command.

### Publish to GitHub Pages

The documentation is published at `https://pytgaen.github.io/iroh-toolbox/`. If
the repository name changes, update `repo_url`, `repo_name`, and `site_url` in
`zensical.toml` and the README links.

1. In **Settings → Pages**, select **GitHub Actions** as the build source.
2. Each push to `main` that changes the documentation (`docs/`, `zensical.toml`,
   or the root files the site includes: `CHANGELOG.md`, `CONTRIBUTING.md`,
   `LICENSE.txt`) builds and publishes the site.
3. To inspect the site first, run the **Documentation** workflow on `main`
   without `deploy` and download its artifact; with **Publish to GitHub Pages**
   enabled, a manual run publishes it.

Only the deployment job receives Pages write and OIDC permissions.

## Issues and pull requests

Include the OS, `iroh-toolbox --version`, command, expected behavior, observed
error, and whether the connection uses a direct path or relay. Replace secrets,
tickets, private addresses, and sensitive file paths with placeholders. For
security-sensitive reports, use GitHub private vulnerability reporting if the
repository offers it; do not publish credentials or exploit details in an issue.

Keep changes focused, explain the user-visible effect, and list the checks run.
Work on a dedicated branch and use squash-merged PRs. Use a Conventional Commit
PR title and a body with one conventional bullet per meaningful change; the
squash commit retains both.

## Changelog convention

During development, write user-facing changes in `notes/changelog-X.Y.Z.md`.
**Do not edit `CHANGELOG.md` in feature, fix or maintenance commits.** The draft
is the complete future public section, headed `## [X.Y.Z] — YYYY-MM-DD`, with
editorial Highlights when useful and scoped bullets explaining behavior and
benefit. Match the style of the latest published sections.

```sh
python scripts/release-notes.py vX.Y.Z --draft
```

This previews the draft before the manifest bump and accepts the date placeholder.

## Release preparation

The detailed procedure is in the
[release workflow](https://github.com/pytgaen/iroh-toolbox/blob/main/notes/release-workflow.md).

1. Before opening the PR, run `mise run outdated`, `mise run lint`,
   `mise run test`, `mise run lint:msrv`, the documentation checks when docs
   change, and the relay, pairing, slot and terminal scenarios above.
2. After green CI and the squash merge, prepare the release on a
   `release/X.Y.Z` branch from an up-to-date `main`. Run `mise run outdated` again; record any deferred dependency update
   and its reason in the draft.
3. Set `Cargo.toml` to `X.Y.Z` (already the case for `0.1.0`), regenerate
   `Cargo.lock` with `mise exec -- cargo build`, and run
   `mise exec -- cargo test --locked --all-features --bin iroh-toolbox`.
4. Date the draft, move its section into `CHANGELOG.md` above older entries, and
   delete the draft. `python scripts/release-notes.py vX.Y.Z` must pass
   **without `--draft`**.
5. Commit only `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` and the draft deletion,
   with the subject `chore(release): X.Y.Z`, and open a PR with that title.
   The maintainer merges it, through the ruleset bypass, only after a
   successful CI run on its head commit.
6. On the updated `main`, create a signed annotated tag on the squash commit
   (`git tag -s vX.Y.Z -m "iroh-toolbox X.Y.Z"`), check it with
   `scripts/verify-release-tag.sh vX.Y.Z`, then push it alone with
   `git push origin vX.Y.Z`. Avoid `git push --tags`: a pushed release tag can
   never be moved or deleted.

Release candidates are not published: the release workflow accepts only stable
`vX.Y.Z` tags.

The release workflow verifies the tag's signature and its ancestry on `main`,
validates the changelog, runs the full CI, verifies the platform archives
against their checksums, waits for the maintainer's approval, attests their
build provenance and publishes their changelog notes.
Each archive includes `LICENSE.txt`, `NOTICE.txt` (word list attribution; see
[the license reference](https://pytgaen.github.io/iroh-toolbox/reference/license/)), an offline `QUICKSTART.txt`,
and `BUILD.json` with version/target/source provenance. CI checks each
executable's `--version`, then runs the extracted archive through a folder
transfer with a one-use code, collision refusal and two HTTP requests through a
tunnel between two paired machines before uploading it. Only the
publication job can write repository contents.

Run `mise run dist` followed by `mise run dist:smoke` to build a host-platform
`-dev` archive and run the same check locally, outside the checkout and with
isolated relay settings. It starts a loopback relay, so set
`IROH_TEST_RELAY_BINARY` as above.

Stable archives require a clean checkout and a binary built from its current
commit. Local archives from a modified checkout use a `-dev` label, which
`BUILD.json` records.

Before 1.0, features and breaking changes increment the minor version; compatible
fixes increment the patch version.
