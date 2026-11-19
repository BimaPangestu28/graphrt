"""Combine the measurement JSON files of one run into <base>.json and <base>.md.

    python3 report.py WORK_DIR OUT_BASE

WORK_DIR holds machine.json, cold.json, seq.json, conc.json, footprint.json and,
after `run.sh --docker`, linux.json and docker_sizes.json.
"""

import json
import os
import sys

MIB = 1024


def load(work, name):
    path = os.path.join(work, name)
    if not os.path.exists(path):
        return None
    with open(path) as handle:
        return json.load(handle)


def ms(value):
    return f"{value:,.1f}" if value >= 10 else f"{value:.2f}" if value >= 0.1 else f"{value:.3f}"


def spread(row):
    return f"{ms(row['median_ms'])} ({ms(row['p25_ms'])}–{ms(row['p75_ms'])})"


def ratio(value, reference):
    return f"{value / reference:,.0f}×" if value / reference >= 10 else f"{value / reference:.1f}×"


def cold_table(cold, title):
    lines = [f"## {title}", "",
             f"Full process, start to exit, one run per process (hyperfine, {cold['hyperfine_runs']} runs after "
             f"{cold['warmup']} warm-up runs). Time: median (p25–p75). Peak RSS: median of "
             f"{cold['rows'][0]['rss']['runs']} runs.", "",
             "| Workflow | Stack | Time ms | vs graphrt | Peak RSS MiB |", "|---|---|---:|---:|---:|"]
    reference = {r["shape"]: r for r in cold["rows"] if r["side"] == "graphrt"}
    for row in cold["rows"]:
        base = reference.get(row["shape"])
        versus = ratio(row["time"]["median_ms"], base["time"]["median_ms"]) if base else "–"
        lines.append(f"| {row['shape']} | {row['side']} | {spread(row['time'])} | {versus} | "
                     f"{row['rss']['median_kib'] / MIB:.1f} |")
    return lines


def size_of(shape):
    return int(shape.split("-")[1]) if "-" in shape else None


def seq_tables(seq):
    rows = seq["rows"]
    lines = ["## Warm in-process latency", "",
             "Graph compiled once, runs one after another in one process after 20 warm-up runs. "
             "graphrt: current-thread Tokio runtime. Python: one asyncio loop (`ainvoke`), or plain "
             "`invoke` for `langgraph-sync`. Median (p25–p75).", "",
             "### 3-node LLM chain (fake server, 0 ms latency)", "",
             "| Stack | ms / run | vs graphrt |", "|---|---:|---:|"]
    llm = [r for r in rows if r["shape"] == "llm3"]
    reference = next(r for r in llm if r["side"] == "graphrt")
    for row in llm:
        lines.append(f"| {row['side']} | {spread(row)} | {ratio(row['median_ms'], reference['median_ms'])} |")
    lines += ["", "### Orchestration overhead (passthrough nodes)", "",
              "µs / node = median run time / N. Marginal = (time at 1,000 − time at 10) / 990, which "
              "removes fixed per-run cost.", "",
              "| Shape | Stack | ms / run | µs / node | Marginal µs / node | vs graphrt (marginal) |",
              "|---|---|---:|---:|---:|---:|"]
    by_key = {(r["shape"], r["side"]): r for r in rows}
    for kind in ("chain", "fanout"):
        marginal = {}
        for side in ("graphrt", "langgraph", "langgraph-sync", "lcel"):
            small, large = by_key.get((f"{kind}-10", side)), by_key.get((f"{kind}-1000", side))
            if small and large:
                marginal[side] = (large["median_ms"] - small["median_ms"]) * 1000 / 990
        for size in (10, 1000):
            for side in ("graphrt", "langgraph", "langgraph-sync", "lcel"):
                row = by_key.get((f"{kind}-{size}", side))
                if not row:
                    continue
                per_node = row["median_ms"] * 1000 / size
                extra = ("–", "–")
                if size == 1000 and side in marginal:
                    extra = (f"{marginal[side]:,.2f}", ratio(marginal[side], marginal["graphrt"]))
                lines.append(f"| {kind}-{size} | {side} | {spread(row)} | {per_node:,.2f} | {extra[0]} | {extra[1]} |")
    return lines


