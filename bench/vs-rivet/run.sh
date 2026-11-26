#!/usr/bin/env bash
# graphrt vs Rivet, same machine, same client, same mock model. One command:
#
#   bench/vs-rivet/run.sh                 # 3 runs of every scenario (~1.5–2 h)
#   bench/vs-rivet/run.sh --runs 5
#   bench/vs-rivet/run.sh --only cold,warm
#   bench/vs-rivet/run.sh --quick         # fewer samples, a smoke test (~10 min)
#
# Scenarios: cold, warm, memory, concurrent, recovery (see README.md).
# Each run starts every system fresh; within a run the systems take turns
# (odd runs graphrt first, even runs Rivet first), never at the same time.
#
# Needs Linux x86_64, Docker (without sudo), Node.js 24 (`node` on PATH, or
# NODE=/path/to/node), cargo, curl, sqlite3. Downloads the graphrt 0.6.2
# release binary and pulls rivetdev/engine:3.1.1.
#
# Writes bench/vs-rivet/results/<date>-<host>/: one JSON per scenario, system
# and run (summary + raw samples), versions.json, summary.json and summary.md
# (medians across runs).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

GRAPHRT_VERSION="${GRAPHRT_VERSION:-0.6.2}"
RIVET_VERSION="${RIVET_VERSION:-3.1.1}"
RUNS=3
QUICK=""
ONLY="cold,warm,memory,concurrent,recovery"
while [ $# -gt 0 ]; do
  case "$1" in
    --runs) RUNS="$2"; shift 2 ;;
    --only) ONLY="$2"; shift 2 ;;
    --quick) QUICK="--quick"; shift ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "run.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || { echo "run.sh: needs Linux x86_64" >&2; exit 2; }
for tool in docker cargo curl sqlite3 sha256sum; do
  command -v "$tool" >/dev/null || { echo "run.sh: $tool is required" >&2; exit 2; }
done
NODE="${NODE:-$(command -v node || true)}"
[ -x "$NODE" ] || { echo "run.sh: Node.js 24 is required (set NODE=)" >&2; exit 2; }
node_major="$("$NODE" -p 'process.versions.node.split(".")[0]')"
[ "$node_major" = 24 ] || echo "run.sh: warning: Node.js $node_major (the published numbers use Node.js 24)" >&2
NPM="$(dirname "$NODE")/npm"
export PATH="$(dirname "$NODE"):$PATH"
docker info >/dev/null 2>&1 || { echo "run.sh: docker is not usable by this user" >&2; exit 2; }

CACHE="${BENCH_CACHE:-$HOME/.cache/graphrt-vs-rivet}"
mkdir -p "$CACHE"

echo "==> graphrt $GRAPHRT_VERSION release binary (gateway build)" >&2
GRAPHRT_DIR="$CACHE/graphrt-$GRAPHRT_VERSION"
if [ ! -x "$GRAPHRT_DIR/graphrt" ]; then
  mkdir -p "$GRAPHRT_DIR"
  base="https://github.com/Graphrt/graphrt/releases/download/v$GRAPHRT_VERSION"
  archive="graphrt-gateway-x86_64-unknown-linux-musl.tar.gz"
  curl -fsSL "$base/$archive" -o "$GRAPHRT_DIR/$archive"
  curl -fsSL "$base/SHA256SUMS" -o "$GRAPHRT_DIR/SHA256SUMS"
  (cd "$GRAPHRT_DIR" && grep " $archive\$" SHA256SUMS | sha256sum -c - >/dev/null)
  tar -xzf "$GRAPHRT_DIR/$archive" -C "$GRAPHRT_DIR" --strip-components 1
fi
export GRAPHRT_BIN="$GRAPHRT_DIR/graphrt"

echo "==> Rivet engine $RIVET_VERSION image" >&2
export RIVET_IMAGE="rivetdev/engine:$RIVET_VERSION"
docker pull -q "$RIVET_IMAGE" >/dev/null

echo "==> mock model (release build)" >&2
cargo build --quiet --release --manifest-path mock-openai/Cargo.toml
export MOCK_BIN="$HERE/mock-openai/target/release/mock-openai"

