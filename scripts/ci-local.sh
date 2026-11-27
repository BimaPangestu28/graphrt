#!/usr/bin/env bash
# Runs the jobs of .github/workflows/ci.yml locally, for when GitHub Actions is
# unavailable or to check a branch before pushing.
#
# It tests the committed HEAD in a temporary git worktree, so uncommitted changes
# cannot hide (or cause) failures, and builds into its own target directory so it
# never fights another build over `target/`.
#
# Usage: scripts/ci-local.sh [--skip <job>]... [--keep]
#   jobs:   fmt, clippy, test, msrv, minimal-core, minimal-gateway, wasm, size, chatbot-e2e  (all run by default)
#   --keep  keep the temporary worktree afterwards (for debugging)
# Environment: GRAPHRT_CI_TARGET_DIR (default: ~/.cache/graphrt-ci-local/target)
#
# Tests run with --no-fail-fast so every failing test binary is reported, not just the first.
#
# Needs rustup. The msrv job needs the 1.89 toolchain and the wasm job the
# wasm32-unknown-unknown target; the script installs them with rustup if missing.
# The chatbot-e2e job (examples/supabase-chatbot/tests/run.sh) needs Node.js,
# Postgres and PostgREST; it is skipped with a note when one is missing.
set -euo pipefail

MSRV_TOOLCHAIN="1.89"
ALL_JOBS=(fmt clippy test msrv minimal-core minimal-gateway wasm size chatbot-e2e)
skipped_jobs=()
keep_worktree=false
while [ $# -gt 0 ]; do
  case "$1" in
    --skip) skipped_jobs+=("${2:?--skip needs a job name}"); shift 2 ;;
    --keep) keep_worktree=true; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
target_dir="${GRAPHRT_CI_TARGET_DIR:-$HOME/.cache/graphrt-ci-local/target}"
worktree_dir="$(mktemp -d)/graphrt"
export CARGO_TARGET_DIR="$target_dir" CARGO_TERM_COLOR=always RUSTFLAGS="-D warnings"

remove_worktree() {
  if [ "$keep_worktree" = true ]; then echo "ci-local: kept $worktree_dir" >&2; return; fi
  git -C "$repo_root" worktree remove --force "$worktree_dir" >/dev/null 2>&1 || true
}
trap remove_worktree EXIT
git -C "$repo_root" worktree add --detach --quiet "$worktree_dir" HEAD
cd "$worktree_dir"
echo "ci-local: testing $(git rev-parse --short HEAD) (target dir: $target_dir)" >&2

is_skipped() {
  local job
  for job in "${skipped_jobs[@]+"${skipped_jobs[@]}"}"; do [ "$job" = "$1" ] && return 0; done
  return 1
}

run_job() {
  local job="$1"; shift
  if is_skipped "$job"; then echo "== $job: skipped" >&2; return; fi
  echo "== $job: $*" >&2
  "$@"
}

# The chatbot e2e test needs a few services; without them it is skipped, not failed.
run_chatbot_e2e() {
  local tool
  for tool in node postgrest pg_ctl initdb psql; do
    if ! command -v "$tool" >/dev/null && ! [ -x "${PG_BIN:-/nonexistent}/$tool" ]; then
      echo "ci-local: chatbot-e2e needs $tool (brew install node postgresql@16 postgrest); skipped" >&2
      return 0
    fi
  done
  # Same features as the test job, so the debug binary it built is reused.
  cargo build -p graphrt-cli --all-features &&
    examples/supabase-chatbot/tests/run.sh "$target_dir/debug/graphrt"
}

# Same two checks as the msrv job of .github/workflows/ci.yml (keep the feature list in
# sync): `local-embed` (default in graphrt-cli) builds candle-core, which needs Rust
# 1.94+, so graphrt-cli is checked with its default features minus `local-embed`.
run_msrv() {
  cargo "+$MSRV_TOOLCHAIN" check --workspace --exclude graphrt-cli &&
    cargo "+$MSRV_TOOLCHAIN" check -p graphrt-cli --no-default-features --features \
      llm,http,code,python,mcp,doc,files,image,rag,plugins,wasm,otel,bundle,serve,trust-gateway,jwt,telemetry,scores,langfuse,deploy-fly,cors,run-follow,conversations,web-chat
}

ensure_toolchain() { rustup toolchain list | grep -q "^$1" || rustup toolchain install --profile minimal "$1"; }
ensure_wasm_target() { rustup target list --installed | grep -q '^wasm32-unknown-unknown$' || rustup target add wasm32-unknown-unknown; }

failed_jobs=()
for job in "${ALL_JOBS[@]}"; do
  case "$job" in
    fmt) command=(cargo fmt --all -- --check) ;;
    clippy) command=(cargo clippy --workspace --all-targets --all-features) ;;
    test) command=(cargo test --workspace --all-features --no-fail-fast) ;;
    msrv) is_skipped msrv || ensure_toolchain "$MSRV_TOOLCHAIN"; command=(run_msrv) ;;
    minimal-core) command=(cargo test -p graphrt-core --no-default-features --no-fail-fast) ;;
    minimal-gateway) command=(cargo test -p graphrt-cli --no-default-features --features gateway --test gateway_dev) ;;
    wasm) is_skipped wasm || ensure_wasm_target
      command=(cargo build -p graphrt-core --no-default-features --features yaml,jinja --target wasm32-unknown-unknown) ;;
    size) command=(cargo build -p graphrt-cli --profile min-size) ;;
    chatbot-e2e) command=(run_chatbot_e2e) ;;
  esac
  if ! run_job "$job" "${command[@]}"; then failed_jobs+=("$job"); fi
done

if ! is_skipped size && [ -f "$target_dir/min-size/graphrt" ]; then
  echo "ci-local: graphrt min-size binary: $(du -h "$target_dir/min-size/graphrt" | cut -f1)" >&2
fi
if [ ${#failed_jobs[@]} -gt 0 ]; then
  echo "ci-local: FAILED: ${failed_jobs[*]}" >&2
  exit 1
fi
echo "ci-local: all jobs passed" >&2
