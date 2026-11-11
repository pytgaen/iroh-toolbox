# Release workflow

This note details the iroh-toolbox branch, changelog and release procedure.
[CONTRIBUTING.md](../CONTRIBUTING.md) summarizes it, and the skills under
`.agents/skills/` apply it for coding agents.

## Development and changelog drafts

Work on a dedicated branch and merge through a PR with squash merge. One PR
represents one semantic intention. Use a Conventional Commit title and a body
with one conventional bullet per meaningful change, for example:

```text
feat(files): improve code-based transfers

- feat(files): pair sender and receiver with one-use codes
- fix(files): refuse colliding destination names
- docs: explain verification on the receiving peer
```

The PR title and description become the squash commit: the repository allows
only squash merges and uses the PR title and body as the commit message.

Write the future public section in `notes/changelog-X.Y.Z.md` during development.
Do not edit `CHANGELOG.md` in feature, fix, documentation or maintenance commits.
The draft is tracked, shared by the work for that target version, and deleted
only when its content is moved into the public changelog at release time.

Draft heading: `## [X.Y.Z] — YYYY-MM-DD`; replace the placeholder with the actual
release date only when finalizing the release. Read the latest published
sections and match their editorial style: optional Highlights, then Features,
Bug Fixes, Performance, Refactoring, Documentation and Housekeeping as needed.
Use `- **scope:** ...` with behavior, benefit or maintainer context; do not paste
raw commit messages. Preserve useful compatibility and known-limitations notes.

Preview a draft without publishing or requiring the manifest to be bumped:

```sh
python scripts/release-notes.py vX.Y.Z --draft
```

The draft is canonical. Conventional subjects and PR bodies help determine its
coverage and the next version.

## Version selection

Inspect subjects and bodies since the latest stable `vX.Y.Z` tag. Without a
stable tag, the first release is `0.1.0`.

| Change | Bump |
| --- | --- |
| `fix`, `perf` | patch |
| `feat` | minor |
| `!` or `BREAKING CHANGE:` | minor before 1.0; major after 1.0 |
| `docs`, `refactor`, `chore`, `ci`, `test`, `style`, `build` alone | no automatic release |

The highest applicable bump wins. Moving to 1.0 is a product decision.

## Before a PR

1. Update the target draft with the user-visible changes; leave `CHANGELOG.md`
   untouched.
2. Run `mise run outdated`, `mise run lint`, `mise run test` and
   `mise run lint:msrv`, then `mise run docs:build` when docs change.
3. Run the relay, pairing, slot and terminal scenarios (`mise run test:relay`,
   `test:pairing`, `test:slots`, `test:pty`), then build a development archive
   and run its extracted smoke checks, as listed in the prerelease skill.
4. Review the diff and commit, signing when configured.
5. Push the branch and open the PR. CI runs only on manual dispatch: once the
   maintainer approves the run, dispatch it on the branch
   (`gh workflow run ci.yml --ref <branch>`). GitHub never counts dispatched
   runs toward a pull request's required checks, so every merge goes through
   the maintainer's ruleset bypass. Before ticking it, the maintainer checks
   that the latest run on the branch succeeded on the PR's head commit
   (`gh run list -b <branch> -L 1`). Wait for that merge before preparing a
   release.

Only the maintainer merges pull requests and bypasses the ruleset; coding
agents follow the maintainer-only rules of the
[release skill](../.agents/skills/release-workflow/SKILL.md).

If `outdated` fails, update the dependencies on a work branch and rerun the
affected checks, or defer them and record the deferred versions and the reason
in the target draft.

## Stable release

Nobody pushes to `main` directly, the release included: the release commit goes
through its own PR, and the tag is created afterwards on the squash commit.

1. Start from a clean, up-to-date `main` (`git pull --ff-only`) after the work
   PR is merged, and create a `release/X.Y.Z` branch.
2. Select the version from the semantic delta and locate its draft.
3. Run `mise run outdated` again. Set `Cargo.toml` to `X.Y.Z`, regenerate
   `Cargo.lock` with `mise exec -- cargo build`, and run
   `mise exec -- cargo test --locked --all-features --bin iroh-toolbox`.
