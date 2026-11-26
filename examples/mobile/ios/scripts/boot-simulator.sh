#!/usr/bin/env bash
# Creates and boots an iOS simulator for the smoke test, and waits until it can
# actually launch apps. `simctl bootstatus -b` returns once the boot sequence ends,
# but SpringBoard and the launch services behind it can still be busy for minutes
# on a fresh device: an app launched then never starts. So after the boot this
# script launches a system app (Settings) and only reports the simulator ready
# once that launch returns a pid. A boot that stalls is retried on a fresh device
# (the smoke test itself is never retried), and the logs of every failed attempt
# are kept for the diagnosis.
#
# Usage: examples/mobile/ios/scripts/boot-simulator.sh
#
# Prints the UDID of the ready simulator on stdout (everything else goes to
# stderr) and appends SIMULATOR_DEVICE=<udid> to $GITHUB_ENV when it is set.
#
# Environment (all optional):
#   SIMULATOR_RUNTIME      runtime identifier
#                          (default: com.apple.CoreSimulator.SimRuntime.iOS-18-5)
#   SIMULATOR_DEVICE_TYPE  device type identifier
#                          (default: com.apple.CoreSimulator.SimDeviceType.iPhone-16)
#   BOOT_ATTEMPTS          how many fresh devices to try (default: 3)
#   BOOT_TIMEOUT_SECONDS   how long one boot may take (default: 300)
#   READY_TIMEOUT_SECONDS  how long a booted device may take to launch an app
#                          (default: 300)
#   DIAGNOSTICS_DIR        where the logs of failed attempts go
#                          (default: a temporary directory)
set -euo pipefail

runtime="${SIMULATOR_RUNTIME:-com.apple.CoreSimulator.SimRuntime.iOS-18-5}"
device_type="${SIMULATOR_DEVICE_TYPE:-com.apple.CoreSimulator.SimDeviceType.iPhone-16}"
attempts="${BOOT_ATTEMPTS:-3}"
boot_timeout_seconds="${BOOT_TIMEOUT_SECONDS:-300}"
ready_timeout_seconds="${READY_TIMEOUT_SECONDS:-300}"
diagnostics_dir="${DIAGNOSTICS_DIR:-$(mktemp -d)}"
probe_bundle_id="com.apple.Preferences"

started=$SECONDS
stamp() { echo "boot [+$((SECONDS - started)) s] $*" >&2; }

# macOS has no timeout(1): perl's alarm kills the command with SIGALRM.
with_timeout() {
  local seconds="$1"
  shift
  perl -e 'alarm shift; exec @ARGV or die "exec $ARGV[0]: $!\n"' "$seconds" "$@"
}

if ! xcrun simctl list runtimes available -j | jq -e --arg id "$runtime" \
  '.runtimes[] | select(.identifier == $id)' >/dev/null; then
  echo "error: simulator runtime $runtime is not installed. Installed runtimes:" >&2
  xcrun simctl list runtimes available >&2
  exit 1
fi
stamp "xcode: $(xcodebuild -version | tr '\n' ' ')"
stamp "simulator: $device_type on $runtime"

collect_diagnostics() {
  local udid="$1" attempt="$2"
  local dir="$diagnostics_dir/attempt-$attempt"
  mkdir -p "$dir"
  stamp "attempt $attempt: saving diagnostics to $dir"
  xcrun simctl list devices >"$dir/devices.txt" 2>&1 || true
  with_timeout 60 xcrun simctl spawn "$udid" launchctl print system \
    >"$dir/launchctl-print-system.txt" 2>&1 || true
  with_timeout 120 xcrun simctl spawn "$udid" log show --last 15m --style compact \
    >"$dir/simulator.log" 2>&1 || true
  tail -n 40 "$dir/simulator.log" >&2 || true
}

# Launching a system app is what the smoke test needs to work: it goes through
# SpringBoard (FrontBoard) and launchd exactly like the sample app will.
wait_until_ready() {
  local udid="$1"
  local deadline=$((SECONDS + ready_timeout_seconds))
  local output
  while ((SECONDS < deadline)); do
    if output="$(with_timeout 60 xcrun simctl launch "$udid" "$probe_bundle_id" 2>&1)" &&
      [[ "$output" =~ ^$probe_bundle_id:\ [0-9]+ ]]; then
      stamp "$output: the simulator launches apps"
      with_timeout 60 xcrun simctl terminate "$udid" "$probe_bundle_id" >/dev/null 2>&1 || true
      return 0
    fi
    stamp "not ready to launch apps yet: ${output:-no output}"
    sleep 5
  done
  return 1
}

for attempt in $(seq 1 "$attempts"); do
  udid="$(xcrun simctl create "graphrt-smoke-$attempt" "$device_type" "$runtime")"
  stamp "attempt $attempt: created $udid"
  if with_timeout "$boot_timeout_seconds" xcrun simctl bootstatus "$udid" -b >&2; then
    stamp "attempt $attempt: boot sequence finished"
    if wait_until_ready "$udid"; then
      if [[ -n "${GITHUB_ENV:-}" ]]; then
        echo "SIMULATOR_DEVICE=$udid" >>"$GITHUB_ENV"
      fi
      echo "$udid"
      exit 0
    fi
    stamp "attempt $attempt: the simulator did not launch $probe_bundle_id within ${ready_timeout_seconds} s"
  else
    stamp "attempt $attempt: the boot did not finish within ${boot_timeout_seconds} s"
  fi
  collect_diagnostics "$udid" "$attempt"
  with_timeout 120 xcrun simctl shutdown "$udid" >/dev/null 2>&1 || true
  with_timeout 120 xcrun simctl delete "$udid" >/dev/null 2>&1 || true
done

echo "error: no simulator became ready in $attempts attempts. Diagnostics are in $diagnostics_dir" >&2
exit 1
