#!/usr/bin/env bash
# Conformance C13: binary delta of the Azure features on top of what the
# gateway links anyway. Builds examples/size_probe.rs with the `min-size`
# profile without features (baseline) and with each feature set, and prints
# the stripped sizes and deltas against the plan §9 budgets
# (auth-azure 60 KiB, gw-aca 80 KiB).
set -euo pipefail
cd "$(dirname "$0")/../.."
target_dir="${CARGO_TARGET_DIR:-target}"
binary="$target_dir/min-size/examples/size_probe"
size_of() {
  cargo build --quiet --profile min-size -p graphrt-provider-azure --example size_probe ${1:+--features "$1"}
  stat -f%z "$binary" 2>/dev/null || stat -c%s "$binary"
}
baseline=$(size_of "")
auth=$(size_of "auth-azure")
aca=$(size_of "provider-aca")
kib() { awk -v bytes="$1" 'BEGIN { printf "%.1f KiB", bytes / 1024 }'; }
echo "baseline (tokio + reqwest/rustls/ring + serde_json): $baseline bytes"
echo "auth-azure:              $auth bytes  (delta $(kib $((auth - baseline))))"
echo "provider-aca (incl. auth): $aca bytes  (delta $(kib $((aca - baseline))), aca alone $(kib $((aca - auth))))"
