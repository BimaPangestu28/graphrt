#!/usr/bin/env sh
# Publish every workspace crate to crates.io in dependency order.
#
#   scripts/publish.sh             # dry run (default): package and verify, upload nothing
#   scripts/publish.sh --execute   # really publish (needs CARGO_REGISTRY_TOKEN or `cargo login`)
#
# Options:
#   --execute         publish for real
#   --delay SECONDS   wait between crates so the index catches up (default 30)
#   --from CRATE      resume from this crate (skip earlier ones)
#
# Environment: PUBLISH_RETRY_WAIT / PUBLISH_RETRY_LIMIT (rate-limit retries),
# PUBLISH_VERIFY=0 (skip verification builds). Versions already on crates.io
# are skipped, so re-running a release is safe.
#
# Crates with `publish = false` are skipped. The order is computed from
# `cargo metadata`, so new crates are picked up automatically. Requires python3.
set -eu

execute=0
delay=30
from=""
while [ $# -gt 0 ]; do
    case "$1" in
        --execute) execute=1 ;;
        --delay) shift; delay="$1" ;;
        --from) shift; from="$1" ;;
        -h | --help) sed -n '2,19p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

cd "$(dirname "$0")/.."

# shellcheck disable=SC2016 # the single-quoted program is Python, not shell
order=$(cargo metadata --format-version 1 --no-deps | python3 -c '
import json, sys
meta = json.load(sys.stdin)
pkgs = {p["name"]: p for p in meta["packages"] if p.get("publish") != []}
deps = {
    # Dev-dependencies count too: `cargo publish` keeps versioned dev-dependencies
    # in the manifest and requires them to exist on crates.io already.
    n: sorted({d["name"] for d in p["dependencies"] if d["name"] in pkgs and d["name"] != n})
    for n, p in pkgs.items()
}
order, seen = [], set()
def visit(n):
    if n in seen:
        return
    seen.add(n)
    for d in deps[n]:
        visit(d)
    order.append(n)
for n in sorted(deps):
    visit(n)
print(" ".join(order))
')

# crates.io rate-limits brand-new crates (a small burst, then one every few
# minutes), so a first release of the whole workspace hits HTTP 429 part way.
# Wait and retry instead of failing; other errors still stop the script.
retry_wait="${PUBLISH_RETRY_WAIT:-600}"
# PUBLISH_VERIFY=0 skips cargo's per-crate verification build (only after the
# whole workspace has been built and tested from this exact checkout).
verify_flag=""
[ "${PUBLISH_VERIFY:-1}" = "0" ] && verify_flag="--no-verify"
retry_limit="${PUBLISH_RETRY_LIMIT:-12}"
# Seconds until the time crates.io names ("Please try again after Fri, 27 Nov
# 2026 22:34:11 GMT"), plus a margin, or $retry_wait when it names none or GNU
# date cannot read it.
retry_after() {
    when=$(sed -n 's/.*try again after \([^.]*GMT\).*/\1/p' "$1" | head -n 1)
    if [ -n "$when" ] && until=$(date -u -d "$when" +%s 2>/dev/null); then
        seconds=$((until - $(date -u +%s) + 15))
        [ "$seconds" -lt 15 ] && seconds=15
        echo "$seconds"
    else
        echo "$retry_wait"
    fi
}
publish_with_retry() {
    attempt=1
    while :; do
        log=$(mktemp)
        # cargo's own exit status (not a pipe's), and plain text in the log:
        # with CARGO_TERM_COLOR=always (as in CI) colour codes hid `error:`
        # from the checks below, so a refused crate counted as published.
        # shellcheck disable=SC2086 # $verify_flag is empty or one flag
        if cargo publish -p "$1" --locked --color never $verify_flag >"$log" 2>&1; then
            status=0
        else
            status=1
        fi
        cat "$log"
        if [ "$status" -eq 0 ]; then
            rm -f "$log"
            return 0
        fi
        # Re-running a release (e.g. the tag workflow after a manual publish)
        # must not fail on versions that are already on crates.io.
        if grep -qiE "already (exists|uploaded)" "$log"; then
            rm -f "$log"
            echo "    $1 is already published at this version; skipping"
            return 0
        fi
        if grep -qiE "429|too many" "$log" && [ "$attempt" -lt "$retry_limit" ]; then
            wait_for=$(retry_after "$log")
            rm -f "$log"
            echo "    rate limited by crates.io; retrying $1 in ${wait_for}s (attempt $attempt/$retry_limit)"
            sleep "$wait_for"
            attempt=$((attempt + 1))
            continue
        fi
        rm -f "$log"
        echo "publishing $1 failed; fix it and resume with: scripts/publish.sh --execute --from $1" >&2
        return 1
    done
}

echo "publish order: $order"
started=0
[ -z "$from" ] && started=1

for crate in $order; do
    if [ "$started" -eq 0 ]; then
        if [ "$crate" = "$from" ]; then started=1; else echo "skip $crate (before --from)"; continue; fi
    fi
    if [ "$execute" -eq 1 ]; then
        echo "==> publishing $crate"
        publish_with_retry "$crate"
        echo "    waiting ${delay}s for the index"
        sleep "$delay"
    else
        # Only crates without unpublished workspace dependencies can be fully verified
        # against crates.io; the rest are packaged together below.
        if cargo publish -p "$crate" --dry-run --allow-dirty --locked >/dev/null 2>&1; then
            echo "==> dry run ok (verified): $crate"
        else
            echo "==> dry run: $crate needs unpublished workspace crates; packaged below"
        fi
    fi
done

if [ "$execute" -eq 0 ]; then
    # Workspace packaging resolves sibling crates locally, checking manifests,
    # included files and package sizes for every crate.
    cargo package --workspace --no-verify --allow-dirty --locked
    echo "dry run complete — re-run with --execute to publish"
fi
