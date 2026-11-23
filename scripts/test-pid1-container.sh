#!/usr/bin/env bash
# SIGTERM drain of `graphrt serve` as PID 1 of a real container (`docker stop`,
# as Docker, Kubernetes and Cloud Run stop one). Companion of test-pid1.sh,
# which uses a PID namespace only; this one goes through a container runtime.
#
#   1. builds an image of the static binary on the base of the release image
#      (distroless static, as in the Dockerfile; BASE_IMAGE overrides it);
#   2. starts a fake OpenAI endpoint on the host that never answers;
#   3. runs the image (host network, store bind-mounted), starts an async run
#      and waits until its LLM call is in flight;
#   4. stops the container (SIGTERM, 10 s before SIGKILL) and checks that
#      graphrt exited 0 within its drain timeout and stored the run as paused
#      (reason `drained`) with a snapshot.
#
# Usage: scripts/test-pid1-container.sh path/to/graphrt   (static Linux build with `serve,llm`)
# Environment: CONTAINER_CLI (default docker; podman works too), BASE_IMAGE.
# The container uses the host network, so a rootless runtime (its own loopback)
# cannot reach the fake model endpoint: use a rootful one.
set -euo pipefail

cli="${CONTAINER_CLI:-docker}"
binary="$(cd "$(dirname "${1:?usage: test-pid1-container.sh path/to/graphrt}")" && pwd)/$(basename "$1")"
work="$(mktemp -d)"
name="graphrt-pid1-$$"
image="graphrt-pid1-test:$$"
upstream_pid=""
cleanup() {
  "$cli" rm -f "$name" >/dev/null 2>&1 || true
  "$cli" rmi -f "$image" >/dev/null 2>&1 || true
  [ -n "$upstream_pid" ] && kill "$upstream_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
fail() {
  echo "test-pid1-container: FAIL: $*" >&2
  "$cli" logs "$name" 2>&1 | sed 's/^/  graphrt: /' >&2 || true
  exit 1
}

free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
upstream_port="$(free_port)"
serve_port="$(free_port)"

mkdir -p "$work/image" "$work/workflows" "$work/store"
cp "$binary" "$work/image/graphrt"
# Not `scratch`: without a CA bundle the HTTP client of the model call cannot be built.
cat > "$work/image/Dockerfile" <<DOCKERFILE
FROM ${BASE_IMAGE:-gcr.io/distroless/static-debian12:nonroot}
COPY graphrt /usr/local/bin/graphrt
ENTRYPOINT ["/usr/local/bin/graphrt"]
DOCKERFILE
"$cli" build -q -t "$image" "$work/image" >/dev/null || fail "image build failed"

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

chmod 0777 "$work/store"
"$cli" run -d --name "$name" --network host \
  -v "$work/workflows:/workflows:ro" -v "$work/store:/store" \
  -e OPENAI_API_KEY=test -e OPENAI_BASE_URL="http://127.0.0.1:$upstream_port/v1" \
  "$image" serve /workflows --addr "127.0.0.1:$serve_port" --store /store --drain-timeout 2s >/dev/null \
  || fail "container did not start"

for _ in $(seq 1 500); do
  curl -fs "http://127.0.0.1:$serve_port/healthz" >/dev/null && break
  sleep 0.01
done
run_id="$(curl -fs -H 'Content-Type: application/json' -d '{"async": true}' \
  "http://127.0.0.1:$serve_port/v1/workflows/chat/runs" | python3 -c 'import json,sys; print(json.load(sys.stdin)["run_id"])')" \
  || fail "could not start a run"
for _ in $(seq 1 500); do
  [ -s "$work/calls" ] && break
  sleep 0.01
done
[ -s "$work/calls" ] || fail "the run never called the model: $(curl -s "http://127.0.0.1:$serve_port/v1/runs/$run_id" | head -c 600)"

started="$(date +%s%N)"
"$cli" stop -t 10 "$name" >/dev/null
elapsed_ms=$(( ($(date +%s%N) - started) / 1000000 ))
status="$("$cli" inspect -f '{{.State.ExitCode}}' "$name")"
[ "$status" = "0" ] || fail "exit status $status after docker stop (137 = SIGKILL: SIGTERM ignored)"
[ "$elapsed_ms" -le 4000 ] || fail "stop took ${elapsed_ms} ms (drain timeout 2 s)"

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
echo "test-pid1-container: ok: $cli stop drained graphrt (PID 1) in ${elapsed_ms} ms, exit 0; run $run_id paused (drained)"
