"""Measurements for the graphrt vs Python comparison (stdlib only).

    python3 measure.py cold   --tools TOOLS.json --out cold.json   # hyperfine + peak RSS per process
    python3 measure.py seq    --tools TOOLS.json --out seq.json    # warm in-process latency per shape
    python3 measure.py conc   --tools TOOLS.json --out conc.json   # async throughput vs the fake server
    python3 measure.py footprint --tools TOOLS.json --out footprint.json

TOOLS.json names the binaries and directories:

    {"graphrt": "...", "graphrt_driver": "...", "fakeoai": "...", "python": "...",
     "workflows": "<dir with llm3.yaml, chain-N.json, fanout-N.json>",
     "venv": "<venv dir, footprint only>", "binaries": {"label": "path"}}

Every subcommand starts and stops its own fake OpenAI server, so both sides talk
to the same process with the same latency. Works on macOS (`/usr/bin/time -l`)
and Linux (GNU `/usr/bin/time -v`).
"""

import argparse
import contextlib
import json
import os
import platform
import shlex
import socket
import statistics
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DRIVER_PY = os.path.join(HERE, "python", "driver.py")
FAKE_ADDRESS = ("127.0.0.1", 9911)
PYTHON_FRAMEWORKS = ("langgraph", "lcel")


def bench_env():
    """Environment for every measured process: fake server, no tracing, no telemetry."""
    env = dict(os.environ)
    for name in ("LANGCHAIN_API_KEY", "LANGSMITH_API_KEY", "LANGCHAIN_ENDPOINT", "LANGSMITH_ENDPOINT"):
        env.pop(name, None)
    env.update({
        "OPENAI_BASE_URL": f"http://{FAKE_ADDRESS[0]}:{FAKE_ADDRESS[1]}/v1",
        "OPENAI_API_KEY": "sk-fake",
        "LANGCHAIN_TRACING_V2": "false",
        "LANGSMITH_TRACING": "false",
        "GRAPHRT_TELEMETRY": "0",
        "DO_NOT_TRACK": "1",
    })
    return env


def wait_for_port(address, timeout_s=10.0):
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        with contextlib.suppress(OSError), socket.create_connection(address, timeout=0.2):
            return
        time.sleep(0.05)
    raise RuntimeError(f"fake server did not start on {address}")


@contextlib.contextmanager
def fake_server(fakeoai, delay_ms):
    address = f"{FAKE_ADDRESS[0]}:{FAKE_ADDRESS[1]}"
    process = subprocess.Popen([fakeoai, address, str(delay_ms)], stderr=subprocess.DEVNULL)
    try:
        wait_for_port(FAKE_ADDRESS)
        yield
    finally:
        process.terminate()
        process.wait()


def workflow_path(tools, shape):
    name = "llm3.yaml" if shape == "llm3" else f"{shape}.json"
    return os.path.join(tools["workflows"], name)


def cold_commands(tools, shape):
    """Full-process commands that run `shape` once and exit."""
    graphrt = [tools["graphrt"], "run", workflow_path(tools, shape), "--input", "x=hello",
               "--no-default-plugins", "--allow-private-network", "--output", "json"]
    commands = {"graphrt": graphrt}
    for framework in PYTHON_FRAMEWORKS:
        commands[framework] = [tools["python"], DRIVER_PY, "once", framework, shape]
    return commands


def baseline_commands(tools):
    """Context rows: the interpreter alone and the imports alone."""
    return {
        "python -c pass": [tools["python"], "-c", "pass"],
        "import langgraph + langchain_openai": [tools["python"], "-c", "import langgraph.graph, langchain_openai"],
        "graphrt version": [tools["graphrt"], "version"],
    }


def hyperfine(command, runs, warmup):
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as handle:
        export = handle.name
    try:
        subprocess.run(
            ["hyperfine", "--shell=none", "--style", "none", "--warmup", str(warmup), "--runs", str(runs),
             "--export-json", export, shlex.join(command)],
            check=True, env=bench_env(), stdout=subprocess.DEVNULL,
        )
        with open(export) as exported:
            result = json.load(exported)["results"][0]
    finally:
        os.unlink(export)
    times_ms = sorted(t * 1000 for t in result["times"])
    quartiles = statistics.quantiles(times_ms, n=4)
    return {"runs": len(times_ms), "median_ms": statistics.median(times_ms), "p25_ms": quartiles[0],
            "p75_ms": quartiles[2], "min_ms": times_ms[0], "max_ms": times_ms[-1],
            "stddev_ms": statistics.stdev(times_ms)}


