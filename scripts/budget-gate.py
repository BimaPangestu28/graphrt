#!/usr/bin/env python3
"""Measure graphrt builds against bench/budgets.toml and fail when one is exceeded.

For every [[build]] in the budgets file this builds graphrt-cli with
`--profile min-size --no-default-features --features <features>` (or takes a
prebuilt binary from --bin-dir), then measures:

- binary size;
- idle RSS of `graphrt serve` one second after it became healthy;
- exec -> first 200 on GET /healthz, p95 over [startup].samples starts;
- that SIGTERM makes the server exit 0 (the drain path; a PID-1 run is covered
  by scripts/test-pid1.sh).

Run-time measurements need a binary that runs on this machine; a cross build
gets its size checked only (--skip-run is implied when it cannot execute).
It also checks that no banned crate is in the dependency tree.

Usage:
  scripts/budget-gate.py [--target TRIPLE] [--build-cmd "cargo zigbuild"]
                         [--bin-dir DIR] [--keep-bins DIR] [--skip-run] [--skip-deps]
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


def rss_mib(pid: int) -> float:
    status = Path(f"/proc/{pid}/status")
    if status.exists():
        for line in status.read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout
    return int(out.strip()) / 1024


def serve_once(binary: Path, workflows: Path, measure_rss: bool) -> tuple[float, float | None]:
    """Start `graphrt serve`, time exec -> healthy, optionally read idle RSS, then SIGTERM it."""
    port = free_port()
    env = {k: v for k, v in os.environ.items() if not k.startswith("GRAPHRT_")}
    began = time.perf_counter()
    process = subprocess.Popen(
        [binary, "serve", workflows, "--addr", f"127.0.0.1:{port}"],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env,
    )
    try:
        while not healthz_ok(port):
            if process.poll() is not None:
                sys.exit(f"budget-gate: {binary.name} serve exited: {process.stderr.read().decode()}")
            if time.perf_counter() - began > 10:
                sys.exit(f"budget-gate: {binary.name} serve did not become healthy in 10 s")
            time.sleep(0.0002)
        startup_ms = (time.perf_counter() - began) * 1000
        rss = None
        if measure_rss:
            time.sleep(1.0)
            rss = rss_mib(process.pid)
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
            binary = build(spec, args, work)
            if args.keep_bins:
                args.keep_bins.mkdir(parents=True, exist_ok=True)
                shutil.copy2(binary, args.keep_bins / binary.name)
            size = binary.stat().st_size
            entry = {"features": spec["features"], "binary_bytes": size, "binary_mib": round(size / MIB, 3)}
            check(results, spec["name"], "binary", size / MIB, budget_for(spec.get("binary_mib"), key), "MiB")
            if not args.skip_run and runs_here(binary):
                timed = spec["name"] == startup.get("build")
                samples = int(startup.get("samples", 30)) if timed else 1
                times, rss = [], None
                for i in range(samples):
                    ms, measured = serve_once(binary, work / "workflows", measure_rss=(i == 0))
                    times.append(ms)
                    rss = measured if measured is not None else rss
                entry["idle_rss_mib"] = round(rss, 2)
                check(results, spec["name"], "idle RSS", rss, budget_for(spec.get("idle_rss_mib"), key), "MiB")
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
        "| Build | Features | Binary (MiB) | Budget | Idle RSS (MiB) | Budget | exec→/healthz p50 / p95 (ms) |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    key = results["platform"]
    for name, entry in results["builds"].items():
        budget = {k: budget_for(v, key) for k, v in budgets.get(name, {}).items()}
        rss = entry.get("idle_rss_mib", "–")
        startup = f"{entry['healthz_p50_ms']} / {entry['healthz_p95_ms']}" if "healthz_p95_ms" in entry else "–"
        lines.append(
            f"| {name} | `{entry['features']}` | {entry['binary_mib']} | {budget.get('binary_mib', '–')} "
            f"| {rss} | {budget.get('idle_rss_mib', '–')} | {startup} |"
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
