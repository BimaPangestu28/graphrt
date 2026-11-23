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

# --gateway installs the gateway archive of the same release.
gateway_archive="graphrt-gateway-${target}.tar.gz"
mkdir -p "$work/release/graphrt-gateway-${target}"
printf '#!/bin/sh\necho "graphrt fake gateway 0.0.0"\n' > "$work/release/graphrt-gateway-${target}/graphrt"
chmod 755 "$work/release/graphrt-gateway-${target}/graphrt"
(cd "$work/release" && tar -czf "$gateway_archive" "graphrt-gateway-${target}")
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$work/release" && sha256sum "$archive" "$gateway_archive" > SHA256SUMS)
else
    (cd "$work/release" && shasum -a 256 "$archive" "$gateway_archive" > SHA256SUMS)
fi
GRAPHRT_DOWNLOAD_BASE="file://$work/release" sh "$root/install.sh" --gateway --dir "$work/bin-gateway"
"$work/bin-gateway/graphrt" | grep -q "graphrt fake gateway" || { echo "--gateway did not install the gateway build" >&2; exit 1; }

# A tampered archive must be rejected.
echo "tampered" >> "$work/release/$archive"
if GRAPHRT_DOWNLOAD_BASE="file://$work/release" sh "$root/install.sh" --dir "$work/bin2" 2>/dev/null; then
    echo "checksum mismatch was not detected" >&2
    exit 1
fi

# A --version with a path in it would point the download at another repository's
# release (`v1/../../../../other/repo/releases/download/v1`); a plain-http mirror
# would be fetched without TLS by wget. Both are refused before any download.
expect_refusal() {
    reason="$1"
    shift
    if output=$("$@" 2>&1); then
        echo "install.sh accepted $reason" >&2
        exit 1
    fi
    case "$output" in
        *downloading*) echo "install.sh started a download for $reason: $output" >&2; exit 1 ;;
    esac
}
expect_refusal "a version with a path" \
    sh "$root/install.sh" --dir "$work/bin3" --version "v1/../../../../other/repo/releases/download/v1"
expect_refusal "a version with a query" env GRAPHRT_VERSION="v1?x=1" sh "$root/install.sh" --dir "$work/bin3"
expect_refusal "a plain-http mirror" env GRAPHRT_DOWNLOAD_BASE="http://127.0.0.1:9/release" \
    sh "$root/install.sh" --dir "$work/bin3"

sh "$root/install.sh" --uninstall --dir "$work/bin"
[ ! -e "$work/bin/graphrt" ] || { echo "uninstall left the binary" >&2; exit 1; }
echo "install.sh: all checks passed"
