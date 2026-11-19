#!/usr/bin/env bash
# Container entry point: cold start + peak RSS of graphrt, LangGraph and LCEL
# inside Linux. Arguments are passed to `measure.py cold` (e.g. --runs 10).
# Progress goes to stderr, the result JSON to stdout.
set -euo pipefail
WORK="$(mktemp -d)"
python /bench/workflows/generate.py "$WORK/workflows"
cp /bench/workflows/llm3.yaml "$WORK/workflows/"
cat > "$WORK/tools.json" <<EOF
{"graphrt": "/usr/local/bin/graphrt", "fakeoai": "/usr/local/bin/fakeoai",
 "python": "/usr/local/bin/python", "workflows": "$WORK/workflows"}
EOF
python /bench/measure.py cold --tools "$WORK/tools.json" --out "$WORK/cold.json" "$@" >&2
python - "$WORK/cold.json" <<'PY'
import json, os, platform, sys
result = json.load(open(sys.argv[1]))
result["environment"] = {
    "os": f"{platform.system()} {platform.release()} {platform.machine()}",
    "python": platform.python_version(),
    "cpus_visible": os.cpu_count(),
}
print(json.dumps(result))
PY
