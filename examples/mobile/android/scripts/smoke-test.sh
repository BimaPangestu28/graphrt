#!/usr/bin/env bash
# Installs the sample APK on a running emulator or device, runs the hello workflow
# with autorun, and checks the logcat output. Exits non-zero on any mismatch.
#
# Usage: examples/mobile/android/scripts/smoke-test.sh [path/to/app-debug.apk]
#
# Environment (all optional):
#   ANDROID_SERIAL        device to use when several are connected (read by adb)
#   SMOKE_TIMEOUT_SECONDS how long to wait for the run to finish (default: 60)
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_dir="$(cd "$script_dir/.." && pwd)"
apk_path="${1:-$app_dir/app/build/outputs/apk/debug/app-debug.apk}"
timeout_seconds="${SMOKE_TIMEOUT_SECONDS:-60}"
package_name="dev.graphrt.sample"
expected_greeting="Hello, Grace."

if [[ ! -f "$apk_path" ]]; then
  echo "error: $apk_path not found; build it with ./gradlew assembleDebug" >&2
  exit 1
fi

adb wait-for-device
echo "device: $(adb shell getprop ro.product.model | tr -d '\r') (API $(adb shell getprop ro.build.version.sdk | tr -d '\r'), $(adb shell getprop ro.product.cpu.abi | tr -d '\r'))"

adb install -r "$apk_path" >/dev/null
adb shell am force-stop "$package_name"
adb logcat -c
adb shell am start -W -n "$package_name/.MainActivity" --es name Grace --ez autorun true >/dev/null

# Both runs log a line once they are done: "result:" (waitJson) and "finish:" (listener).
log_file="$(mktemp)"
trap 'rm -f "$log_file"' EXIT
deadline=$((SECONDS + timeout_seconds))
while ((SECONDS < deadline)); do
  adb logcat -d -s GraphrtSample:V AndroidRuntime:E >"$log_file"
  if grep -qE "FATAL EXCEPTION| failed" "$log_file"; then
    break
  fi
  if grep -q "result: " "$log_file" && grep -q "finish: " "$log_file"; then
    break
  fi
  sleep 1
done
cat "$log_file"

failures=()
grep -q "FATAL EXCEPTION" "$log_file" && failures+=("the app crashed")
grep -q "result: .*\"status\":\"succeeded\"" "$log_file" || failures+=("waitJson did not report status succeeded")
grep -q "result: .*\"greeting\":\"$expected_greeting\"" "$log_file" || failures+=("waitJson result lacks greeting '$expected_greeting'")
grep -q "event: .*\"type\":\"run_succeeded\"" "$log_file" || failures+=("no run_succeeded event was streamed")
grep -q "finish: .*\"greeting\":\"$expected_greeting\"" "$log_file" || failures+=("onFinish result lacks greeting '$expected_greeting'")

if ((${#failures[@]} > 0)); then
  printf 'smoke test FAILED: %s\n' "${failures[@]}" >&2
  exit 1
fi
echo "smoke test passed: hello workflow ran through the Kotlin binding"
