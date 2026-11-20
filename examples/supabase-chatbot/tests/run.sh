#!/usr/bin/env bash
# Runs every offline test of the Supabase chatbot backend (see README.md here):
#   smoke.sh         serve flags, JWT refusal, streamed answer, rate limit (mocks)
#   security_e2e.sh  user isolation and prompt injection on real PostgREST + Postgres
#
#   examples/supabase-chatbot/tests/run.sh [graphrt-binary] [supabase-plugin-dir]
#
# Defaults: graphrt from target/debug, then PATH; the plugin from
# ~/.graphrt/plugins/supabase, else fetched at the commit the Dockerfile pins
# (PLUGINS_REV). SKIP_SECURITY_E2E=1 runs only the smoke test (no Postgres or
# PostgREST needed). No LLM key, no Supabase project, no network except that fetch.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
GRAPHRT=${1:-}
if [ -z "$GRAPHRT" ]; then
  if [ -x "$REPO/target/debug/graphrt" ]; then GRAPHRT="$REPO/target/debug/graphrt"; else GRAPHRT=$(command -v graphrt || true); fi
fi
[ -x "$GRAPHRT" ] || { echo "run.sh: no graphrt binary (cargo build -p graphrt-cli, or pass its path)" >&2; exit 2; }

PLUGIN_DIR=${2:-}
FETCHED=""
trap '[ -z "$FETCHED" ] || rm -rf "$FETCHED"' EXIT
if [ -z "$PLUGIN_DIR" ] && [ -f "$HOME/.graphrt/plugins/supabase/graphrt-plugin.json" ]; then
  PLUGIN_DIR="$HOME/.graphrt/plugins/supabase"
fi
if [ -z "$PLUGIN_DIR" ]; then
  plugins_rev=$(sed -n 's/^ARG PLUGINS_REV=//p' "$HERE/../Dockerfile")
  FETCHED=$(mktemp -d)
  git -C "$FETCHED" init -q
  git -C "$FETCHED" fetch -q --depth 1 https://github.com/BimaPangestu28/graphrt-plugins "$plugins_rev"
  git -C "$FETCHED" checkout -q FETCH_HEAD
  PLUGIN_DIR="$FETCHED/supabase"
fi

echo "== smoke ($GRAPHRT, plugin $PLUGIN_DIR)"
"$HERE/smoke.sh" "$GRAPHRT" "$PLUGIN_DIR"
if [ "${SKIP_SECURITY_E2E:-0}" = 1 ]; then echo "== security e2e: skipped (SKIP_SECURITY_E2E=1)"; exit 0; fi
echo "== security e2e"
"$HERE/security_e2e.sh" "$GRAPHRT" "$PLUGIN_DIR"
