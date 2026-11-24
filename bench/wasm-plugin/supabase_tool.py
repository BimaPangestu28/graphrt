#!/usr/bin/env python3
"""Node.js vs WebAssembly build of the supabase `call_function` tool, through a real agent.

Runs examples/supabase-chatbot/agent.yaml (one tool call per run) against the
mock OpenAI and Supabase upstreams of examples/supabase-chatbot/tests, once per
plugin directory, and measures:

- cold: `graphrt run` per sample, exec -> exit (process start, plugin load or
  start, two model turns, one tool call);
- warm: one `graphrt serve`, sequential streamed runs, request -> run_succeeded;
- RSS of the serve process plus its children (the Node.js host) after the warm runs;
- the plugin's files on disk.

The mock upstreams answer in well under a millisecond, so the difference between
two plugins is the plugin's own cost. Standard library only.

  bench/wasm-plugin/supabase_tool.py --graphrt BIN --plugin node=DIR --plugin wasm=DIR \\
      [--cold 30] [--warm 200] [--json OUT]
"""

from __future__ import annotations

import argparse
import base64
import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EXAMPLE = ROOT / "examples" / "supabase-chatbot"
JWT = ".".join(
    base64.urlsafe_b64encode(json.dumps(part).encode()).rstrip(b"=").decode()
    for part in ({"alg": "HS256"}, {"sub": "user-a", "role": "authenticated"})
) + ".c2lnbmF0dXJl"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return round(ordered[min(len(ordered) - 1, max(0, round(fraction * len(ordered)) - 1))], 2)


def summary(values: list[float]) -> dict:
    return {"p50": percentile(values, 0.5), "p95": percentile(values, 0.95), "samples": len(values)}


def plugin_env(openai_port: int, supabase_port: int) -> dict:
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GRAPHRT_", "SUPABASE_", "OPENAI_"))}
    env.update({
        "OPENAI_API_KEY": "mock", "OPENAI_BASE_URL": f"http://127.0.0.1:{openai_port}/v1",
        "SUPABASE_URL": f"http://127.0.0.1:{supabase_port}", "SUPABASE_KEY": "sb_publishable_mock",
        "SUPABASE_TOOL_MODE": "user_rpc", "SUPABASE_TOOL_RPC_ALLOWLIST": "get_my_profile",
    })
    return env


def plugin_args(plugin_dir: Path) -> list[str]:
    return ["--plugins", str(plugin_dir), "--no-default-plugins", "--trust-plugin", "supabase", "--allow-private-network"]


def cold_runs(graphrt: Path, plugin_dir: Path, env: dict, samples: int) -> list[float]:
    command = [str(graphrt), "run", str(EXAMPLE / "agent.yaml"), *plugin_args(plugin_dir),
               "--input", "message=What is my display name?", "--input", f"user_jwt={JWT}"]
    times = []
    for _ in range(samples):
        began = time.perf_counter()
        done = subprocess.run(command, env=env, capture_output=True, text=True, timeout=60)
        times.append((time.perf_counter() - began) * 1000)
        if done.returncode != 0 or "Ada" not in done.stdout:
            sys.exit(f"supabase_tool: cold run failed ({plugin_dir}): {done.stdout[-500:]} {done.stderr[-1500:]}")
    return times


def stream_run(port: int) -> float:
    body = json.dumps({"stream": True, "inputs": {"message": "What is my display name?", "user_jwt": JWT}})
    began = time.perf_counter()
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    connection.request("POST", "/v1/workflows/agent/runs", body, {"content-type": "application/json"})
    response = connection.getresponse()
    text = b""
    while b"event: run_succeeded" not in text and b"event: run_failed" not in text:
        chunk = response.read1(65536)
        if not chunk:
            break
        text += chunk
    elapsed = (time.perf_counter() - began) * 1000
    connection.close()
    if b"event: run_succeeded" not in text or b"Ada" not in text:
        sys.exit(f"supabase_tool: warm run failed: {text[-800:]!r}")
    return elapsed


def tree_rss_mib(pid: int) -> float:
    """RSS of `pid` and its descendants (ps works on Linux and macOS)."""
    table = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,rss="], capture_output=True, text=True).stdout
    rows = [tuple(int(field) for field in line.split()) for line in table.splitlines() if line.strip()]
    family, total = {pid}, 0
    changed = True
    while changed:
        changed = False
        for child, parent, _ in rows:
            if parent in family and child not in family:
                family.add(child)
                changed = True
    total = sum(rss for child, _, rss in rows if child in family)
    return round(total / 1024, 1)


def warm_runs(graphrt: Path, plugin_dir: Path, env: dict, samples: int) -> tuple[list[float], dict]:
    port = free_port()
    command = [str(graphrt), "serve", str(EXAMPLE / "agent.yaml"), "--addr", f"127.0.0.1:{port}", *plugin_args(plugin_dir)]
    server = subprocess.Popen(command, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        deadline = time.time() + 15
        while True:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    break
            except OSError:
                if server.poll() is not None or time.time() > deadline:
                    sys.exit(f"supabase_tool: serve did not start: {server.stderr.read().decode()[-1500:]}")
                time.sleep(0.02)
        idle = tree_rss_mib(server.pid)
        first = stream_run(port)
        times = [stream_run(port) for _ in range(samples)]
        rss = {"idle_mib": idle, "after_runs_mib": tree_rss_mib(server.pid), "first_run_ms": round(first, 2)}
        return times, rss
    finally:
        server.terminate()
        server.wait(timeout=10)


# What an install holds: no tests, and for the WebAssembly build no Rust sources or build output.
SKIPPED_PARTS = {"test", "target", "src", "node_modules"}


def disk_bytes(plugin_dir: Path) -> int:
    files = [path for path in plugin_dir.rglob("*") if path.is_file()]
    kept = [path for path in files if not SKIPPED_PARTS & set(path.relative_to(plugin_dir).parts) and not path.name.startswith("Cargo.")]
    return sum(path.stat().st_size for path in kept)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--graphrt", type=Path, required=True, help="graphrt build with serve, llm, plugins, wasm and http")
    parser.add_argument("--plugin", action="append", required=True, help="LABEL=DIR, e.g. node=../graphrt-plugins/supabase")
    parser.add_argument("--cold", type=int, default=30)
    parser.add_argument("--warm", type=int, default=200)
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()
    openai_port, supabase_port = free_port(), free_port()
    work = Path(tempfile.mkdtemp(prefix="supabase-tool-bench-"))
    upstreams = subprocess.Popen(
        [sys.executable, str(EXAMPLE / "tests" / "mock_upstreams.py"), str(openai_port), str(supabase_port), str(work / "supabase.log")],
        stdout=subprocess.PIPE, text=True,
    )
    upstreams.stdout.readline()
    results = {}
    try:
        env = plugin_env(openai_port, supabase_port)
        for spec in args.plugin:
            label, _, directory = spec.partition("=")
            plugin_dir = Path(directory).resolve()
            warm, rss = warm_runs(args.graphrt, plugin_dir, env, args.warm)
            cold = cold_runs(args.graphrt, plugin_dir, env, args.cold)
            results[label] = {"dir": str(plugin_dir), "disk_bytes": disk_bytes(plugin_dir), "cold_run_ms": summary(cold),
                              "warm_run_ms": summary(warm), **rss}
            print(f"{label}: {json.dumps(results[label])}", flush=True)
    finally:
        upstreams.terminate()
    if args.json:
        args.json.write_text(json.dumps({"graphrt": str(args.graphrt), "results": results}, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