4. Date the draft and finalize its text. Insert the section before the first
   published version in `CHANGELOG.md`, leaving older sections unchanged. For
   the first release, replace the no-release notice.
5. Delete the consumed `notes/changelog-X.Y.Z.md`. Validate with
   `python scripts/release-notes.py vX.Y.Z`: without `--draft`, it reads only the
   public changelog, requires a real date and matches the manifest version.
6. Stage only `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` and the draft deletion.
   Check `git diff --cached --name-status` and `git diff --cached --check`.
7. Commit with the subject `chore(release): X.Y.Z`, signing when configured.
8. Push the branch and open the PR titled `chore(release): X.Y.Z`. Dispatch CI on
   it after approval. The maintainer squash-merges it through the bypass only
   after that run succeeded on the PR's head commit: a pushed `vX.Y.Z` tag can
   no longer be moved or deleted, so a release PR is never merged without
   green CI.
9. Update `main` (`git switch main && git pull --ff-only`) and create a signed
   annotated tag on the squash commit:
   `git tag -s vX.Y.Z -m "iroh-toolbox X.Y.Z"`. Check it locally with
   `scripts/verify-release-tag.sh vX.Y.Z`, then push only that tag with
   `git push origin vX.Y.Z`. Avoid `git push --tags`, `--all` and `--mirror`,
   which would also publish local branches and tags.
10. The release workflow verifies the tag, runs the full CI, then waits for the
    maintainer's approval of the `release` environment before publishing.

For `0.1.0`, the Cargo files already carry the version, so the release commit
contains only `CHANGELOG.md` and the deletion of `notes/changelog-0.1.0.md`.

## Prerelease validation and publication

The [prerelease skill](../.agents/skills/prerelease-workflow/SKILL.md) lists the
local gate: tests, relay scenarios and `-dev` archives. It does not bump versions
or edit the public changelog.

Release candidates (`vX.Y.Z-rc.N`) are not published: there is no prerelease
workflow, and the release validator rejects candidate tags.

A stable tag runs the complete reusable CI before publication: all platform
checks, code-pairing scenarios, archives with license and notice, checksums and
extracted-archive file/HTTP smoke checks. Only the publish job has contents-write
permission. It attests the build provenance of the archives and install
scripts (`gh attestation verify <file> --repo pytgaen/iroh-toolbox`).

## GitHub repository settings

These settings carry the release guarantees; change them only deliberately.

- **`main` ruleset**: pull request required, squash merge only, linear history,
  signed commits, no force push or deletion. Required checks: `Lint`,
  `Test (ubuntu-latest)`, `Test (windows-latest)`, `Minimum Rust`,
  `Advisories, licenses and sources`, `Documentation`,
  `Build x86_64-unknown-linux-musl` and `Build x86_64-pc-windows-msvc`. A
  repository admin can bypass it only from a pull request, by explicitly
  ticking the bypass option when merging. Because CI runs only on manual
  dispatch, and dispatched runs never satisfy a pull request's required checks,
  every merge uses this bypass: it records the maintainer's confirmation that
  CI succeeded on the head commit. Renaming one of these CI jobs requires
  updating the ruleset.
- **`release-tags-create` ruleset**: only a repository admin can create `v*`
  tags.
- **`release-tags-immutable` ruleset**: nobody can move or delete a `v*` tag.
  Published releases are also immutable.
- **Tag verification**: the release workflow runs `scripts/verify-release-tag.sh`.
  The tag must be annotated, sit on `main`, and be signed by a key listed in
  `.github/allowed_signers` on `main`. Adding a maintainer key goes through a
  PR.
- **`release` environment**: limited to `v*` tags; the maintainer approves each
  publication.
- **`github-pages` environment**: limited to `main`.
- **Actions**: only GitHub actions and the allow-listed third-party actions,
  pinned by full commit SHA. Workflows from outside contributors wait for
  approval.
- **Security**: private vulnerability reporting, Dependabot alerts and security
  update PRs, secret scanning with push protection.
