#!/usr/bin/env bash
# Every measurable target of the gateway plan on Linux, against bench/budgets.toml:
#
#   1. sizes, idle RSS and exec -> /healthz of the agent and gateway builds
#      (scripts/budget-gate.py);
#   2. gateway run-time targets: TTFB overhead over a Unix socket and loopback
#      TCP, 1,000 concurrent SSE streams, cold wake (with and without a Node.js
#      plugin), webhook ack, cron lag (bench/gateway/gwbench.py);
#   3. RS256 JWT verification microbench (crates/graphrt-gateway/tests/jwt_verify_bench.rs);
#   4. SIGTERM drain of graphrt as PID 1 of a real container
#      (scripts/test-pid1-container.sh), else of a PID namespace (scripts/test-pid1.sh).
#
# Prints one table per part and exits 1 when any budget is missed.
#
#   bench/targets.sh                         # build here (cargo), then measure
#   bench/targets.sh --target x86_64-unknown-linux-musl --build-cmd "cargo zigbuild"
#   bench/targets.sh --bin-dir DIR           # measure prebuilt binaries (see below)
#   bench/targets.sh --quick                 # fewer samples, never fails (smoke run)
#
# A --bin-dir holds graphrt-<build> for every [[build]] of bench/budgets.toml,
# graphrt-gateway-k8s (features gateway,gateway-k8s) and jwt-verify-bench (the
# test binary); bench/targets.sh --build-only --bin-dir DIR writes exactly that,
# e.g. on a Mac with cargo-zigbuild, to copy into a Linux VM.
#
# Needs Linux, Python 3.11+, curl, node (wake-node) and, for the container
# test, root and docker (CONTAINER_CLI). Results: bench/results/targets-<date>-<host>.{md,json}.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN_DIR=""
TARGET=""
BUILD_CMD="cargo build"
QUICK=""
BUILD_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) BIN_DIR="$2"; shift 2 ;;
    --target) TARGET="$2"; shift 2 ;;
    --build-cmd) BUILD_CMD="$2"; shift 2 ;;
    --quick) QUICK="--quick"; shift ;;
    --build-only) BUILD_ONLY=1; shift ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
    *) echo "targets.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

target_args=()
[ -n "$TARGET" ] && target_args=(--target "$TARGET")
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

build_all() {
  local out="$1"
  mkdir -p "$out"
  echo "==> agent and gateway builds (min-size)" >&2
  # Exit 1 is a size over budget: measured (and reported) by the measurement run.
  python3 scripts/budget-gate.py "${target_args[@]}" --build-cmd "$BUILD_CMD" --keep-bins "$out" --skip-run --skip-deps >/dev/null \
    || [ $? -eq 1 ]
  echo "==> gateway with the Kubernetes provider (ttfb-tcp)" >&2
  # shellcheck disable=SC2086
  $BUILD_CMD -q -p graphrt-cli --locked --profile min-size --no-default-features \
    --features gateway,gateway-k8s "${target_args[@]}"
  cp "$CARGO_TARGET_DIR/${TARGET:+$TARGET/}min-size/graphrt" "$out/graphrt-gateway-k8s"
  echo "==> JWT verification microbench (test binary, release)" >&2
  # shellcheck disable=SC2086
  local bench
  bench="$($BUILD_CMD -q --release -p graphrt-gateway --test jwt_verify_bench "${target_args[@]}" \
    --message-format json | python3 -c '
import json, sys
for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    if message.get("reason") == "compiler-artifact" and message.get("executable"):
        print(message["executable"])
' | tail -1)"
  cp "$bench" "$out/jwt-verify-bench"
}

if [ "$BUILD_ONLY" = 1 ]; then
  [ -n "$BIN_DIR" ] || { echo "targets.sh: --build-only needs --bin-dir" >&2; exit 2; }
  build_all "$BIN_DIR"
  echo "binaries in $BIN_DIR" >&2
  exit 0
fi

