---
name: release-workflow
description: Prepare iroh-toolbox commits, PR closeout, editorial changelog drafts and stable releases following the repository's release workflow. Use for changelog, version bump, release preparation or Git closeout.
---

# iroh-toolbox release workflow

Follow [notes/release-workflow.md](../../../notes/release-workflow.md) and use the
`mise` tasks.

## Maintainer-only actions

These steps belong to the maintainer. An agent prepares them, gives the exact
commands and checks the result read-only afterwards.

- **Merging**: never merge a pull request, never run `gh pr merge --admin` and
  never bypass a ruleset. Dispatched CI never satisfies a pull request's
  required checks, so every merge goes through the maintainer's bypass, which
  stands for their check that CI succeeded on the head commit.
- **CI runs**: each run (dispatch, re-run, any workflow) needs the maintainer's
  explicit approval, with its expected duration stated. Approval covers one
  run only. Reproduce failures locally instead of iterating on CI.
- **Keys and signatures**: never sign with the maintainer's key except the
  commits and tags they asked for. Never generate keys or change the SSH agent,
  `~/.ssh` or the git signing configuration. Before any test that signs or
  verifies signatures, such as `scripts/verify-release-tag.sh`, explain the
  exact steps and side effects and wait for consent.
- **Release tags**: `v*` tags are immutable once pushed. Create and push one
  only on an explicit request, after `scripts/verify-release-tag.sh` passes.
- **Reporting**: state which checks ran, which were refused or skipped, and
  why. Never report a refused check as done.

## Assess the current state

Inspect `git status --short --branch`, staged, unstaged and untracked changes,
recent commit subjects and bodies, stable tags (`git tag --list 'v*'`, excluding
`-rc` tags) and the target draft. Without a stable tag, the target is `0.1.0`.

- Review or dry run: inspect and report without changing anything.
- Work branch: update the draft and the requested changes, then validate them.
- `main` with work changes: move them to a work branch instead of committing on
  `main`.
- Clean `main` after the work PR is merged: prepare the stable release when
  requested.
- Existing release commit or tag: verify it and continue from the first missing
  step.

## Work branch and editorial draft

1. Choose the target from the semantic delta: `feat` is minor, `fix`/`perf`
   patch; breaking changes are minor before 1.0 and major after.
   Documentation or maintenance alone needs no release.
2. Read recent public changelog sections. Write or update the complete future
   section in `notes/changelog-X.Y.Z.md`, headed `## [X.Y.Z] — YYYY-MM-DD`, with
   Highlights when useful and scoped bullets. Leave `CHANGELOG.md` untouched;
   if it has unexpected edits, compare them with the draft before deciding.
3. Run `python scripts/release-notes.py vX.Y.Z --draft`.
4. Before the PR, run `mise run outdated`, `mise run lint`, `mise run test`,
   `mise run lint:msrv`, the documentation checks and the
   [prerelease skill](../prerelease-workflow/SKILL.md). If `outdated` fails,
   update the dependencies or defer them and record the deferred versions and
   the reason in the draft.
5. For a commit, stage the intended scope, run `git diff --cached --check` and
   sign when configured. Use a Conventional Commit subject and body that
   describe the final change.
6. Write a PR title and body for one semantic intention, with one conventional
   bullet per meaningful change. CI runs only on manual dispatch; ask the
   maintainer before each run. Release preparation waits for a successful run
   on the PR's head commit and the maintainer's squash merge.

## Stable release

Follow the stable-release sequence of the workflow note:

- Nobody pushes to `main`, the release included. Start from a clean,
  up-to-date `main` with the work PR merged and the target draft present,
  create `release/X.Y.Z`, and run `mise run outdated` again.
- Bump `Cargo.toml`, regenerate `Cargo.lock` with `mise exec -- cargo build`
  (a locked build cannot update the root package entry), then run
  `mise exec -- cargo test --locked --all-features --bin iroh-toolbox`. For
  `0.1.0`, both manifests already carry the version.
- Date the draft, insert its section into `CHANGELOG.md` without rewriting older
  sections, and delete the draft. For the first release, replace the
  no-release notice.
- `python scripts/release-notes.py vX.Y.Z` must pass without `--draft`.
- Stage only `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` and the draft deletion,
  and commit with the subject `chore(release): X.Y.Z`. Push the branch and open
  the PR with the same title. The maintainer merges it through the bypass
  only after a successful CI run on its head commit.
- After the squash merge, the signed annotated tag goes on the squash commit
  of the updated `main` (`git tag -s vX.Y.Z -m "iroh-toolbox X.Y.Z"`) and must
  pass `scripts/verify-release-tag.sh vX.Y.Z`. Pushing it triggers the public
  release and it can never be moved or deleted afterwards. Create and push it
  only when asked, by name (`git push origin vX.Y.Z`), never with `--tags`.
- Publication then waits for the maintainer's approval of the `release`
  environment.

Finish with the local state, the checks run and the steps performed. Release
candidates are not published; use the prerelease skill for local validation.
