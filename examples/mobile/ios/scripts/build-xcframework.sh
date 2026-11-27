#!/usr/bin/env bash
# Builds GraphrtFFI.xcframework for the sample app (iOS device + simulator) with
# the Swift package's script.
#
# Usage: examples/mobile/ios/scripts/build-xcframework.sh
# Output: examples/mobile/ios/build/GraphrtFFI.xcframework
#
# Environment (all optional), passed on to sdk/swift/scripts/build-xcframework.sh:
#   GRAPHRT_TARGETS    space-separated Rust targets (default: "aarch64-apple-ios aarch64-apple-ios-sim")
#   GRAPHRT_FEATURES   cargo feature flags (default: "--features sdk")
#   GRAPHRT_PROFILE    cargo profile (default: min-size)
#   IPHONEOS_DEPLOYMENT_TARGET  minimum iOS version (default: 16.0, matches the app)
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_dir="$(cd "$script_dir/.." && pwd)"
repo_root="$(cd "$app_dir/../../.." && pwd)"
export GRAPHRT_TARGETS="${GRAPHRT_TARGETS:-aarch64-apple-ios aarch64-apple-ios-sim}"
mkdir -p "$app_dir/build"
exec "$repo_root/sdk/swift/scripts/build-xcframework.sh" "$app_dir/build/GraphrtFFI.xcframework"
