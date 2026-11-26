#!/usr/bin/env python3
"""Measure graphrt builds against bench/budgets.toml and fail when one is exceeded.

For every [[build]] in the budgets file this builds graphrt-cli with
`--profile min-size --no-default-features --features <features>` (or takes a
prebuilt binary from --bin-dir), then measures:

- binary size, and for a build with `delta_of`, its size minus that earlier
  build's (`delta_kib`: what a feature costs, independent of the base growing);
- idle memory of `graphrt serve` (or, for a build with `run = "gateway"`, of
  `graphrt gateway` with one deployment and no agent) one second after it
  became healthy, the median over [startup].rss_samples starts (default 5)
  (`run = "none"`: size only). The budget (`idle_anon_mib`) is on anonymous
  memory (`RssAnon`: heap, stacks, written data pages), what each process
  costs on its own. Total RSS is reported too, but not gated: most of it is
  the binary's code pages, shared and file backed, and it moves by tens of
  KiB with code layout alone (the binary size budgets cover code);
- exec -> first 200 on GET /healthz, p95 over [startup].samples starts;
- that SIGTERM makes the server exit 0 (the drain path; a PID-1 run is covered
  by scripts/test-pid1.sh).

Run-time measurements need a binary that runs on this machine; a cross build
gets its size checked only (--skip-run is implied when it cannot execute).
It also checks that no banned crate is in the dependency tree. Builds marked
`optional = true` (the WebAssembly agent profile) run only with --optional, or
with --bin-dir when their binary is there.

Usage:
  scripts/budget-gate.py [--target TRIPLE] [--build-cmd "cargo zigbuild"]
                         [--bin-dir DIR] [--keep-bins DIR] [--skip-run] [--skip-deps] [--optional]
                         [--json OUT.json] [--markdown OUT.md]

Environment: CARGO_TARGET_DIR (default: target). Exit status 1 when a budget is
exceeded, 2 on a usage or build error. Needs Python 3.11+ (tomllib).
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shlex
import shutil
import signal
import statistics
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MIB = 1024 * 1024
WORKFLOW = """version: "1"
name: hello
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - { id: end, type: end, outputs: [] }
  edges:
    - { source: start, target: end }
