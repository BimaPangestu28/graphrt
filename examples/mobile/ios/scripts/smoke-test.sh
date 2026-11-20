#!/usr/bin/env bash
# Installs the sample app on a booted iOS simulator, runs the hello workflow with
# autorun, and checks the console output. Exits non-zero on any mismatch.
#
# Usage: examples/mobile/ios/scripts/smoke-test.sh [path/to/GraphrtSample.app]
#
# Environment (all optional):
#   SIMULATOR_DEVICE      simulator UDID or "booted" (default: booted)
#   SMOKE_TIMEOUT_SECONDS how long to wait for the run to finish (default: 60)
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_dir="$(cd "$script_dir/.." && pwd)"
app_bundle="${1:-$app_dir/build/DerivedData/Build/Products/Debug-iphonesimulator/GraphrtSample.app}"
device="${SIMULATOR_DEVICE:-booted}"
timeout_seconds="${SMOKE_TIMEOUT_SECONDS:-60}"
bundle_id="dev.graphrt.sample"
expected_greeting="Hello, Grace."

if [[ ! -d "$app_bundle" ]]; then
  echo "error: $app_bundle not found; build it with xcodebuild (see examples/mobile/ios/README.md)" >&2
  exit 1
fi

xcrun simctl install "$device" "$app_bundle"

# --console-pty gives the app a terminal, so print() is line-buffered and every
# line reaches the log as soon as it is written. The launch stays attached until
# the app exits, so it runs in the background and is stopped below.
log_file="$(mktemp)"
xcrun simctl launch --console-pty --terminate-running-process "$device" "$bundle_id" \
  -name Grace -autorun YES >"$log_file" 2>&1 &
launch_pid=$!
cleanup() {
  xcrun simctl terminate "$device" "$bundle_id" >/dev/null 2>&1 || true
  kill "$launch_pid" >/dev/null 2>&1 || true
  rm -f "$log_file"
}
trap cleanup EXIT

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
tr -d '\r' <"$log_file"

failures=()
grep -q "result: .*\"status\":\"succeeded\"" "$log_file" || failures+=("waitJson did not report status succeeded")
grep -q "result: .*\"greeting\":\"$expected_greeting\"" "$log_file" || failures+=("waitJson result lacks greeting '$expected_greeting'")
grep -q "event: run_succeeded" "$log_file" || failures+=("no run_succeeded event was streamed")
grep -q "finish: .*\"greeting\":\"$expected_greeting\"" "$log_file" || failures+=("onFinish result lacks greeting '$expected_greeting'")

if ((${#failures[@]} > 0)); then
  printf 'smoke test FAILED: %s\n' "${failures[@]}" >&2
  exit 1
fi
echo "smoke test passed: hello workflow ran through the Swift binding"
