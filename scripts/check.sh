#!/usr/bin/env bash
# The local check to run before every push: the same three commands as the
# `fmt + clippy + test` job of .github/workflows/ci.yml, with warnings as errors.
#
# It checks the working tree as it is, uncommitted changes included, and stops at
# the first command that fails. scripts/ci-local.sh runs every CI job against the
# committed HEAD instead.
#
# Usage: scripts/check.sh
# Environment: CARGO_TARGET_DIR (default: target/check). A directory of its own,
# because RUSTFLAGS differs from a plain `cargo build` and sharing `target/debug`
# would rebuild everything on each switch between the two.
set -euo pipefail

if [ $# -gt 0 ]; then
  echo "usage: scripts/check.sh (takes no arguments)" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/target/check}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
# Same as the `env` block of ci.yml.
export RUSTFLAGS="-D warnings"

run_step() {
  local name="$1"
  shift
  echo "== $name: $*" >&2
  if ! "$@"; then
    echo "check: FAILED at $name" >&2
    exit 1
  fi
}

run_step fmt cargo fmt --all -- --check
run_step clippy cargo clippy --workspace --all-targets --all-features
run_step test cargo test --workspace --all-features

echo "check: all passed" >&2