def conc_table(conc):
    lines = ["## Concurrent throughput (3-node LLM chain)", "",
             "C runs kept in flight until the round's total is done; median of 3 rounds (min–max) after "
             "a warm-up round. Python: one asyncio event loop. graphrt: multi-thread Tokio runtime, all "
             "cores or one worker thread. Peak RSS: whole driver process.", "",
             "| Server latency | Concurrent runs | Stack | Runs / s | Ideal | Peak RSS MiB |",
             "|---:|---:|---|---:|---:|---:|"]
    for row in conc["rows"]:
        ideal = f"{row['concurrency'] / (row['delay_ms'] / 1000):,.0f}" if row["delay_ms"] else "–"
        rss = f"{row['peak_rss_kib'] / MIB:.1f}" if row.get("peak_rss_kib") else "–"
        lines.append(f"| {row['delay_ms']} ms | {row['concurrency']:,} | {row['side']} | "
                     f"{row['runs_per_sec_median']:,.0f} ({row['runs_per_sec_min']:,.0f}–{row['runs_per_sec_max']:,.0f}) | "
                     f"{ideal} | {rss} |")
    return lines


def footprint_table(footprint, docker_sizes):
    lines = ["## Install footprint", "", "| What | Size | Dependencies |", "|---|---:|---:|"]
    labels = {"default": "graphrt binary, default features (min-size)",
              "llm-only": "graphrt binary, `--no-default-features --features llm` (min-size)"}
    for label, size in footprint["binaries"].items():
        lines.append(f"| {labels.get(label, label)} | {size / 1048576:.2f} MiB | 0 (static binary, no runtime) |")
    lines.append(f"| Python venv: langgraph + langchain-openai, excluding pip | "
                 f"{footprint['site_packages_without_pip_kib'] / MIB:.1f} MiB | {footprint['distributions']} distributions |")
    lines.append(f"| Python venv, whole directory | {footprint['venv_kib'] / MIB:.1f} MiB | "
                 f"+ a {footprint['python']} interpreter |")
    if docker_sizes:
        for label, size in docker_sizes.items():
            lines.append(f"| Container image: {label} | {size / 1048576:.1f} MiB | – |")
    return lines


def main():
    work, base = sys.argv[1], sys.argv[2]
    result = {name: load(work, f"{name}.json")
              for name in ("machine", "cold", "seq", "conc", "footprint", "linux", "docker_sizes")}
    with open(base + ".json", "w") as handle:
        json.dump({k: v for k, v in result.items() if v is not None}, handle, indent=2)

    machine = result["machine"]
    lines = [f"# graphrt vs Python agent stacks — {os.path.basename(base)}", "",
             f"{machine['cpu']}, {machine['logical_cpus']} logical CPUs, {machine['memory_gib']} GiB RAM, "
             f"{machine['os']}, {machine['rustc']}, {machine['graphrt']}, commit {machine['git_commit']}.", "",
             "Python: " + ", ".join(f"{k} {v}" for k, v in machine["python_packages"].items()) + ".", "",
             "Every model call goes to the same local fake OpenAI-compatible server (`rust/src/bin/fakeoai.rs`), "
             "which streams 10 deltas. Shared, noisy machine: treat differences under ~20% as noise.", ""]
    lines += cold_table(result["cold"], "Cold start to first result (host)") + [""]
    lines += seq_tables(result["seq"]) + [""]
    lines += conc_table(result["conc"]) + [""]
    if result["linux"]:
        env = result["linux"]["environment"]
        lines += cold_table(result["linux"], f"Cold start and peak RSS in a Linux container ({env['os']}, "
                                             f"Python {env['python']}, `--cpus 1 --memory 1g`)") + [""]
    lines += footprint_table(result["footprint"], result["docker_sizes"]) + [""]
    with open(base + ".md", "w") as handle:
        handle.write("\n".join(lines))
    print(base + ".json")
    print(base + ".md")


if __name__ == "__main__":
    main()
