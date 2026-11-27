#!/usr/bin/env bash
# Regenerates the UniFFI bindings in bindings/ (Kotlin, Swift, Python) from a
# debug build of graphrt-ffi. Features do not change the exported API; the build
# uses the SDK's features (`sdk`) so it is the library the SDK tests load too.
#
# Usage: scripts/generate-bindings.sh [--check]
#   --check  regenerate into a temporary directory and fail when bindings/ differs
#            (what CI runs, so a change to the API cannot land with stale bindings)
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"
check=false
if [[ "${1:-}" == "--check" ]]; then
  check=true
elif [[ $# -gt 0 ]]; then
  echo "usage: scripts/generate-bindings.sh [--check]" >&2
  exit 2
fi

target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
cargo build -q -p graphrt-ffi --lib --features sdk
case "$(uname -s)" in
  Darwin) lib="$target_dir/debug/libgraphrt_ffi.dylib" ;;
  *) lib="$target_dir/debug/libgraphrt_ffi.so" ;;
esac

out_root="$repo_root/bindings"
if $check; then
  out_root="$(mktemp -d)"
  trap 'rm -rf "$out_root"' EXIT
fi
for lang in kotlin swift python; do
  cargo run -q -p graphrt-ffi --features bindgen-cli --bin uniffi-bindgen -- \
    generate --library "$lib" --language "$lang" --out-dir "$out_root/$lang" >/dev/null
done

if $check; then
  stale=0
  for file in kotlin/uniffi/graphrt_ffi/graphrt_ffi.kt swift/graphrt_ffi.swift swift/graphrt_ffiFFI.h \
    swift/graphrt_ffiFFI.modulemap python/graphrt_ffi.py; do
    if ! diff -q "$out_root/$file" "bindings/$file" >/dev/null; then
      echo "stale: bindings/$file" >&2
      stale=1
    fi
  done
  if ((stale)); then
    echo "the bindings do not match the API; run scripts/generate-bindings.sh and commit bindings/" >&2
    exit 1
  fi
  echo "bindings are up to date"
fi
