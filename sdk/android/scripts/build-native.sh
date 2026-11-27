#!/usr/bin/env bash
# Cross-compiles libgraphrt_ffi.so for the SDK's ABIs into graphrt/src/main/jniLibs,
# where the AAR (and the sample app, which depends on :graphrt) picks them up.
#
# Usage: sdk/android/scripts/build-native.sh
#
# Environment (all optional):
#   ANDROID_NDK_HOME   NDK to use, r27 to r29 (default: newest under $ANDROID_HOME/ndk or
#                      ~/Library/Android/sdk/ndk)
#   GRAPHRT_ABIS       space-separated ABIs (default: "arm64-v8a armeabi-v7a x86_64")
#   GRAPHRT_FEATURES   cargo feature flags (default: "--features sdk", what the release
#                      AAR ships; needs libclang for the JavaScript code node)
#   GRAPHRT_PROFILE    cargo profile (default: min-size)
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
sdk_dir="$(cd "$script_dir/.." && pwd)"
repo_root="$(cd "$sdk_dir/../.." && pwd)"
jni_libs_dir="$sdk_dir/graphrt/src/main/jniLibs"

abis="${GRAPHRT_ABIS:-arm64-v8a armeabi-v7a x86_64}"
features="${GRAPHRT_FEATURES:---features sdk}"
profile="${GRAPHRT_PROFILE:-min-size}"

find_newest_ndk() {
  local sdk_root="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
  local ndk_parent="$sdk_root/ndk"
  if [[ ! -d "$ndk_parent" ]]; then
    return 1
  fi
  local newest
  # The newest below r30 (see the NDK check below).
  newest="$(ls -1 "$ndk_parent" | grep -E '^(2[7-9])\.' | sort -V | tail -n 1)"
  [[ -n "$newest" ]] && echo "$ndk_parent/$newest"
}

if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
  if ! ANDROID_NDK_HOME="$(find_newest_ndk)"; then
    echo "error: no NDK found; install one with sdkmanager 'ndk;28.2.13676358' or set ANDROID_NDK_HOME" >&2
    exit 1
  fi
fi
export ANDROID_NDK_HOME

if ! command -v cargo-ndk >/dev/null 2>&1; then
  echo "error: cargo-ndk is not installed; run: cargo install cargo-ndk" >&2
  exit 1
fi

# NDK r30 refuses the bare Rust triple that bindgen (the QuickJS bindings of the
# JavaScript code node) passes to clang ("Unversioned target triples are not
# supported"), and cargo-ndk replaces any bindgen arguments set here. Until
# cargo-ndk passes a versioned triple, build with r27 to r29.
ndk_major="$(sed -n 's/^Pkg.Revision *= *\([0-9]*\).*/\1/p' "$ANDROID_NDK_HOME/source.properties" 2>/dev/null || true)"
if [[ -n "$ndk_major" ]] && ((ndk_major >= 30)) && [[ "$features" != *no-default-features* ]]; then
  echo "error: NDK r$ndk_major cannot build the JavaScript code node yet; use r27 to r29 (ANDROID_NDK_HOME)" >&2
  exit 1
fi

target_flags=()
for abi in $abis; do
  target_flags+=(-t "$abi")
done

echo "NDK:      $ANDROID_NDK_HOME"
echo "ABIs:     $abis"
echo "features: $features"
echo "profile:  $profile"

cd "$repo_root"
# shellcheck disable=SC2086 # $features is a list of flags on purpose
cargo ndk "${target_flags[@]}" -o "$jni_libs_dir" \
  build -p graphrt-ffi --lib --profile "$profile" $features

echo
echo "Built libraries:"
for abi in $abis; do
  ls -l "$jni_libs_dir/$abi/libgraphrt_ffi.so"
done
