#!/bin/sh
# Smoke test for a packaged release binary: it starts, reports the workspace
# version and the release features (`SMOKE_REQUIRED_FEATURES`, default
# `eval online-eval skills learning deploy-fly`; with `gateway`, also the
# gateway command; with `deploy-fly`, the direct deploy refusing to run without
# a Fly token; with `skills` and `learning`, a skill installed and a lesson
# added in scratch stores), validates and runs the offline hello workflow.
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
default_features="eval online-eval skills learning deploy-fly deploy-cloud-run deploy-aws"
required_features=${SMOKE_REQUIRED_FEATURES-$default_features}
features_output=$("$binary" version | tr -d '\r') || fail "\`version\` exited with $?"
for feature in $required_features; do
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
case " $required_features " in
    *" deploy-fly "*)
        direct_output=$(env -u FLY_API_TOKEN "$binary" deploy --target fly --direct --deployment smoke --version v1 \
            --json "$workflow" 2>/dev/null | tr -d '\r') || true
        case "$direct_output" in
            '{"type":"error","code":"missing_token"'*) ;;
            *) fail "\`deploy --direct --json\` printed \`$direct_output\`, expected a missing_token error line" ;;
        esac
        ;;
esac

# The Lambda direct deploy refuses a missing profile in JSON Lines, before any network call.
case " $required_features " in
    *" deploy-aws "*)
        lambda_output=$(env -u AWS_ACCESS_KEY_ID -u AWS_PROFILE "$binary" deploy --target lambda --direct \
            --deployment smoke --version v1 --region us-east-1 --profile graphrt-smoke-surely-missing \
            --json "$workflow" 2>/dev/null | tr -d '\r') || true
        case "$lambda_output" in
            '{"type":"error","code":"missing_credentials"'*) ;;
            *) fail "\`deploy --target lambda --direct --json\` printed \`$lambda_output\`, expected a missing_credentials error line" ;;
        esac
        ;;
esac

# Skills and self learning work end to end without a model or the network: a
# skill is installed into a scratch store and listed, a lesson is added and
# listed. The stores are scratch directories, never the user's own.
scratch=""
cleanup() {
    if [ -n "$scratch" ]; then rm -rf "$scratch"; fi
}
trap cleanup EXIT
case " $required_features " in
    *" skills "* | *" learning "*) scratch=$(mktemp -d) || fail "cannot create a scratch directory" ;;
esac
case " $required_features " in
    *" skills "*)
        mkdir -p "$scratch/smoke-skill"
        printf -- '---\nname: smoke-skill\ndescription: Checks that skills work. Use in the release smoke test.\n---\n# Smoke\n\nSay hello.\n' \
            > "$scratch/smoke-skill/SKILL.md"
        "$binary" skill install "$scratch/smoke-skill" --dir "$scratch/skills" >/dev/null \
            || fail "\`skill install\` exited with $?"
        skills_output=$("$binary" skill list --dir "$scratch/skills" | tr -d '\r') || fail "\`skill list\` exited with $?"
        case "$skills_output" in
            *smoke-skill*) ;;
            *) fail "\`skill list\` printed \`$skills_output\`, expected smoke-skill" ;;
        esac
        ;;
esac
case " $required_features " in
    *" learning "*)
        "$binary" learn add smoke-agent "Greet people by name." --dir "$scratch/learning" >/dev/null \
            || fail "\`learn add\` exited with $?"
        lessons_output=$("$binary" learn list smoke-agent --dir "$scratch/learning" | tr -d '\r') \
            || fail "\`learn list\` exited with $?"
        case "$lessons_output" in
            *"Greet people by name."*) ;;
            *) fail "\`learn list\` printed \`$lessons_output\`, expected the added lesson" ;;
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
