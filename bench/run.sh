#!/usr/bin/env bash
# Reproducible graphrt benchmarks.
#
#   bench/run.sh               # everything except the Docker image
#   bench/run.sh --docker      # also build the container image and record its size
#   bench/run.sh --skip-sizes  # skip the (slow) min-size CLI builds
#
# Writes bench/results/<date>-<host>.json and bench/results/<date>-<host>.md.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

DOCKER=0
SIZES=1
for arg in "$@"; do
  case "$arg" in
    --docker) DOCKER=1 ;;
    --skip-sizes) SIZES=0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

STAMP="$(date +%Y-%m-%d)-$(hostname -s)"
OUT_DIR="bench/results"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$OUT_DIR"

echo "==> building the harness (release)"
cargo build --quiet --release --manifest-path bench/Cargo.toml
HARNESS_DIR="$(cargo metadata --manifest-path bench/Cargo.toml --format-version 1 --no-deps \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
BENCH="$HARNESS_DIR/release/graphrt-bench"

# Machine description.
python3 - "$WORK/machine.json" <<'PY'
import json, os, platform, subprocess, sys
def sh(cmd):
    try:
        return subprocess.run(cmd, shell=True, capture_output=True, text=True).stdout.strip()
    except Exception:
        return ""
cpu = sh("lscpu | sed -n 's/^Model name:[[:space:]]*//p'") or platform.processor()
mem_kib = sh("awk '/MemTotal/ {print $2}' /proc/meminfo")
info = {
    "cpu": cpu,
    "logical_cpus": os.cpu_count(),
    "memory_gib": round(int(mem_kib) / 1048576, 1) if mem_kib else None,
    "os": sh("uname -sr"),
    "rustc": sh("rustc --version"),
    "git_commit": sh("git rev-parse --short HEAD"),
}
json.dump(info, open(sys.argv[1], "w"))
PY

echo "==> scaling regression check (per-node cost 100 vs 1,000 nodes)"
cargo test --quiet --release -p graphrt-core --test engine_scaling -- --ignored

for section in engine concurrency streaming startup pool; do
  echo "==> $section"
  "$BENCH" "$section" > "$WORK/$section.json"
done

echo "==> memory (one process per measurement)"
: > "$WORK/mem.jsonl"
for n in 1 100 1000 10000; do
  "$BENCH" mem-paused "$n" >> "$WORK/mem.jsonl"
  "$BENCH" mem-inflight "$n" >> "$WORK/mem.jsonl"
done

: > "$WORK/sizes.jsonl"
if [ "$SIZES" = 1 ]; then
  CLI_DIR="$(cargo metadata --format-version 1 --no-deps \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
  for features in "--no-default-features" ""; do
    if [ -n "$features" ]; then label="core-only"; else label="default"; fi
    echo "==> min-size CLI build ($label)"
    # shellcheck disable=SC2086
    cargo build --quiet -p graphrt-cli --profile min-size $features
    cp "$CLI_DIR/min-size/graphrt" "$WORK/graphrt-$label"
    bytes="$(stat -c %s "$WORK/graphrt-$label")"
    echo "{\"build\": \"$label\", \"bytes\": $bytes}" >> "$WORK/sizes.jsonl"
  done

  echo "==> CLI cold start"
  python3 - "$WORK/graphrt-default" "$ROOT/examples/hello.yaml" "$WORK/coldstart.json" <<'PY'
import json, statistics, subprocess, sys, time
binary, hello, out = sys.argv[1], sys.argv[2], sys.argv[3]
def measure(args, n=50):
    for _ in range(3):
        subprocess.run(args, capture_output=True, check=True)
    samples = []
    for _ in range(n):
        t = time.perf_counter()
        subprocess.run(args, capture_output=True, check=True)
        samples.append((time.perf_counter() - t) * 1000)
    samples.sort()
    return {"runs": n, "p50_ms": statistics.median(samples), "p95_ms": samples[int(0.95 * (n - 1))]}
json.dump({
    "version": measure([binary, "version"]),
    "run_hello": measure([binary, "run", hello, "--input", "name=Ada", "--no-default-plugins", "--output", "json"]),
}, open(out, "w"))
PY
fi

if [ "$DOCKER" = 1 ] && command -v docker >/dev/null; then
  echo "==> docker image"
  docker build --quiet --network host -t graphrt-bench:latest . > /dev/null
  bytes="$(docker image inspect graphrt-bench:latest --format '{{.Size}}')"
  echo "{\"build\": \"docker-image\", \"bytes\": $bytes}" >> "$WORK/sizes.jsonl"
fi