[ "$(uname -s)" = Linux ] || { echo "targets.sh: measurements need Linux (use --build-only elsewhere)" >&2; exit 2; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
if [ -z "$BIN_DIR" ]; then
  BIN_DIR="$WORK/bins"
  build_all "$BIN_DIR"
fi
BIN_DIR="$(cd "$BIN_DIR" && pwd)"

STAMP="$(date +%Y-%m-%d)-$(hostname -s)-$(uname -m)"
OUT="bench/results/targets-$STAMP"
mkdir -p bench/results
failed=0
deps_flag=()
command -v cargo >/dev/null || deps_flag=(--skip-deps)

echo "==> sizes, idle RSS, start-up" >&2
python3 scripts/budget-gate.py "${target_args[@]}" --bin-dir "$BIN_DIR" "${deps_flag[@]}" \
  --json "$WORK/budgets.json" --markdown "$WORK/budgets.md" >/dev/null || failed=1

echo "==> gateway run-time targets" >&2
k8s_flag=()
[ -x "$BIN_DIR/graphrt-gateway-k8s" ] && k8s_flag=(--gateway-k8s "$BIN_DIR/graphrt-gateway-k8s")
python3 bench/gateway/gwbench.py --gateway "$BIN_DIR/graphrt-gateway" --agent "$BIN_DIR/graphrt-agent-slim-gateway" \
  "${k8s_flag[@]}" $QUICK --json "$WORK/gateway.json" --markdown "$WORK/gateway.md" >/dev/null || failed=1

echo "==> JWT verification" >&2
jwt_budget="$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["gateway_targets"]["jwt_verify_rs256_us"])' bench/budgets.toml)"
jwt_line="$(GRAPHRT_JWT_BUDGET_US="$jwt_budget" "$BIN_DIR/jwt-verify-bench" --ignored --nocapture 2>&1 | grep jwt_verify_rs256_mean_us || true)"
jwt_status="FAIL"
GRAPHRT_JWT_BUDGET_US="$jwt_budget" "$BIN_DIR/jwt-verify-bench" --ignored >/dev/null 2>&1 && jwt_status="ok" || failed=1

echo "==> PID 1 drain" >&2
cli="${CONTAINER_CLI:-docker}"
if [ "$(id -u)" = 0 ] && command -v "$cli" >/dev/null && "$cli" info >/dev/null 2>&1; then
  pid1="$(CONTAINER_CLI="$cli" scripts/test-pid1-container.sh "$BIN_DIR/graphrt-agent-slim" 2>&1 | tail -1)" || failed=1
elif [ "$(id -u)" = 0 ]; then
  pid1="$(scripts/test-pid1.sh "$BIN_DIR/graphrt-agent-slim" 2>&1 | tail -1) (no container runtime: PID namespace only)" || failed=1
else
  pid1="skipped: needs root (sudo bench/targets.sh, or run scripts/test-pid1-container.sh as root)"
fi

{
  cat "$WORK/budgets.md"
  echo
  cat "$WORK/gateway.md"
  echo
  echo "### JWT verification (RS256, cached JWKS key)"
  echo
  echo "| Metric | Value | Target | |"
  echo "| --- | --- | ---: | --- |"
  echo "| mean per verification | ${jwt_line:-no output} | ≤ ${jwt_budget} µs | ${jwt_status} |"
  echo
  echo "### PID 1 drain"
  echo
  echo "$pid1"
  echo
  if [ "$failed" = 0 ]; then echo "**All budgets met.**"; else echo "**Budgets missed (see FAIL rows).**"; fi
} | tee "$OUT.md"
python3 - "$WORK" "$OUT.json" <<'PY'
import json, os, sys
work, out = sys.argv[1], sys.argv[2]
load = lambda name: json.load(open(os.path.join(work, name))) if os.path.exists(os.path.join(work, name)) else None
json.dump({"budgets": load("budgets.json"), "gateway": load("gateway.json")}, open(out, "w"), indent=2)
PY
[ -n "$QUICK" ] && exit 0
exit "$failed"
