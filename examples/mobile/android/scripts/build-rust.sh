#!/usr/bin/env bash
# Builds libgraphrt_ffi.so for the sample app's ABIs into the SDK module the app
# depends on (sdk/android/graphrt/src/main/jniLibs).
#
# Usage: examples/mobile/android/scripts/build-rust.sh
#
# Environment (all optional), passed on to sdk/android/scripts/build-native.sh:
#   ANDROID_NDK_HOME   NDK to use (default: newest under $ANDROID_HOME/ndk)
#   GRAPHRT_ABIS       space-separated ABIs (default: "arm64-v8a x86_64", what the app ships)
#   GRAPHRT_FEATURES   cargo feature flags (default: "--features sdk")
#   GRAPHRT_PROFILE    cargo profile (default: min-size)
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../../../.." && pwd)"
export GRAPHRT_ABIS="${GRAPHRT_ABIS:-arm64-v8a x86_64}"
exec "$repo_root/sdk/android/scripts/build-native.sh"
