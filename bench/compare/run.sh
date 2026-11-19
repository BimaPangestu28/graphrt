#!/usr/bin/env bash
# graphrt vs Python agent stacks (LangGraph, LangChain LCEL): one entry point.
#
#   bench/compare/run.sh            # host measurements (~15 minutes)
#   bench/compare/run.sh --docker   # also cold start + peak RSS in a Linux container (1 CPU, 1 GiB)
#   bench/compare/run.sh --quick    # fewer hyperfine runs, for a smoke test
#
# Needs: cargo, hyperfine, Python 3.11 ($PYTHON, default python3.11), optionally docker.
# Writes bench/results/<date>-<host>-vs-python.{json,md}. The venv lives in
# bench/compare/.venv and is reused; `rm -rf bench/compare/.venv bench/compare/target`
# cleans up.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

DOCKER=0
COLD_RUNS=30
for arg in "$@"; do
  case "$arg" in
    --docker) DOCKER=1 ;;
    --quick) COLD_RUNS=5 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

PYTHON="${PYTHON:-python3.11}"
for tool in cargo hyperfine "$PYTHON"; do
  command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done

STAMP="$(date +%Y-%m-%d)-$(hostname -s)-vs-python"
OUT_DIR="$ROOT/bench/results"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$OUT_DIR"

CLI_TARGET="$(cargo metadata --format-version 1 --no-deps \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
for features in "" "--no-default-features --features llm"; do
  if [ -z "$features" ]; then label="default"; else label="llm-only"; fi
  echo "==> graphrt CLI, min-size profile ($label)"
  # shellcheck disable=SC2086
  cargo build --quiet -p graphrt-cli --profile min-size $features
  cp "$CLI_TARGET/min-size/graphrt" "$WORK/graphrt-$label"
done

echo "==> fake OpenAI server and in-process graphrt driver (release)"
CARGO_TARGET_DIR="$HERE/target" cargo build --quiet --release --locked --manifest-path "$HERE/rust/Cargo.toml"

echo "==> Python venv"
if [ ! -x "$HERE/.venv/bin/python" ]; then
  "$PYTHON" -m venv "$HERE/.venv"
fi
"$HERE/.venv/bin/python" -m pip install --quiet --disable-pip-version-check -r "$HERE/requirements.txt"

python3 "$HERE/workflows/generate.py" "$WORK/workflows"
cp "$HERE/workflows/llm3.yaml" "$WORK/workflows/"
cat > "$WORK/tools.json" <<EOF
{"graphrt": "$WORK/graphrt-default", "graphrt_driver": "$HERE/target/release/graphrt-driver",
 "fakeoai": "$HERE/target/release/fakeoai", "python": "$HERE/.venv/bin/python",
 "workflows": "$WORK/workflows", "venv": "$HERE/.venv",
 "binaries": {"default": "$WORK/graphrt-default", "llm-only": "$WORK/graphrt-llm-only"}}
EOF

"$HERE/.venv/bin/python" - "$WORK/machine.json" "$WORK/graphrt-default" <<'PY'
import importlib.metadata as md, json, os, platform, subprocess, sys
def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True).stdout.strip()
cpu = sh("sysctl -n machdep.cpu.brand_string 2>/dev/null") or sh("lscpu | sed -n 's/^Model name:[[:space:]]*//p'")
mem = sh("sysctl -n hw.memsize 2>/dev/null")
mem_gib = round(int(mem) / 2**30, 1) if mem else round(int(sh("awk '/MemTotal/ {print $2}' /proc/meminfo")) / 2**20, 1)
packages = {"python": platform.python_version()}
for name in ("langgraph", "langchain-core", "langchain-openai", "openai", "httpx", "pydantic"):
    packages[name] = md.version(name)
json.dump({
    "cpu": cpu or platform.processor(), "logical_cpus": os.cpu_count(), "memory_gib": mem_gib,
    "os": sh("uname -srm"), "rustc": sh("rustc --version"),
    "graphrt": sh(f"{sys.argv[2]} version").split(" (")[0],
    "git_commit": sh("git rev-parse --short HEAD"), "python_packages": packages,
}, open(sys.argv[1], "w"))
PY

for section in cold seq conc footprint; do
  echo "==> $section"
  python3 "$HERE/measure.py" "$section" --tools "$WORK/tools.json" --out "$WORK/$section.json" --runs "$COLD_RUNS"
done

if [ "$DOCKER" = 1 ]; then
  command -v docker >/dev/null || { echo "missing: docker" >&2; exit 1; }
  echo "==> Linux container images"
  : > "$WORK/docker_sizes.jsonl"
  for target in graphrt-image python-image measure; do
    docker build --quiet -f "$HERE/docker/Dockerfile" --target "$target" -t "graphrt-compare:$target" . > /dev/null
  done
  python3 - "$WORK/docker_sizes.json" <<'PY'
import json, subprocess, sys
size = lambda tag: int(subprocess.run(["docker", "image", "inspect", tag, "--format", "{{.Size}}"],
                                      capture_output=True, text=True, check=True).stdout)
json.dump({"graphrt (distroless static)": size("graphrt-compare:graphrt-image"),
           "python:3.11-slim + requirements.txt": size("graphrt-compare:python-image")}, open(sys.argv[1], "w"))
PY
  echo "==> Linux container: cold start + peak RSS (--cpus 1 --memory 1g)"
  docker run --rm --cpus 1 --memory 1g graphrt-compare:measure --runs "$COLD_RUNS" > "$WORK/linux.json"
fi

python3 "$HERE/report.py" "$WORK" "$OUT_DIR/$STAMP"