def peak_rss_kib(command):
    """Peak RSS of one run of `command`, in KiB."""
    if platform.system() == "Darwin":
        timed = ["/usr/bin/time", "-l", *command]
    else:
        timed = ["/usr/bin/time", "-v", *command]
    completed = subprocess.run(timed, capture_output=True, text=True, env=bench_env())
    if completed.returncode != 0:
        raise RuntimeError(f"{shlex.join(command)} failed: {completed.stderr[-2000:]}")
    for line in completed.stderr.splitlines():
        line = line.strip()
        if line.endswith("maximum resident set size"):  # macOS: bytes
            return int(line.split()[0]) / 1024
        if line.startswith("Maximum resident set size (kbytes):"):  # GNU time: KiB
            return float(line.rsplit(":", 1)[1])
    raise RuntimeError(f"no RSS in /usr/bin/time output of {shlex.join(command)}")


def median_rss(command, repeats):
    samples = sorted(peak_rss_kib(command) for _ in range(repeats))
    return {"median_kib": statistics.median(samples), "min_kib": samples[0], "max_kib": samples[-1],
            "runs": repeats}


def measure_cold(tools, args):
    rows = []
    with fake_server(tools["fakeoai"], 0):
        for shape in ("llm3", "chain-1000"):
            for side, command in cold_commands(tools, shape).items():
                print(f"  cold {shape} {side}", file=sys.stderr)
                rows.append({"shape": shape, "side": side, "time": hyperfine(command, args.runs, args.warmup),
                             "rss": median_rss(command, args.rss_repeats)})
        for label, command in baseline_commands(tools).items():
            print(f"  baseline {label}", file=sys.stderr)
            rows.append({"shape": "baseline", "side": label, "time": hyperfine(command, args.runs, args.warmup),
                         "rss": median_rss(command, args.rss_repeats)})
    return {"rows": rows, "hyperfine_runs": args.runs, "warmup": args.warmup}


def run_json(command):
    completed = subprocess.run(command, capture_output=True, text=True, env=bench_env())
    if completed.returncode != 0:
        raise RuntimeError(f"{shlex.join(command)} failed: {completed.stderr[-2000:]}")
    return json.loads(completed.stdout.strip().splitlines()[-1])


# Iterations per process: (graphrt, python). The 1,000-node shapes take ~0.5 ms
# per run in graphrt and 100+ ms in Python.
SEQ_SHAPES = {
    "llm3": (2000, 300),
    "chain-10": (5000, 500),
    "chain-1000": (500, 30),
    "fanout-10": (5000, 500),
    "fanout-1000": (500, 30),
}


def seq_commands(tools, shape, graphrt_iterations, python_iterations):
    commands = {"graphrt": [tools["graphrt_driver"], "seq", workflow_path(tools, shape), str(graphrt_iterations)]}
    frameworks = PYTHON_FRAMEWORKS if shape == "llm3" else ("langgraph", "langgraph-sync", "lcel")
    for framework in frameworks:
        commands[framework] = [tools["python"], DRIVER_PY, "seq", framework, shape, str(python_iterations)]
    return commands


def measure_seq(tools, _args):
    rows = []
    with fake_server(tools["fakeoai"], 0):
        for shape, (graphrt_iterations, python_iterations) in SEQ_SHAPES.items():
            for side, command in seq_commands(tools, shape, graphrt_iterations, python_iterations).items():
                print(f"  seq {shape} {side}", file=sys.stderr)
                rows.append({"shape": shape, "side": side, **run_json(command)})
    return {"rows": rows}


# (fake-server delay ms, concurrency, total runs per round)
CONC_CASES = [(50, 1, 40), (50, 10, 400), (50, 100, 2000), (50, 1000, 5000), (0, 100, 3000)]


