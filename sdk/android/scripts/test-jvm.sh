#!/usr/bin/env bash
# Runs the Kotlin tests of the SDK and the Kotlin snippets of docs/mobile.md on
# the JVM, against a host build of libgraphrt_ffi with the SDK's features.
#
# Usage: sdk/android/scripts/test-jvm.sh
# Needs a JDK 17+ (JAVA_HOME or java on PATH); Gradle comes from the wrapper.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
sdk_dir="$(cd "$script_dir/.." && pwd)"
repo_root="$(cd "$sdk_dir/../.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"

cd "$repo_root"
cargo build -p graphrt-ffi --lib --features sdk
python3 scripts/check-doc-snippets.py

cd "$sdk_dir"
GRAPHRT_LIB_DIR="$target_dir/debug" ./gradlew --no-daemon :graphrt-jvm:test
