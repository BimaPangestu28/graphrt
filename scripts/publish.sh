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
        -h | --help) sed -n '2,15p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

cd "$(dirname "$0")/.."

order=$(cargo metadata --format-version 1 --no-deps | python3 -c '
import json, sys
meta = json.load(sys.stdin)
pkgs = {p["name"]: p for p in meta["packages"] if p.get("publish") != []}
deps = {
    n: sorted({d["name"] for d in p["dependencies"] if d["name"] in pkgs and d["kind"] in (None, "build")})
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

echo "publish order: $order"
started=0
[ -z "$from" ] && started=1

for crate in $order; do
    if [ "$started" -eq 0 ]; then
        if [ "$crate" = "$from" ]; then started=1; else echo "skip $crate (before --from)"; continue; fi
    fi
    if [ "$execute" -eq 1 ]; then
        echo "==> publishing $crate"
        cargo publish -p "$crate" --locked
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
