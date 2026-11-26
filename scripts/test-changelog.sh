#!/usr/bin/env bash
# Checks scripts/changelog.py (changelog fragments) against a fixture and the
# real changelog.d/ (no network).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="$here/changelog.py"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fail() { echo "test-changelog: FAIL: $*" >&2; exit 1; }

# The real fragments are valid.
python3 "$script" check > /dev/null || fail "changelog.d/ has an invalid fragment"

mkdir -p "$work/changelog.d"
cat > "$work/CHANGELOG.md" <<'MD'
# Changelog

Intro.

## [Unreleased]

### Added

- Existing added entry.

### Fixed

- Existing fix,
  on two lines.

## [1.0.0] - 2027-01-01

### Added

- First release.
MD
printf -- '- A new feature.\n' > "$work/changelog.d/new-feature.added.md"
printf -- '- A breaking change,\n  explained.\n' > "$work/changelog.d/api.changed.md"
printf -- '- Another fix.\n' > "$work/changelog.d/b-fix.fixed.md"
printf 'How to write fragments.\n' > "$work/changelog.d/README.md"

run() { CHANGELOG_ROOT="$work" python3 "$script" "$@"; }

run check > /dev/null || fail "valid fixture refused"
preview=$(run preview)
expected='## [Unreleased]

### Added

- Existing added entry.
- A new feature.

### Changed

- A breaking change,
  explained.

### Fixed

- Existing fix,
  on two lines.
- Another fix.'
[ "$preview" = "$expected" ] || fail "preview:
$preview"

run release 1.1.0 2027-02-03 > /dev/null || fail "release failed"
grep -q '^## \[Unreleased\]$' "$work/CHANGELOG.md" || fail "release dropped the Unreleased heading"
release_section=$(awk '/^## \[1.1.0\] - 2027-02-03$/{f=1;next} /^## \[1.0.0\]/{f=0} f' "$work/CHANGELOG.md")
[ "$(printf '%s\n' "$release_section" | sed '/./,$!d')" = "$(printf '%s\n' "$expected" | sed '1,2d')" ] \
  || fail "release section:
$release_section"
grep -q '^- First release\.$' "$work/CHANGELOG.md" || fail "older release lost"
[ -z "$(find "$work/changelog.d" -name '*.*.md')" ] || fail "fragments were not removed"
[ -f "$work/changelog.d/README.md" ] || fail "README.md was removed"
if run release 1.2.0 2027-03-01 > /dev/null 2>&1; then fail "empty release accepted"; fi

# Bad fragments are refused with a reason.
bad() {
  local name="$1" content="$2" why="$3"
  rm -f "$work/changelog.d/"*.*.md
  printf '%b' "$content" > "$work/changelog.d/$name"
  if run check > /dev/null 2>&1; then fail "accepted: $why"; fi
}
bad "feature.md" '- x\n' "a name without a kind"
bad "feature.improved.md" '- x\n' "an unknown kind"
bad "feature.added.md" '' "an empty fragment"
bad "feature.added.md" 'Not a bullet.\n' "prose instead of a bullet"
bad "feature.added.md" '- x\nnot indented\n' "an unindented continuation line"
bad "Feature.added.md" '- x\n' "an upper-case name"
rm -f "$work/changelog.d/"*.*.md
if run release nope > /dev/null 2>&1; then fail "a bad version was accepted"; fi

echo "test-changelog: ok"
