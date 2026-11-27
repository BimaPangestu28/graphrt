#!/usr/bin/env bash
# Installs the sample app on a booted iOS simulator, runs the hello workflow and
# the phone assistant (examples/mobile/agent.yaml: a scripted model, native tools
# through the Swift ToolProvider, a pause for approval, the resume and the
# streamed answer) with autorun, and checks the console output. Exits non-zero on
# any mismatch.
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
#   DIAGNOSTICS_DIR        where the console output and the simulator logs go when
#                          the test fails (default: not saved, only printed)
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_dir="$(cd "$script_dir/.." && pwd)"
app_bundle="${1:-$app_dir/build/DerivedData/Build/Products/Debug-iphonesimulator/GraphrtSample.app}"
device="${SIMULATOR_DEVICE:-booted}"
launch_timeout_seconds="${LAUNCH_TIMEOUT_SECONDS:-300}"
timeout_seconds="${SMOKE_TIMEOUT_SECONDS:-60}"
bundle_id="dev.graphrt.sample"
expected_greeting="Hello, Grace."
expected_answer="Done: sent to Ada"

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

# The runs print a line once they are done: "result:" (waitJson), "finish:" (listener)
# and "agent finished:" (the assistant).
deadline=$((SECONDS + timeout_seconds))
while ((SECONDS < deadline)); do
  if grep -q "\[GraphrtSample\] .* failed" "$log_file"; then
    break
  fi
  if grep -q "\[GraphrtSample\] result: " "$log_file" && grep -q "\[GraphrtSample\] finish: " "$log_file" \
    && grep -q "\[GraphrtSample\] agent finished: " "$log_file"; then
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
grep -q "agent tool: battery_level" "$log_file" || failures+=("the agent did not call the native tool battery_level")
grep -q "agent tool done: battery_level error=false" "$log_file" || failures+=("battery_level did not answer through the ToolProvider")
grep -q "agent ui: stats" "$log_file" || failures+=("the checked UI block of battery_level did not reach the app")
grep -q "agent paused: send_message" "$log_file" || failures+=("the run did not pause for approval of send_message")
grep -q "agent tool done: send_message error=false" "$log_file" || failures+=("send_message did not run after the approval")
(( $(grep -c "agent answer chunk: " "$log_file") > 1 )) || failures+=("the answer did not stream in several chunks")
grep -q "agent finished: succeeded $expected_answer" "$log_file" || failures+=("the assistant did not finish with '$expected_answer'")

if ((${#failures[@]} > 0)); then
  printf 'smoke test FAILED: %s\n' "${failures[@]}" >&2
  # Tell an app that never started apart from one that started and went silent.
  if xcrun simctl spawn "$device" launchctl list 2>/dev/null | grep -q "UIKitApplication:$bundle_id"; then
    echo "the app process is running" >&2
  else
    echo "the app process is not running (it never started, or it exited)" >&2
  fi
  if [[ -n "${DIAGNOSTICS_DIR:-}" ]]; then
    mkdir -p "$DIAGNOSTICS_DIR/smoke-test"
    tr -d '\r' <"$log_file" >"$DIAGNOSTICS_DIR/smoke-test/console.txt"
    xcrun simctl spawn "$device" launchctl print system >"$DIAGNOSTICS_DIR/smoke-test/launchctl-print-system.txt" 2>&1 || true
    xcrun simctl spawn "$device" log show --last 15m --style compact \
      >"$DIAGNOSTICS_DIR/smoke-test/simulator.log" 2>&1 || true
    echo "diagnostics saved to $DIAGNOSTICS_DIR/smoke-test" >&2
  fi
  # What the simulator logged about the app, for the diagnosis.
  echo "--- simulator log of GraphrtSample (last 10 minutes) ---" >&2
  xcrun simctl spawn "$device" log show --last 10m --style compact \
    --predicate 'process == "GraphrtSample"' 2>&1 | tail -n 80 >&2 || true
  exit 1
fi
echo "smoke test passed: hello workflow and the phone assistant ran through the Swift SDK"
