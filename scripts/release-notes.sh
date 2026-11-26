#!/bin/sh
# Print the GitHub Release body for one version: its CHANGELOG.md section (summary
# paragraph, then ### Added / ### Changed / ...), the install block from
# .github/release-notes/install.md and a "Full changelog" line.
#
# Usage: scripts/release-notes.sh <vX.Y.Z> [changelog-path]
#   RELEASE_REPO overrides the repository the links point at (default Graphrt/graphrt).
# Exits 1 if the version has no section or the section is empty.
# Used by .github/workflows/release.yml to fill the release body.
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 <vX.Y.Z> [changelog-path]" >&2
    exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd)
tag="v${1#v}"
version="${tag#v}"
changelog="${2:-$root/CHANGELOG.md}"
template="$root/.github/release-notes/install.md"
repo="${RELEASE_REPO:-Graphrt/graphrt}"
owner=$(printf '%s' "${repo%%/*}" | tr '[:upper:]' '[:lower:]')

[ -f "$changelog" ] || { echo "error: changelog not found: $changelog" >&2; exit 1; }
[ -f "$template" ] || { echo "error: install template not found: $template" >&2; exit 1; }

# Section = lines after "## [<version>]" up to the next "## " heading or the link
# reference block. The next "## [x.y.z]" heading names the previous version.
# Leading and trailing blank lines are trimmed.
section_file=$(mktemp)
trap 'rm -f "$section_file" "$section_file.prev"' EXIT INT TERM
if ! awk -v heading="## [${version}]" -v out="$section_file" '
    index($0, heading) == 1 { in_section = 1; found = 1; next }
    in_section && /^## \[/ {
        prev = $0; sub(/^## \[/, "", prev); sub(/\].*/, "", prev); in_section = 0
    }
    in_section && (/^## / || /^\[[^]]+\]: /) { in_section = 0 }
    in_section { lines[++count] = $0 }
    END {
        if (!found) exit 3
        first = 1; while (first <= count && lines[first] ~ /^[[:space:]]*$/) first++
        last = count; while (last >= first && lines[last] ~ /^[[:space:]]*$/) last--
        if (first > last) exit 4
        for (i = first; i <= last; i++) print lines[i] > out
        print prev
    }
' "$changelog" > "$section_file.prev"; then
    echo "error: no non-empty \"## [${version}]\" section in $changelog" >&2
    exit 1
fi
prev=$(cat "$section_file.prev")

cat "$section_file"
printf '\n'
sed -e "s|{{REPO}}|$repo|g" -e "s|{{OWNER}}|$owner|g" -e "s|{{TAG}}|$tag|g" -e "s|{{VERSION}}|$version|g" "$template"
printf '\n'
anchor="$(printf '%s' "$version" | tr -d .)---$(grep -m1 "^## \[${version}\] - " "$changelog" | sed 's/^.* - //')"
line="**Full changelog**: [CHANGELOG.md](https://github.com/$repo/blob/$tag/CHANGELOG.md#$anchor)"
if [ -n "$prev" ]; then
    line="$line · [v$prev...$tag](https://github.com/$repo/compare/v$prev...$tag)"
fi
printf '%s\n' "$line"
