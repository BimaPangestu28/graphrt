#!/usr/bin/env bash
# SIGTERM drain of `graphrt serve` running as PID 1, as in a container without
# an init process (Docker, Kubernetes, Cloud Run). Linux only; needs root (or
# unprivileged user namespaces) for `unshare --pid`.
#
# The kernel ignores a signal sent to a PID 1 that installed no handler for it,
# so before graphrt handled SIGTERM itself the process kept running until the
# platform's SIGKILL and lost its active runs. This test:
#
#   1. starts a fake OpenAI endpoint that never answers (a slow LLM call);
#   2. runs `graphrt serve --store DIR --drain-timeout 2s` as PID 1 of a new PID
#      namespace, starts an async run and waits until its LLM call is in flight;
#   3. sends SIGTERM and checks that graphrt exits 0 within the drain timeout
#      and stored the run as paused (reason `drained`) with a snapshot.
#
# Usage: sudo scripts/test-pid1.sh path/to/graphrt   (a build with `serve,llm`)
set -euo pipefail

binary="$(cd "$(dirname "${1:?usage: test-pid1.sh path/to/graphrt}")" && pwd)/$(basename "$1")"
work="$(mktemp -d)"
upstream_pid=""
outer_pid=""
cleanup() {
  [ -n "$outer_pid" ] && kill -KILL "$outer_pid" 2>/dev/null || true
  [ -n "$upstream_pid" ] && kill "$upstream_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "test-pid1: FAIL: $*" >&2; [ -f "$work/serve.log" ] && sed 's/^/  serve: /' "$work/serve.log" >&2; exit 1; }

free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
upstream_port="$(free_port)"
serve_port="$(free_port)"

# A chat-completions endpoint that accepts the request and then hangs.
cat > "$work/upstream.py" <<'PY'
import http.server, sys, time
class Hang(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        open(sys.argv[2], "a").write("call\n")
        time.sleep(600)
    def log_message(self, *args):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Hang).serve_forever()
PY
python3 "$work/upstream.py" "$upstream_port" "$work/calls" &
upstream_pid=$!
# graphrt starts faster than Python: a run sent before the fake endpoint
# listens fails with a network error instead of hanging in the model call.
for _ in $(seq 1 500); do
  (exec 3<>"/dev/tcp/127.0.0.1/$upstream_port") 2>/dev/null && break
  sleep 0.01
done

mkdir -p "$work/workflows"
cat > "$work/workflows/chat.yaml" <<'YAML'
version: "1"
name: chat
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - id: llm
      type: llm
      model: { provider: openai, name: gpt-4o-mini }
      prompt_template:
        - { role: user, text: "Say hi." }
    - { id: end, type: end, outputs: [{ variable: text, value_selector: [llm, text] }] }
  edges:
    - { source: start, target: llm }
    - { source: llm, target: end }
YAML

OPENAI_API_KEY=test OPENAI_BASE_URL="http://127.0.0.1:$upstream_port/v1" \
  unshare --pid --fork --mount-proc -- "$binary" serve "$work/workflows" \
  --addr "127.0.0.1:$serve_port" --store "$work/store" --drain-timeout 2s \
  > "$work/serve.log" 2>&1 &
outer_pid=$!

# graphrt is the child of `unshare --fork`; check that it is PID 1 in its namespace.
graphrt_pid=""
for _ in $(seq 1 200); do
  graphrt_pid="$(pgrep -P "$outer_pid" || true)"
  [ -n "$graphrt_pid" ] && break
  sleep 0.02
done
[ -n "$graphrt_pid" ] || fail "graphrt did not start"
nspid="$(awk '/^NSpid:/ {print $NF}' "/proc/$graphrt_pid/status")"
[ "$nspid" = "1" ] || fail "graphrt is PID $nspid in its namespace, expected 1"

for _ in $(seq 1 500); do
  curl -fs "http://127.0.0.1:$serve_port/healthz" >/dev/null && break
  sleep 0.01
done
run_id="$(curl -fs -H 'Content-Type: application/json' -d '{"async": true}' \
  "http://127.0.0.1:$serve_port/v1/workflows/chat/runs" | python3 -c 'import json,sys; print(json.load(sys.stdin)["run_id"])')"
for _ in $(seq 1 500); do
  [ -s "$work/calls" ] && break
  sleep 0.01
done
[ -s "$work/calls" ] || fail "the run never called the model"

started="$(date +%s%N)"
kill -TERM "$graphrt_pid"
status=0
wait "$outer_pid" || status=$?
outer_pid=""
elapsed_ms=$(( ($(date +%s%N) - started) / 1000000 ))
[ "$status" -eq 0 ] || fail "exit status $status after SIGTERM"
[ "$elapsed_ms" -le 2500 ] || fail "exit took ${elapsed_ms} ms (drain timeout 2 s)"

record="$work/store/runs/$run_id/record.json"
[ -f "$record" ] || fail "no stored record for $run_id"
python3 - "$record" <<'PY' || fail "run not stored as drained"
import json, sys
record = json.load(open(sys.argv[1]))
assert record["status"] == "paused", record["status"]
assert record["pause_reasons"][0]["kind"] == "drained", record["pause_reasons"]
assert record["pause_reasons"][0]["data"]["cause"] == "SIGTERM"
PY
[ -f "$work/store/runs/$run_id/snapshot.json" ] || fail "no snapshot for $run_id"
echo "test-pid1: ok: graphrt as PID 1 drained on SIGTERM in ${elapsed_ms} ms; run $run_id paused (drained)"
