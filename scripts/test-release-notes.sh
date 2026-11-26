#!/bin/sh
# Checks scripts/release-notes.sh against a fixture CHANGELOG and the real one (no network).
#   scripts/test-release-notes.sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
script="$root/scripts/release-notes.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

fail() { echo "FAIL: $*" >&2; exit 1; }

sh -n "$script"
if command -v shellcheck >/dev/null 2>&1; then
    shellcheck -s sh "$script" "$0"
fi

cat > "$work/CHANGELOG.md" <<'MD'
# Changelog

## [Unreleased]

## [1.2.0] - 2027-01-02

Summary of 1.2.0.

### Added
- A thing.

## [1.1.0] - 2026-12-01

### Fixed
- Another thing.

## [1.0.0] - 2026-11-01

## [0.9.0] - 2026-10-01

### Fixed
- Old.

[1.2.0]: https://example.invalid/compare/v1.1.0...v1.2.0
MD

out=$("$script" v1.2.0 "$work/CHANGELOG.md")
printf '%s\n' "$out" | head -n 1 | grep -qx 'Summary of 1.2.0.' || fail "section does not start with the summary"
printf '%s\n' "$out" | grep -qx -- '- A thing.' || fail "section body missing"
printf '%s\n' "$out" | grep -q 'Another thing' && fail "next section leaked into the notes"
printf '%s\n' "$out" | grep -qx '### Install' || fail "install block missing"
printf '%s\n' "$out" | grep -q 'releases/download/v1.2.0/install.sh' || fail "tag not filled in"
printf '%s\n' "$out" | grep -q 'ghcr.io/graphrt/graphrt:1.2.0' || fail "image not lower case or version not filled in"
printf '%s\n' "$out" | grep -q 'brew install graphrt/graphrt/graphrt' || fail "brew tap wrong"
printf '%s\n' "$out" | grep -q '{{' && fail "placeholder left in output"
printf '%s\n' "$out" | tail -n 1 | grep -qF 'CHANGELOG.md#120---2027-01-02) · [v1.1.0...v1.2.0](https://github.com/Graphrt/graphrt/compare/v1.1.0...v1.2.0)' \
    || fail "full changelog line wrong"

# A version without "v" works, and the oldest version gets no compare link.
"$script" 0.9.0 "$work/CHANGELOG.md" | tail -n 1 | grep -q 'compare' && fail "oldest version has a compare link"

# Missing and empty sections are errors.
if "$script" v3.0.0 "$work/CHANGELOG.md" > /dev/null 2>&1; then fail "missing section accepted"; fi
if "$script" v1.0.0 "$work/CHANGELOG.md" > /dev/null 2>&1; then fail "empty section accepted"; fi

# RELEASE_REPO moves every link and the image.
RELEASE_REPO=Example/fork "$script" v1.2.0 "$work/CHANGELOG.md" | grep -q 'ghcr.io/example/graphrt:1.2.0' || fail "RELEASE_REPO ignored"

# The real CHANGELOG renders its newest released version.
latest=$(grep -m1 -E '^## \[[0-9]' "$root/CHANGELOG.md" | sed 's/^## \[\([^]]*\)\].*/\1/')
"$script" "v$latest" > /dev/null || fail "real CHANGELOG.md $latest does not render"

echo "release notes: ok"
