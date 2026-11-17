#!/bin/sh
# Smoke test for install.sh against a fake local release (no network).
#   scripts/test-install.sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

sh -n "$root/install.sh"
if command -v shellcheck >/dev/null 2>&1; then
    shellcheck -s sh "$root/install.sh" "$root/scripts/publish.sh" "$0"
fi

case "$(uname -m)" in x86_64 | amd64) arch=x86_64 ;; *) arch=aarch64 ;; esac
case "$(uname -s)" in Darwin) target="${arch}-apple-darwin" ;; *) target="${arch}-unknown-linux-musl" ;; esac
archive="graphrt-${target}.tar.gz"

mkdir -p "$work/release/graphrt-${target}" "$work/bin"
printf '#!/bin/sh\necho "graphrt fake 0.0.0"\n' > "$work/release/graphrt-${target}/graphrt"
chmod 755 "$work/release/graphrt-${target}/graphrt"
(cd "$work/release" && tar -czf "$archive" "graphrt-${target}")
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$work/release" && sha256sum "$archive" > SHA256SUMS)
else
    (cd "$work/release" && shasum -a 256 "$archive" > SHA256SUMS)
fi

GRAPHRT_DOWNLOAD_BASE="file://$work/release" sh "$root/install.sh" --dir "$work/bin"
"$work/bin/graphrt" | grep -q "graphrt fake" || { echo "installed binary does not run" >&2; exit 1; }

# A tampered archive must be rejected.
echo "tampered" >> "$work/release/$archive"
if GRAPHRT_DOWNLOAD_BASE="file://$work/release" sh "$root/install.sh" --dir "$work/bin2" 2>/dev/null; then
    echo "checksum mismatch was not detected" >&2
    exit 1
fi

sh "$root/install.sh" --uninstall --dir "$work/bin"
[ ! -e "$work/bin/graphrt" ] || { echo "uninstall left the binary" >&2; exit 1; }
echo "install.sh: all checks passed"