python3 - "$WORK" "$OUT_DIR/$STAMP" <<'PY'
import json, os, sys
work, base = sys.argv[1], sys.argv[2]
load = lambda name: json.load(open(os.path.join(work, name)))
lines = lambda name: [json.loads(l) for l in open(os.path.join(work, name)) if l.strip()]
result = {
    "machine": load("machine.json"),
    "engine": load("engine.json"),
    "concurrency": load("concurrency.json"),
    "streaming": load("streaming.json"),
    "startup": load("startup.json"),
    "pool": load("pool.json"),
    "memory": lines("mem.jsonl"),
    "sizes": lines("sizes.jsonl"),
}
if os.path.exists(os.path.join(work, "coldstart.json")):
    result["cli_cold_start"] = load("coldstart.json")
json.dump(result, open(base + ".json", "w"), indent=2)

m = result["machine"]
md = [f"# graphrt benchmarks — {os.path.basename(base)}", "",
      f"{m['cpu']}, {m['logical_cpus']} logical CPUs, {m['memory_gib']} GiB RAM, {m['os']}, {m['rustc']}, commit {m['git_commit']}", "",
      "## Engine overhead (sequential runs, current-thread runtime)", "",
      "| Graph | Nodes | p50 ms | p95 ms | µs / node (p50) |", "|---|---:|---:|---:|---:|"]
for name, r in result["engine"].items():
    md.append(f"| {name} | {r['nodes']} | {r['p50_ms']:.3f} | {r['p95_ms']:.3f} | {r['p50_us_per_node']:.2f} |")
md += ["", "## Concurrent runs (multi-thread runtime, mock model, 10 streamed chunks)", "",
       "| Model delay | Concurrent runs | Runs / s | Ideal runs / s |", "|---:|---:|---:|---:|"]
for r in result["concurrency"]["rows"]:
    ideal = "-" if r["ideal_runs_per_sec"] is None else f"{r['ideal_runs_per_sec']:,.0f}"
    md.append(f"| {r['model_delay_ms']} ms | {r['concurrent_runs']} | {r['runs_per_sec']:,.0f} | {ideal} |")
s = result["streaming"]
md += ["", "## Streaming", "",
       f"Time to first answer chunk through engine + ResponseAssembler: p50 {s['engine_assembler_first_chunk_p50_us']:.1f} µs "
       f"(p95 {s['engine_assembler_first_chunk_p95_us']:.1f} µs); raw mock stream first delta: {s['direct_model_first_delta_p50_us']:.1f} µs; "
       f"overhead ≈ {s['overhead_p50_us']:.1f} µs.", "",
       "## Memory", "", "| Scenario | Runs | Peak RSS MiB | Snapshot bytes (avg) |", "|---|---:|---:|---:|"]
def rss(r):
    # Peak RSS comes from /proc/self/status (VmHWM), which only Linux has.
    return f"{r['peak_rss_kib']/1024:.1f}" if r.get("peak_rss_kib") else "n/a (Linux only)"
for r in result["memory"]:
    if "paused_runs" in r:
        md.append(f"| paused (snapshots held) | {r['paused_runs']} | {rss(r)} | {r['snapshot_json_bytes_avg']} |")
    else:
        md.append(f"| in flight (200 ms model) | {r['inflight_runs']} | {rss(r)} | - |")
p = result["pool"]
st = result["startup"]
md += ["", f"Variable pool clone ({p['pool']}): {p['bytes_allocated_per_clone']} bytes allocated, {p['clone_ns']:.1f} ns; "
       f"first write after a clone copies one namespace ({p['bytes_allocated_by_first_write_after_clone']} bytes).", "",
       "## Startup", "",
       f"- Parse + build, 3-node YAML: p50 {st['parse_build_small_yaml_3_nodes']['p50_ms']*1000:.0f} µs",
       f"- Parse + build, 1,002-node JSON: p50 {st['parse_build_large_json_1002_nodes']['p50_ms']:.2f} ms",
       f"- Node registry construction: {st['registry_build_us']:.0f} µs"]
if "cli_cold_start" in result:
    c = result["cli_cold_start"]
    md += [f"- CLI `graphrt version`: p50 {c['version']['p50_ms']:.1f} ms",
           f"- CLI `graphrt run hello.yaml`: p50 {c['run_hello']['p50_ms']:.1f} ms (process start to exit)"]
if result["sizes"]:
    md += ["", "## Sizes", "", "| Build | Size |", "|---|---:|"]
    for r in result["sizes"]:
        md.append(f"| {r['build']} | {r['bytes']/1048576:.2f} MiB |")
open(base + ".md", "w").write("\n".join(md) + "\n")
print(base + ".json")
print(base + ".md")
PY