echo "==> RivetKit app and harness (npm ci)" >&2
(cd rivet/app && "$NPM" ci --no-audit --no-fund --loglevel=error && "$NPM" run --silent build)
(cd harness && "$NPM" ci --no-audit --no-fund --loglevel=error)
export RIVET_NODE_BIN="$NODE"

# 1,000 streams need a few thousand descriptors in the client and the runtimes.
ulimit -n "$(ulimit -Hn)" 2>/dev/null || ulimit -n 65536 || true

STAMP="$(date +%Y-%m-%d)-$(hostname -s)${QUICK:+-quick}"
OUT="$HERE/results/$STAMP"
mkdir -p "$OUT"
export BENCH_WORK="$CACHE/work"
rm -rf "$BENCH_WORK"
mkdir -p "$BENCH_WORK"

"$NODE" - "$OUT/versions.json" <<'JS'
const { execSync } = require("node:child_process");
const fs = require("node:fs");
const sh = (cmd) => { try { return execSync(cmd, { stdio: ["ignore", "pipe", "ignore"] }).toString().trim(); } catch { return null; } };
const lock = JSON.parse(fs.readFileSync("rivet/app/package-lock.json", "utf8")).packages;
const versions = {
  graphrt: sh(`"${process.env.GRAPHRT_BIN}" version`),
  rivet_engine_image: process.env.RIVET_IMAGE,
  rivet_engine_image_id: sh(`docker image inspect -f '{{.Id}}' ${process.env.RIVET_IMAGE}`),
  rivet_engine_repo_digest: sh(`docker image inspect -f '{{index .RepoDigests 0}}' ${process.env.RIVET_IMAGE}`),
  rivetkit: lock["node_modules/rivetkit"]?.version,
  ai_sdk: lock["node_modules/ai"]?.version,
  ai_sdk_openai: lock["node_modules/@ai-sdk/openai"]?.version,
  node: process.version,
  docker: sh("docker version -f '{{.Server.Version}}'"),
  kernel: sh("uname -sr"),
  cpu: sh("lscpu | sed -n 's/^Model name:[[:space:]]*//p'"),
  logical_cpus: require("node:os").cpus().length,
  memory_gib: Math.round(require("node:os").totalmem() / 2 ** 30 * 10) / 10,
  disk: sh("df -h --output=source,fstype,size / | tail -1"),
  git_commit: sh("git rev-parse --short HEAD"),
  date: new Date().toISOString(),
};
fs.writeFileSync(process.argv[2], JSON.stringify(versions, null, 2) + "\n");
console.error(versions);
JS

IFS=, read -r -a scenarios <<<"$ONLY"

# "<scenario> <system> [extra args]" per measured configuration.
configs_for() {
  case "$1" in
    cold|warm)
      echo "$1 graphrt"
      echo "$1 rivet"
      echo "$1 rivet --transport ws" ;;
    concurrent)
      echo "concurrent graphrt --replicas 1"
      echo "concurrent rivet --replicas 1"
      echo "concurrent graphrt --replicas 4"
      echo "concurrent rivet --replicas 4" ;;
    *)
      echo "$1 graphrt"
      echo "$1 rivet" ;;
  esac
}

failed=0
for run in $(seq 1 "$RUNS"); do
  for scenario in "${scenarios[@]}"; do
    mapfile -t configs < <(configs_for "$scenario")
    # Odd runs graphrt first, even runs Rivet first.
    if [ $((run % 2)) = 0 ]; then
      mapfile -t configs < <(printf '%s\n' "${configs[@]}" | sort -k2,2r -s)
    fi
    for config in "${configs[@]}"; do
      echo "==> run $run/$RUNS: $config" >&2
      # shellcheck disable=SC2086
      if ! "$NODE" harness/bench.mjs $config --run "$run" --out "$OUT" $QUICK >"$OUT/.last.log" 2>&1; then
        echo "run.sh: FAILED: $config (run $run); log:" >&2
        tail -30 "$OUT/.last.log" >&2
        cp "$OUT/.last.log" "$OUT/failed-$(echo "$config" | tr ' ' '_')-run$run.log"
        failed=1
      fi
    done
  done
done
rm -f "$OUT/.last.log"

echo "==> report" >&2
"$NODE" harness/report.mjs "$OUT"
echo "results: $OUT" >&2
exit "$failed"
