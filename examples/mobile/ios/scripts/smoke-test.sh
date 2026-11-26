#!/usr/bin/env bash
# Installs the sample app on a booted iOS simulator, runs the hello workflow with
# autorun, and checks the console output. Exits non-zero on any mismatch.
#
# Usage: examples/mobile/ios/scripts/smoke-test.sh [path/to/GraphrtSample.app]
#
# Environment (all optional):
#   SIMULATOR_DEVICE       simulator UDID or "booted" (default: booted)
#   LAUNCH_TIMEOUT_SECONDS how long to wait for the app to come up and load the
#                          workflow (default: 300). The first launch after an
#                          install on a freshly booted simulator takes well over a
#                          minute on CI runners: it is waited out here, not
#                          counted against the run.
#   SMOKE_TIMEOUT_SECONDS  how long to wait for the runs once the workflow loaded
#                          (default: 60)
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_dir="$(cd "$script_dir/.." && pwd)"
app_bundle="${1:-$app_dir/build/DerivedData/Build/Products/Debug-iphonesimulator/GraphrtSample.app}"
device="${SIMULATOR_DEVICE:-booted}"
launch_timeout_seconds="${LAUNCH_TIMEOUT_SECONDS:-300}"
timeout_seconds="${SMOKE_TIMEOUT_SECONDS:-60}"
bundle_id="dev.graphrt.sample"
expected_greeting="Hello, Grace."

if [[ ! -d "$app_bundle" ]]; then
  echo "error: $app_bundle not found; build it with xcodebuild (see examples/mobile/ios/README.md)" >&2
  exit 1
fi

started=$SECONDS
stamp() { echo "smoke [+$((SECONDS - started)) s] $*"; }

xcrun simctl install "$device" "$app_bundle"
stamp "installed"

# --console-pty gives the app a terminal, so print() is line-buffered and every
# line reaches the log as soon as it is written. The launch stays attached until
# the app exits, so it runs in the background and is stopped below.
log_file="$(mktemp)"
xcrun simctl launch --console-pty --terminate-running-process "$device" "$bundle_id" \
  -name Grace -autorun YES >"$log_file" 2>&1 &
launch_pid=$!
cleanup() {
  # `simctl terminate` can hang on a busy simulator: never wait for it long.
  xcrun simctl terminate "$device" "$bundle_id" >/dev/null 2>&1 &
  local terminate_pid=$!
  for _ in $(seq 1 20); do
    kill -0 "$terminate_pid" 2>/dev/null || break
    sleep 1
  done
  kill "$terminate_pid" "$launch_pid" >/dev/null 2>&1 || true
  rm -f "$log_file"
}
trap cleanup EXIT
stamp "launched"

# The app prints "loaded:" once the runtime compiled the workflow (or "... failed").
launch_deadline=$((SECONDS + launch_timeout_seconds))
while ((SECONDS < launch_deadline)); do
  if grep -qE "\[GraphrtSample\] (loaded: |.* failed)" "$log_file"; then
    break
  fi
  if ! kill -0 "$launch_pid" 2>/dev/null; then
    break # the app exited (or crashed)
  fi
  sleep 1
done
if grep -q "\[GraphrtSample\] loaded: " "$log_file"; then
  stamp "workflow loaded"
else
  stamp "the app did not load the workflow within ${launch_timeout_seconds} s"
fi

# Both runs print a line once they are done: "result:" (waitJson) and "finish:" (listener).
deadline=$((SECONDS + timeout_seconds))
while ((SECONDS < deadline)); do
  if grep -q "\[GraphrtSample\] .* failed" "$log_file"; then
    break
  fi
  if grep -q "\[GraphrtSample\] result: " "$log_file" && grep -q "\[GraphrtSample\] finish: " "$log_file"; then
    break
  fi
  if ! kill -0 "$launch_pid" 2>/dev/null; then
    break # the app exited (or crashed)
  fi
  sleep 1
done
stamp "runs ended"
tr -d '\r' <"$log_file"

failures=()
grep -q "result: .*\"status\":\"succeeded\"" "$log_file" || failures+=("waitJson did not report status succeeded")
grep -q "result: .*\"greeting\":\"$expected_greeting\"" "$log_file" || failures+=("waitJson result lacks greeting '$expected_greeting'")
grep -q "event: run_succeeded" "$log_file" || failures+=("no run_succeeded event was streamed")
grep -q "finish: .*\"greeting\":\"$expected_greeting\"" "$log_file" || failures+=("onFinish result lacks greeting '$expected_greeting'")

if ((${#failures[@]} > 0)); then
  printf 'smoke test FAILED: %s\n' "${failures[@]}" >&2
  # What the simulator logged about the app, for the diagnosis.
  echo "--- simulator log of GraphrtSample (last 10 minutes) ---" >&2
  xcrun simctl spawn "$device" log show --last 10m --style compact \
    --predicate 'process == "GraphrtSample"' 2>&1 | tail -n 80 >&2 || true
  exit 1
fi
echo "smoke test passed: hello workflow ran through the Swift binding"
