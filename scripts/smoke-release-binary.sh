#!/bin/sh
# Smoke test for a packaged release binary: it starts, reports the workspace
# version, validates and runs the offline hello workflow.
#   scripts/smoke-release-binary.sh dist/graphrt-x86_64-unknown-linux-musl/graphrt
# Used by .github/workflows/release.yml; POSIX sh (Git Bash on Windows).
set -eu

binary=${1:?usage: smoke-release-binary.sh PATH_TO_GRAPHRT}
root=$(cd "$(dirname "$0")/.." && pwd)
workflow="$root/examples/hello.yaml"

fail() {
    echo "smoke test failed: $*" >&2
    exit 1
}

expected_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
version_output=$("$binary" --version) || fail "\`$binary --version\` exited with $?"
case "$version_output" in
    *"$expected_version"*) ;;
    *) fail "\`--version\` printed \`$version_output\`, expected version $expected_version" ;;
esac

"$binary" validate "$workflow" >/dev/null || fail "\`validate $workflow\` exited with $?"

# Strip CR so the comparison also holds for Windows console output.
run_output=$("$binary" run "$workflow" --input name=Ada | tr -d '\r') || fail "\`run $workflow\` exited with $?"
case "$run_output" in
    *'"greeting"'*'Hello, Ada.'*) ;;
    *) fail "\`run\` printed \`$run_output\`, expected a greeting for Ada" ;;
esac

echo "smoke test passed: $version_output"
