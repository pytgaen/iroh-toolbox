# iroh-toolbox agent instructions

Use the `mise.toml` tasks for builds, tests and documentation. Check
`git status --short --branch` before editing and keep unrelated changes intact.
Commit, push, tag or publish only when asked. Never merge pull requests or
bypass rulesets, never trigger CI without approval, and never use or alter
signing keys without consent: see the maintainer-only actions of the
[release skill](.agents/skills/release-workflow/SKILL.md).

## Changelog and releases

Follow [notes/release-workflow.md](notes/release-workflow.md) for commits, PRs,
version bumps, changelog drafts, tags and releases. Repository skills:

- [release-workflow](.agents/skills/release-workflow/SKILL.md): Git closeout,
  changelog drafts and stable releases.
- [prerelease-workflow](.agents/skills/prerelease-workflow/SKILL.md): local
  validation before a PR or a release.

Development changes belong in `notes/changelog-X.Y.Z.md`, never in
`CHANGELOG.md`. A stable release is a dedicated `chore(release): X.Y.Z` commit
on `main`, limited to `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` and deletion of
the consumed draft. Work reaches `main` through squash-merged PRs.

## Verification

- Rust or build changes: focused checks, then `mise run lint` and `mise run test`.
- Documentation changes: verify affected examples and `mise run docs:build`.
- Release-script changes: `python -m unittest discover -s tests -p test_release_scripts.py`.
- Skill changes: validate their frontmatter.

Routine tests use direct-only or local-relay scenarios; public-relay checks
(`mise run test:public-relay`) run only on request. When reporting results, list
the ignored scenarios that were not run. Never include relay tokens or sensitive
tickets in output.
