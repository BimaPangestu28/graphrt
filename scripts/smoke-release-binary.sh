#!/bin/sh
# Smoke test for a packaged release binary: it starts, reports the workspace
# version and the release features (`SMOKE_REQUIRED_FEATURES`, default
# `eval online-eval deploy-fly`; with `gateway`, also the gateway command;
# with `deploy-fly`, the direct deploy refusing to run without a Fly token),
# validates and runs the offline hello workflow.
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

# Release builds add the opt-in features in release.yml's EXTRA_FEATURES.
features_output=$("$binary" version | tr -d '\r') || fail "\`version\` exited with $?"
for feature in ${SMOKE_REQUIRED_FEATURES-eval online-eval deploy-fly}; do
    case "$features_output" in
        *" $feature,"* | *" $feature)"*) ;;
        *) fail "\`version\` printed \`$features_output\`, expected the \`$feature\` feature" ;;
    esac
done

# The gateway build (`SMOKE_REQUIRED_FEATURES` names `gateway`) also has the
# `gateway` and `deploy` commands.
case " ${SMOKE_REQUIRED_FEATURES-} " in
    *" gateway "*)
        "$binary" gateway --help >/dev/null || fail "\`gateway --help\` exited with $?"
        "$binary" deploy --help >/dev/null || fail "\`deploy --help\` exited with $?"
        ;;
esac

# The direct deploy (desktop "Put it online") answers in JSON Lines even
# without a token: one `missing_token` error line, exit 1, no network call.
case " ${SMOKE_REQUIRED_FEATURES-eval online-eval deploy-fly} " in
    *" deploy-fly "*)
        direct_output=$(env -u FLY_API_TOKEN "$binary" deploy --target fly --direct --deployment smoke --version v1 \
            --json "$workflow" 2>/dev/null | tr -d '\r') || true
        case "$direct_output" in
            '{"type":"error","code":"missing_token"'*) ;;
            *) fail "\`deploy --direct --json\` printed \`$direct_output\`, expected a missing_token error line" ;;
        esac
        ;;
esac

"$binary" validate "$workflow" >/dev/null || fail "\`validate $workflow\` exited with $?"

# Strip CR so the comparison also holds for Windows console output.
run_output=$("$binary" run "$workflow" --input name=Ada | tr -d '\r') || fail "\`run $workflow\` exited with $?"
case "$run_output" in
    *'"greeting"'*'Hello, Ada.'*) ;;
    *) fail "\`run\` printed \`$run_output\`, expected a greeting for Ada" ;;
esac

echo "smoke test passed: $version_output"
