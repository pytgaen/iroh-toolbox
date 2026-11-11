#!/usr/bin/env bash
# Verify that a release tag is annotated, signed by a maintainer key and on main.
#
# Usage: scripts/verify-release-tag.sh vX.Y.Z [main-ref]
#
# The trusted keys are read from .github/allowed_signers on the main ref, never
# from the tagged tree, so a tag cannot vouch for itself.
set -euo pipefail

tag=${1:?usage: verify-release-tag.sh vX.Y.Z [main-ref]}
main_ref=${2:-origin/main}

fail() {
  echo "error: $*" >&2
  exit 1
}

signers=$(mktemp)
trap 'rm -f "$signers"' EXIT
git show "$main_ref:.github/allowed_signers" >"$signers" ||
  fail "cannot read .github/allowed_signers from $main_ref"

[ "$(git cat-file -t "refs/tags/$tag" 2>/dev/null)" = tag ] ||
  fail "$tag is not an annotated tag"

git -c gpg.format=ssh -c gpg.ssh.allowedSignersFile="$signers" verify-tag "$tag" ||
  fail "$tag is not signed by a key listed in .github/allowed_signers"

commit=$(git rev-parse "refs/tags/$tag^{commit}")
git merge-base --is-ancestor "$commit" "$main_ref" ||
  fail "$tag ($commit) is not on $main_ref"

echo "$tag: annotated, signed and on $main_ref ($commit)"
