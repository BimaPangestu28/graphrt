#!/bin/sh
# graphrt installer — run your agent everywhere.
#
#   curl -fsSL https://raw.githubusercontent.com/BimaPangestu28/graphrt/main/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version v0.1.0 --dir /usr/local/bin
#   curl -fsSL .../install.sh | sh -s -- --uninstall
#
# Options:
#   --version TAG   release tag to install (default: latest)
#   --dir DIR       install directory (default: ~/.local/bin, or /usr/local/bin when root)
#   --uninstall     remove an installed graphrt binary from --dir
#   -h, --help      show this help
#
# Environment: GRAPHRT_REPO (owner/repo, default below), GRAPHRT_VERSION, GRAPHRT_INSTALL_DIR,
#   GRAPHRT_DOWNLOAD_BASE (mirror URL or file:// directory with the archives and SHA256SUMS).
# Downloads a release archive, verifies it against the release's SHA256SUMS and
# installs the single `graphrt` binary. Never runs sudo or prompts.
set -eu

REPO="${GRAPHRT_REPO:-BimaPangestu28/graphrt}"
BIN="graphrt"
version="${GRAPHRT_VERSION:-latest}"
dir="${GRAPHRT_INSTALL_DIR:-}"
uninstall=0

say() { printf '%s\n' "graphrt-install: $*" >&2; }
die() { say "error: $*"; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a value"; version="$2"; shift ;;
        --dir) [ $# -ge 2 ] || die "--dir needs a value"; dir="$2"; shift ;;
        --uninstall) uninstall=1 ;;
        -h | --help) sed -n '2,17p' "$0" 2>/dev/null || true; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
    shift
done

if [ -z "$dir" ]; then
    if [ "$(id -u)" = "0" ]; then dir="/usr/local/bin"; else dir="${HOME}/.local/bin"; fi
fi

if [ "$uninstall" -eq 1 ]; then
    if [ -e "$dir/$BIN" ]; then
        rm -f "$dir/$BIN" || die "cannot remove $dir/$BIN (try --dir or run with permissions for it)"
        say "removed $dir/$BIN"
    else
        say "nothing to remove: $dir/$BIN does not exist"
    fi
    exit 0
fi

need() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }
need uname
need tar
need mkdir

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q --https-only -O "$2" "$1"; }
else
    die "need curl or wget"
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    die "need sha256sum or shasum to verify the download"
fi

os=$(uname -s)
arch=$(uname -m)
case "$arch" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) die "unsupported CPU architecture: $arch" ;;
esac
case "$os" in
    Linux) target="${arch}-unknown-linux-musl" ;;
    Darwin) target="${arch}-apple-darwin" ;;
    *) die "unsupported OS: $os (Windows: download the .zip from the releases page)" ;;
esac

if [ -n "${GRAPHRT_DOWNLOAD_BASE:-}" ]; then
    # Testing / mirrors: a directory URL holding the archive and SHA256SUMS.
    base="$GRAPHRT_DOWNLOAD_BASE"
    case "$base" in
        file://*) fetch() { cp "${1#file://}" "$2"; } ;;
    esac
elif [ "$version" = "latest" ]; then
    base="https://github.com/${REPO}/releases/latest/download"
else
    case "$version" in v*) ;; *) version="v${version}" ;; esac
    base="https://github.com/${REPO}/releases/download/${version}"
fi
archive="${BIN}-${target}.tar.gz"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t graphrt)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading ${archive} (${version})"
fetch "${base}/${archive}" "${tmp}/${archive}" || die "download failed: ${base}/${archive}"
fetch "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" || die "download failed: ${base}/SHA256SUMS"

expected=$(grep " ${archive}\$" "${tmp}/SHA256SUMS" | cut -d ' ' -f 1 | head -n 1)
[ -n "$expected" ] || die "no checksum for ${archive} in SHA256SUMS"
actual=$(sha256 "${tmp}/${archive}")
[ "$expected" = "$actual" ] || die "checksum mismatch for ${archive} (expected ${expected}, got ${actual})"
say "checksum ok"

tar -xzf "${tmp}/${archive}" -C "$tmp"
src=$(find "$tmp" -type f -name "$BIN" | head -n 1)
[ -n "$src" ] || die "archive does not contain a ${BIN} binary"

mkdir -p "$dir" 2>/dev/null || die "cannot create ${dir}; choose another with --dir"
[ -w "$dir" ] || die "${dir} is not writable; re-run with --dir \"\$HOME/.local/bin\" or as a user who can write it"
cp "$src" "${dir}/${BIN}.tmp"
chmod 755 "${dir}/${BIN}.tmp"
mv -f "${dir}/${BIN}.tmp" "${dir}/${BIN}"
say "installed ${dir}/${BIN}"

case ":${PATH}:" in
    *":${dir}:"*) "${dir}/${BIN}" version || true ;;
    *) say "note: ${dir} is not on your PATH; add it, e.g. export PATH=\"${dir}:\$PATH\"" ;;
esac
