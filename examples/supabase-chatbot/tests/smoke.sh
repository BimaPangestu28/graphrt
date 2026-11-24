#!/usr/bin/env bash
# Offline end-to-end check of the chatbot backend: no LLM key, no Supabase project.
#
#   examples/supabase-chatbot/tests/smoke.sh <path-to-graphrt> <supabase-plugin-dir>
#
# Starts mock OpenAI + Supabase upstreams (mock_upstreams.py), runs `graphrt serve`
# exactly as deployed (--input-header, --verify-jwt, --user-rate-limit,
# --trust-plugin supabase in user_rpc mode) and checks that:
#   1. a request without a token, and one with a forged token, are refused (401);
#   2. a valid user JWT streams an answer built from the tool result;
#   3. the plugin called the allowlisted RPC with the *user's* JWT as Bearer;
#   4. the JWT never appears in the SSE stream;
#   5. the per-user rate limit answers 429.
# Needs bash, curl, python3 and Node.js >= 20 (for the plugin).
set -euo pipefail

GRAPHRT=${1:?usage: smoke.sh <graphrt binary> <supabase plugin dir>}
PLUGIN_DIR=${2:?usage: smoke.sh <graphrt binary> <supabase plugin dir>}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$WORK"' EXIT

OPENAI_PORT=18781 SUPABASE_PORT=18782 SERVE_PORT=18783
JWT_SECRET=smoke-test-secret-that-is-at-least-32-bytes-long
ISSUER="http://127.0.0.1:$SUPABASE_PORT/auth/v1"

python3 "$HERE/mock_upstreams.py" "$OPENAI_PORT" "$SUPABASE_PORT" "$WORK/supabase.log" &
sleep 0.5

# An HS256 Supabase-style access token for user `user-a`, valid for 10 minutes.
make_jwt() { # $1 = secret
  python3 - "$1" "$ISSUER" <<'PY'
import base64, hashlib, hmac, json, sys, time
b64 = lambda raw: base64.urlsafe_b64encode(raw).rstrip(b"=").decode()
header = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
claims = {"sub": "user-a", "role": "authenticated", "aud": "authenticated", "iss": sys.argv[2], "exp": int(time.time()) + 600}
payload = b64(json.dumps(claims).encode())
signature = b64(hmac.new(sys.argv[1].encode(), f"{header}.{payload}".encode(), hashlib.sha256).digest())
print(f"{header}.{payload}.{signature}")
PY
}
USER_JWT=$(make_jwt "$JWT_SECRET")
FORGED_JWT=$(make_jwt "not-the-project-secret-but-32-bytes-long")

GRAPHRT_API_KEY=server-only-admin-key \
OPENAI_API_KEY=mock OPENAI_BASE_URL="http://127.0.0.1:$OPENAI_PORT/v1" \
SUPABASE_URL="http://127.0.0.1:$SUPABASE_PORT" SUPABASE_KEY=sb_publishable_mock \
SUPABASE_TOOL_MODE=user_rpc \
SUPABASE_TOOL_RPC_ALLOWLIST=get_my_profile,list_diary_entries,list_important_dates,list_chat_messages,start_chat_session,add_chat_message \
SUPABASE_TOOL_TIMEOUT_MS=8000 SUPABASE_TOOL_MAX_ROWS=50 \
SUPABASE_JWT_SECRET="$JWT_SECRET" \
"$GRAPHRT" serve "$HERE/../agent.yaml" --addr "127.0.0.1:$SERVE_PORT" \
  --plugins "$PLUGIN_DIR" --no-default-plugins --trust-plugin supabase \
  --input-header user_jwt=X-User-Authorization \
  --verify-jwt user_jwt --jwt-secret-env SUPABASE_JWT_SECRET \
  --jwt-issuer "$ISSUER" --jwt-audience authenticated \
  --user-rate-limit 3 --max-body-bytes 65536 ${SERVE_EXTRA_ARGS:-} 2>"$WORK/serve.log" &
for _ in $(seq 50); do curl -fs "http://127.0.0.1:$SERVE_PORT/healthz" >/dev/null && break; sleep 0.1; done

URL="http://127.0.0.1:$SERVE_PORT/v1/workflows/agent/runs"
BODY='{"stream": true, "inputs": {"message": "What is my display name?", "session_id": "s1"},
       "sys": {"history": [{"role": "user", "content": "Hi"}, {"role": "assistant", "content": "Hello!"}], "today": "2026-11-20"}}'
status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
fail() { echo "FAIL: $*"; echo "--- serve log"; cat "$WORK/serve.log"; exit 1; }

[ "$(status -X POST "$URL" -H 'content-type: application/json' -d "$BODY")" = 401 ] || fail "no token was not refused"
[ "$(status -X POST "$URL" -H "X-User-Authorization: Bearer $FORGED_JWT" -H 'content-type: application/json' -d "$BODY")" = 401 ] \
  || fail "forged token was not refused"

start=$(python3 -c 'import time; print(time.time())')
curl -sN -X POST "$URL" -H "X-User-Authorization: Bearer $USER_JWT" -H 'content-type: application/json' -d "$BODY" >"$WORK/sse.txt"
elapsed=$(python3 -c "import time; print(round((time.time() - $start) * 1000))")
grep -q 'event: run_succeeded' "$WORK/sse.txt" || fail "run did not succeed: $(cat "$WORK/sse.txt")"
answer=$(python3 - "$WORK/sse.txt" <<'PY'
import json, sys
text = ""
for block in open(sys.argv[1]).read().split("\n\n"):
    lines = dict(line.split(": ", 1) for line in block.splitlines() if ": " in line)
    if lines.get("event") == "response":
        text += json.loads(lines["data"])["text"]
print(text)
PY
)
[ "$answer" = "Your display name is Ada." ] || fail "unexpected answer: $answer"
grep -q "$USER_JWT" "$WORK/sse.txt" && fail "the user JWT leaked into the SSE stream"
python3 - "$WORK/supabase.log" "$USER_JWT" <<'PY' || fail "the RPC was not called as the user"
import json, sys
calls = [json.loads(line) for line in open(sys.argv[1])]
assert len(calls) == 1, calls
call = calls[0]
assert call["path"] == "/rest/v1/rpc/get_my_profile", call
assert call["authorization"] == f"Bearer {sys.argv[2]}", call
assert call["apikey"] == "sb_publishable_mock", call
PY

# 3 runs per minute: 1 used above, 2 more fit, the next is limited. The second
# run is timed warm (the plugin process is already running).
curl -s -o /dev/null -X POST "$URL" -H "X-User-Authorization: Bearer $USER_JWT" -H 'content-type: application/json' -d "$BODY"
warm=$(curl -s -o /dev/null -w '%{time_total}' -X POST "$URL" -H "X-User-Authorization: Bearer $USER_JWT" \
  -H 'content-type: application/json' -d "$BODY" | python3 -c 'import sys; print(round(float(sys.stdin.read()) * 1000))')
[ "$(status -X POST "$URL" -H "X-User-Authorization: Bearer $USER_JWT" -H 'content-type: application/json' -d "$BODY")" = 429 ] \
  || fail "rate limit not applied"

echo "ok: 401 without/forged token; streamed answer \"$answer\" (first run ${elapsed} ms incl. plugin start, warm ${warm} ms, mock upstreams); RPC sent as the user; no JWT in SSE; 429 over the limit"
