"""Python driver, the counterpart of rust/src/bin/graphrt_driver.rs.

    python driver.py once <framework> <shape>                 # one run, print the result (cold start)
    python driver.py seq  <framework> <shape> <iterations>    # warm sequential latency
    python driver.py conc <framework> <shape> <concurrency> <total>   # async throughput

The graph is compiled once and reused across runs. Async frameworks run on
one asyncio event loop (the default loop, as LangGraph's docs use); C runs are
kept in flight by C worker coroutines. Prints one JSON object on stdout.
"""

import asyncio
import json
import os
import sys
import time

# Tracing off explicitly, whatever the caller's environment says.
for _name in ("LANGCHAIN_API_KEY", "LANGSMITH_API_KEY", "LANGCHAIN_ENDPOINT", "LANGSMITH_ENDPOINT"):
    os.environ.pop(_name, None)
os.environ["LANGCHAIN_TRACING_V2"] = "false"
os.environ["LANGSMITH_TRACING"] = "false"
os.environ["LANGSMITH_TRACING_V2"] = "false"

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import graphs  # noqa: E402

WARMUP_RUNS = 20
THROUGHPUT_ROUNDS = 3
RUN_INPUT = {"x": "hello"}
RECURSION_CONFIG = {"recursion_limit": 100_000}


def is_sync(framework):
    return framework == "langgraph-sync"


def run_config(framework):
    return RECURSION_CONFIG if framework.startswith("langgraph") else None


def summarize_ms(samples):
    samples = sorted(samples)
    at = lambda q: samples[round((len(samples) - 1) * q)]  # noqa: E731
    return {
        "runs": len(samples),
        "median_ms": at(0.5), "p25_ms": at(0.25), "p75_ms": at(0.75),
        "min_ms": samples[0], "max_ms": samples[-1],
    }


def check_result(result):
    if not result:
        raise RuntimeError(f"empty result: {result!r}")
    return result


def once(framework, shape):
    runnable = graphs.build(framework, shape)
    config = run_config(framework)
    if is_sync(framework):
        result = runnable.invoke(RUN_INPUT, config=config)
    else:
        result = asyncio.run(runnable.ainvoke(RUN_INPUT, config=config))
    check_result(result)
    return {"ok": True}


def sequential(framework, shape, iterations):
    runnable = graphs.build(framework, shape)
    config = run_config(framework)
    samples = []
    if is_sync(framework):
        for _ in range(WARMUP_RUNS):
            check_result(runnable.invoke(RUN_INPUT, config=config))
        for _ in range(iterations):
            started = time.perf_counter()
            check_result(runnable.invoke(RUN_INPUT, config=config))
            samples.append((time.perf_counter() - started) * 1000)
        return summarize_ms(samples)

    async def measure():
        for _ in range(WARMUP_RUNS):
            check_result(await runnable.ainvoke(RUN_INPUT, config=config))
        for _ in range(iterations):
            started = time.perf_counter()
            check_result(await runnable.ainvoke(RUN_INPUT, config=config))
            samples.append((time.perf_counter() - started) * 1000)

    asyncio.run(measure())
    return summarize_ms(samples)


async def throughput_round(runnable, config, concurrency, total):
    remaining = [total]

    async def worker():
        while remaining[0] > 0:
            remaining[0] -= 1
            check_result(await runnable.ainvoke(RUN_INPUT, config=config))

    started = time.perf_counter()
    await asyncio.gather(*(worker() for _ in range(concurrency)))
    return total / (time.perf_counter() - started)


def concurrent(framework, shape, concurrency, total):
    if is_sync(framework):
        raise ValueError("throughput is measured with the async API only")
    runnable = graphs.build(framework, shape)
    config = run_config(framework)

    async def measure():
        await throughput_round(runnable, config, concurrency, max(concurrency, WARMUP_RUNS))
        return [await throughput_round(runnable, config, concurrency, total) for _ in range(THROUGHPUT_ROUNDS)]

    rounds = sorted(asyncio.run(measure()))
    return {
        "concurrency": concurrency, "total_runs": total,
        "runs_per_sec_median": rounds[len(rounds) // 2],
        "runs_per_sec_min": rounds[0], "runs_per_sec_max": rounds[-1],
    }


def main(argv):
    usage = "usage: driver.py once FW SHAPE | seq FW SHAPE N | conc FW SHAPE C TOTAL"
    if len(argv) < 3:
        sys.exit(usage)
    mode, framework, shape = argv[0], argv[1], argv[2]
    if mode == "once" and len(argv) == 3:
        result = once(framework, shape)
    elif mode == "seq" and len(argv) == 4:
        result = sequential(framework, shape, int(argv[3]))
    elif mode == "conc" and len(argv) == 5:
        result = concurrent(framework, shape, int(argv[3]), int(argv[4]))
    else:
        sys.exit(usage)
    print(json.dumps(result))


if __name__ == "__main__":
    main(sys.argv[1:])