def conc_commands(tools, concurrency, total):
    workflow = workflow_path(tools, "llm3")
    driver = [tools["graphrt_driver"], "conc", workflow, str(concurrency), str(total)]
    commands = {"graphrt (all cores)": (driver, {}), "graphrt (1 worker thread)": (driver, {"GRAPHRT_DRIVER_WORKERS": "1"})}
    for framework in PYTHON_FRAMEWORKS:
        commands[framework] = ([tools["python"], DRIVER_PY, "conc", framework, "llm3", str(concurrency), str(total)], {})
    return commands


def run_json_with_rss(command, extra_env):
    """Run a driver under /usr/bin/time; return its JSON line and peak RSS (KiB)."""
    env = {**bench_env(), **extra_env}
    flag = "-l" if platform.system() == "Darwin" else "-v"
    with tempfile.TemporaryFile("w+") as stderr:
        completed = subprocess.run(["/usr/bin/time", flag, *command], stdout=subprocess.PIPE, stderr=stderr,
                                   text=True, env=env)
        stderr.seek(0)
        err = stderr.read()
    if completed.returncode != 0:
        raise RuntimeError(f"{shlex.join(command)} failed: {err[-2000:]}")
    rss = None
    for line in err.splitlines():
        line = line.strip()
        if line.endswith("maximum resident set size"):
            rss = int(line.split()[0]) / 1024
        elif line.startswith("Maximum resident set size (kbytes):"):
            rss = float(line.rsplit(":", 1)[1])
    return {**json.loads(completed.stdout.strip().splitlines()[-1]), "peak_rss_kib": rss}


def measure_conc(tools, _args):
    rows = []
    for delay_ms, concurrency, total in CONC_CASES:
        with fake_server(tools["fakeoai"], delay_ms):
            for side, (command, extra_env) in conc_commands(tools, concurrency, total).items():
                # graphrt finishes 5,000 runs in well under a second; give it more work per round.
                if side.startswith("graphrt") and delay_ms == 0:
                    command = [*command[:-1], str(total * 10)]
                print(f"  conc delay={delay_ms} c={concurrency} {side}", file=sys.stderr)
                rows.append({"delay_ms": delay_ms, "side": side, **run_json_with_rss(command, extra_env)})
    return {"rows": rows}


def directory_kib(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            full = os.path.join(root, name)
            if not os.path.islink(full):
                total += os.lstat(full).st_blocks * 512
    return total / 1024


def measure_footprint(tools, _args):
    venv = tools["venv"]
    python = os.path.join(venv, "bin", "python")
    listing = json.loads(subprocess.run([python, "-m", "pip", "list", "--format", "json"],
                                        capture_output=True, text=True, check=True).stdout)
    tooling = {"pip", "setuptools", "wheel"}
    packages = sorted(p["name"] for p in listing if p["name"].lower() not in tooling)
    site_packages = subprocess.run([python, "-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"],
                                   capture_output=True, text=True, check=True).stdout.strip()
    tooling_kib = sum(directory_kib(os.path.join(site_packages, entry)) for entry in os.listdir(site_packages)
                      if entry.split("-")[0].lower() in tooling)
    return {
        "binaries": {label: os.path.getsize(path) for label, path in tools["binaries"].items()},
        "venv_kib": directory_kib(venv),
        "site_packages_kib": directory_kib(site_packages),
        "site_packages_without_pip_kib": directory_kib(site_packages) - tooling_kib,
        "distributions": len(packages),
        "distribution_names": packages,
        "python": subprocess.run([python, "--version"], capture_output=True, text=True).stdout.strip(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("section", choices=["cold", "seq", "conc", "footprint"])
    parser.add_argument("--tools", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--runs", type=int, default=30, help="hyperfine runs per cold-start command")
    parser.add_argument("--warmup", type=int, default=3, help="hyperfine warm-up runs")
    parser.add_argument("--rss-repeats", type=int, default=5)
    args = parser.parse_args()
    with open(args.tools) as handle:
        tools = json.load(handle)
    sections = {"cold": measure_cold, "seq": measure_seq, "conc": measure_conc, "footprint": measure_footprint}
    result = sections[args.section](tools, args)
    with open(args.out, "w") as handle:
        json.dump(result, handle, indent=2)


if __name__ == "__main__":
    main()