"""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--budgets", type=Path, default=ROOT / "bench" / "budgets.toml")
    parser.add_argument("--target", help="target triple (default: the host)")
    parser.add_argument("--build-cmd", default="cargo build", help='build command, e.g. "cargo zigbuild"')
    parser.add_argument("--bin-dir", type=Path, help="use prebuilt binaries <dir>/graphrt-<build name>")
    parser.add_argument("--skip-run", action="store_true", help="check sizes only")
    parser.add_argument("--skip-deps", action="store_true", help="skip the dependency check")
    parser.add_argument(
        "--optional", action="store_true",
        help="also build and gate the builds marked `optional = true` (with --bin-dir they run when their binary is there)",
    )
    parser.add_argument("--keep-bins", type=Path, help="copy the measured binaries into this directory")
    parser.add_argument("--json", type=Path, help="write the measurements as JSON")
    parser.add_argument("--markdown", type=Path, help="write the result table as Markdown")
    return parser.parse_args()


def build(spec: dict, args: argparse.Namespace, out_dir: Path) -> Path:
    """Build one feature set and copy the binary out (the next build overwrites it)."""
    if args.bin_dir:
        binary = args.bin_dir / f"graphrt-{spec['name']}"
        if not binary.is_file():
            sys.exit(f"budget-gate: missing prebuilt binary {binary}")
        return binary
    command = shlex.split(args.build_cmd) + [
        "-p", "graphrt-cli", "--locked", "--profile", "min-size",
        "--no-default-features", "--features", spec["features"],
    ]
    if args.target:
        command += ["--target", args.target]
    print(f"budget-gate: {' '.join(command)}", file=sys.stderr, flush=True)
    if subprocess.run(command, cwd=ROOT).returncode != 0:
        sys.exit(2)
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    built = target_dir / (args.target or "") / "min-size" / "graphrt"
    copy = out_dir / f"graphrt-{spec['name']}"
    shutil.copy2(built, copy)
    return copy


def runs_here(binary: Path) -> bool:
    try:
        return subprocess.run([binary, "version"], capture_output=True, timeout=10).returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def healthz_ok(port: int) -> bool:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.05) as sock:
            sock.sendall(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            return sock.recv(64).startswith(b"HTTP/1.1 200")
    except OSError:
        return False


def memory_mib(pid: int) -> tuple[float, float | None]:
    """Total RSS and anonymous RSS of `pid` in MiB. Without /proc (macOS) only
    the total is known (from ps) and the anonymous figure is `None`."""
    status = Path(f"/proc/{pid}/status")
    if status.exists():
        fields = {}
        for line in status.read_text().splitlines():
            name, _, value = line.partition(":")
            if name in ("VmRSS", "RssAnon"):
                fields[name] = int(value.split()[0]) / 1024
        return fields["VmRSS"], fields.get("RssAnon")
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout
    return int(out.strip()) / 1024, None


def gateway_config(work: Path, port: int) -> Path:
    """A gateway with one deployment (no agent running): JSON, which is YAML."""
    config = {
        "listen": f"127.0.0.1:{port}",
        "store": str(work / "gateway.db"),
        "socket_dir": str(work / "sockets"),
        "deployments": [{"name": "shop", "keys_env": "BUDGET_GATE_KEYS", "api_keys": ["0" * 64]}],
    }
    path = work / f"gateway-{port}.yaml"
    path.write_text(json.dumps(config))
    return path


def serve_once(
    binary: Path, workflows: Path, measure_rss: bool, mode: str = "serve"
) -> tuple[float, tuple[float, float | None] | None]:
    """Start `graphrt serve` (or `graphrt gateway`), time exec -> healthy, optionally read idle memory, then SIGTERM it."""
    port = free_port()
    env = {k: v for k, v in os.environ.items() if not k.startswith("GRAPHRT_")}
    if mode == "gateway":
        command = [binary, "gateway", "--config", gateway_config(workflows.parent, port)]
        env["BUDGET_GATE_KEYS"] = "k1:" + "ab" * 32
    else:
        command = [binary, "serve", workflows, "--addr", f"127.0.0.1:{port}"]
    began = time.perf_counter()
    process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env)
    try:
        while not healthz_ok(port):
            if process.poll() is not None:
                sys.exit(f"budget-gate: {binary.name} {mode} exited: {process.stderr.read().decode()}")
            if time.perf_counter() - began > 10:
                sys.exit(f"budget-gate: {binary.name} {mode} did not become healthy in 10 s")
            time.sleep(0.0002)
        startup_ms = (time.perf_counter() - began) * 1000
        rss = None
        if measure_rss:
            time.sleep(1.0)
            rss = memory_mib(process.pid)
        process.send_signal(signal.SIGTERM)
        code = process.wait(timeout=10)
        if code != 0:
            sys.exit(f"budget-gate: {binary.name} exited {code} on SIGTERM: {process.stderr.read().decode()}")
        return startup_ms, rss
    finally:
        if process.poll() is None:
            process.kill()


def p95(values: list[float]) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, round(0.95 * len(ordered)) - 1))]


def banned_dependencies(rules: dict) -> list[str]:
    tree = subprocess.run(
        ["cargo", "tree", "--workspace", "--all-features", "--target", "all", "-e", "normal,build", "--prefix", "none"],
        cwd=ROOT, capture_output=True, text=True,
    )
    if tree.returncode != 0:
        sys.exit(f"budget-gate: cargo tree failed: {tree.stderr}")
    names = {line.split()[0] for line in tree.stdout.splitlines() if line.strip()}
    banned = set(rules.get("banned", []))
    prefixes = tuple(rules.get("banned_prefixes", []))
    return sorted(name for name in names if name in banned or (prefixes and name.startswith(prefixes)))


def main() -> int:
    args = parse_args()
    budgets = tomllib.loads(args.budgets.read_text())
    key = platform_key(args.target)
    target = args.target or f"{key} (host)"
    results: dict = {"target": target, "platform": key, "profile": "min-size", "builds": {}, "violations": []}
    work = Path(tempfile.mkdtemp(prefix="graphrt-budget-"))
    try:
        (work / "workflows").mkdir()
        (work / "workflows" / "hello.yaml").write_text(WORKFLOW)
        startup = budgets.get("startup", {})
        for spec in budgets["build"]:
            if spec.get("optional") and not wants_optional(spec, args):
                continue
            binary = build(spec, args, work)
            if args.keep_bins:
                args.keep_bins.mkdir(parents=True, exist_ok=True)
                shutil.copy2(binary, args.keep_bins / binary.name)
            size = binary.stat().st_size
            entry = {"features": spec["features"], "binary_bytes": size, "binary_mib": round(size / MIB, 3)}
            check(results, spec["name"], "binary", size / MIB, budget_for(spec.get("binary_mib"), key), "MiB")
            if "delta_of" in spec:
                base = results["builds"].get(spec["delta_of"])
                if base is None:
                    print(f"budget-gate: {spec['name']}: delta_of `{spec['delta_of']}` must come earlier", file=sys.stderr)
                    return 2
                delta = (size - base["binary_bytes"]) / 1024
                entry["delta_kib"] = round(delta, 1)
                entry["delta_of"] = spec["delta_of"]
                check(results, spec["name"], f"size over {spec['delta_of']}", delta, budget_for(spec.get("delta_kib"), key), "KiB")
            mode = spec.get("run", "serve")
            if not args.skip_run and mode != "none" and runs_here(binary):
                timed = spec["name"] == startup.get("build")
                rss_samples = int(startup.get("rss_samples", 5))
                samples = max(int(startup.get("samples", 30)), rss_samples) if timed else rss_samples
                times, rss_values, anon_values = [], [], []
                for i in range(samples):
                    ms, measured = serve_once(binary, work / "workflows", measure_rss=(i < rss_samples), mode=mode)
                    times.append(ms)
                    if measured is not None:
                        rss_values.append(measured[0])
                        if measured[1] is not None:
                            anon_values.append(measured[1])
                entry["idle_rss_mib"] = round(statistics.median(rss_values), 2)
                entry["idle_rss_samples"] = len(rss_values)
                if anon_values:
                    anon = statistics.median(anon_values)
                    entry["idle_anon_mib"] = round(anon, 2)
                    budget = budget_for(spec.get("idle_anon_mib"), key)
                    check(results, spec["name"], "idle anonymous memory", anon, budget, "MiB")
                if timed:
                    entry["healthz_p50_ms"] = round(sorted(times)[len(times) // 2], 2)
                    entry["healthz_p95_ms"] = round(p95(times), 2)
                    entry["healthz_samples"] = samples
                    check(results, spec["name"], "exec->/healthz p95", p95(times), budget_for(startup.get("healthz_p95_ms"), key), "ms")
            results["builds"][spec["name"]] = entry
        if not args.skip_deps and "dependencies" in budgets:
            found = banned_dependencies(budgets["dependencies"])
            results["banned_dependencies"] = found
            if found:
                results["violations"].append(f"banned crates in the dependency tree: {', '.join(found)}")
    finally:
        shutil.rmtree(work, ignore_errors=True)
    report(results, args)
    return 1 if results["violations"] else 0


def wants_optional(spec: dict, args: argparse.Namespace) -> bool:
    """An optional build is measured with --optional, or when --bin-dir has its binary."""
    if args.optional:
        return True
    return bool(args.bin_dir and (args.bin_dir / f"graphrt-{spec['name']}").is_file())


def platform_key(target: str | None) -> str:
    """`<arch>-<os>` of the measured binaries, e.g. `x86_64-linux`."""
    if target:
        arch, _, rest = target.partition("-")
        return f"{arch}-{'linux' if 'linux' in rest else rest.split('-')[-1]}"
    arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(platform.machine().lower(), platform.machine().lower())
    return f"{arch}-{platform.system().lower()}"


def budget_for(value: float | dict | None, key: str) -> float | None:
    """A budget that is a number, or a table keyed by platform with a `default`."""
    if isinstance(value, dict):
        return value.get(key, value.get("default"))
    return value


def check(results: dict, build_name: str, what: str, value: float, budget: float | None, unit: str) -> None:
    if budget is not None and value > budget:
        results["violations"].append(f"{build_name}: {what} {value:.2f} {unit} > budget {budget} {unit}")


def report(results: dict, args: argparse.Namespace) -> None:
    budgets = {b["name"]: b for b in tomllib.loads(args.budgets.read_text())["build"]}
    lines = [
        f"### graphrt budgets ({results['target']}, min-size)",
        "",
        "| Build | Features | Binary (MiB) | Budget | Idle anon (MiB) | Budget | Idle RSS (MiB) | exec→/healthz p50 / p95 (ms) |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    key = results["platform"]
    for name, entry in results["builds"].items():
        budget = {k: budget_for(v, key) for k, v in budgets.get(name, {}).items()}
        anon = entry.get("idle_anon_mib", "–")
        rss = entry.get("idle_rss_mib", "–")
        startup = f"{entry['healthz_p50_ms']} / {entry['healthz_p95_ms']}" if "healthz_p95_ms" in entry else "–"
        size_budget = budget.get("binary_mib", "–")
        if "delta_kib" in entry:
            size_budget = f"+{entry['delta_kib']} KiB over {entry['delta_of']} (budget +{budget.get('delta_kib', '–')})"
        lines.append(
            f"| {name} | `{entry['features']}` | {entry['binary_mib']} | {size_budget} "
            f"| {anon} | {budget.get('idle_anon_mib', '–')} | {rss} | {startup} |"
        )
    if "banned_dependencies" in results:
        lines += ["", f"Banned crates found: {', '.join(results['banned_dependencies']) or 'none'}"]
    lines += [""] + ([f"**FAIL** {v}" for v in results["violations"]] or ["All budgets met."])
    text = "\n".join(lines) + "\n"
    print(text)
    if args.markdown:
        args.markdown.write_text(text)
    if args.json:
        args.json.write_text(json.dumps(results, indent=2) + "\n")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as out:
            out.write(text)


if __name__ == "__main__":
    sys.exit(main())
