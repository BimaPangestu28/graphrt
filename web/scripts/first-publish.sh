#!/usr/bin/env bash
# Publishes the very first version of the npm package `graphrt` by hand.
#
# npm lets a package name a trusted publisher (the release workflow) only once
# the package exists, so its first version is published once from the machine
# of an owner of the npm account. Every later version is published by the
# `npm` job of .github/workflows/release.yml. See web/README.md, "Publishing".
#
# It refuses to run unless `npm whoami` succeeds, refuses when `graphrt` is on
# npm already, and builds from a fresh clone of the release tag, never from
# this working tree. npm asks for the one-time password of the account when
# it publishes (or set NPM_CONFIG_OTP).
#
# Usage: web/scripts/first-publish.sh [--dry-run] vX.Y.Z
#   --dry-run  do everything but publish: write the .tgz and print its path
# Environment: GRAPHRT_REPO_URL (default: https://github.com/Graphrt/graphrt.git)
set -euo pipefail

usage() {
  echo "usage: web/scripts/first-publish.sh [--dry-run] vX.Y.Z" >&2
  exit 2
}

dry_run=false
tag=""
for argument in "$@"; do
  case "$argument" in
    --dry-run) dry_run=true ;;
    v[0-9]*) [ -z "$tag" ] || usage; tag="$argument" ;;
    *) usage ;;
  esac
done
[ -n "$tag" ] || usage
case "$tag" in
  *[!0-9A-Za-z.+-]*) echo "first-publish: '$tag' is not a release tag" >&2; exit 2 ;;
esac
repo_url="${GRAPHRT_REPO_URL:-https://github.com/Graphrt/graphrt.git}"

if ! user="$(npm whoami 2>/dev/null)" || [ -z "$user" ]; then
  echo "first-publish: npm whoami failed. Run \`npm login\` with the account that will own graphrt, then try again." >&2
  exit 1
fi
echo "npm user: $user"

if npm view graphrt version >/dev/null 2>&1; then
  echo "first-publish: graphrt is on npm already. Later versions are published by the release workflow." >&2
  exit 1
fi

if [ -z "$(git ls-remote --tags "$repo_url" "refs/tags/$tag")" ]; then
  echo "first-publish: $tag is not a tag of $repo_url" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
echo "cloning $tag of $repo_url"
git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$tag" "$repo_url" "$work/graphrt"
checkout="$work/graphrt"
if [ "$(git -C "$checkout" describe --tags --exact-match HEAD)" != "$tag" ]; then
  echo "first-publish: the clone is not at the tag $tag" >&2
  exit 1
fi
manifest="$checkout/web/packages/graphrt/package.json"
if [ ! -f "$manifest" ] || ! grep -q '"name": "graphrt"' "$manifest"; then
  echo "first-publish: $tag has no web/packages/graphrt: tag a release that has it" >&2
  exit 1
fi

cd "$checkout/web"
npm ci --no-audit --no-fund
if [ "$dry_run" = true ]; then
  node scripts/publish.mjs --pack --version "${tag#v}"
  mkdir -p "${TMPDIR:-/tmp}/graphrt-first-publish"
  cp .pack/graphrt-*.tgz "${TMPDIR:-/tmp}/graphrt-first-publish/"
  echo "dry run: nothing published, the package is in ${TMPDIR:-/tmp}/graphrt-first-publish"
else
  node scripts/publish.mjs --execute --version "${tag#v}"
  echo "Next: on npmjs.com, graphrt, Settings, Trusted publishing, add GitHub Actions:"
  echo "  organization Graphrt, repository graphrt, workflow release.yml, environment npm."
  echo "Then set the repository variable PUBLISH_NPM to true."
fi
