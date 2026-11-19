# graphrt vs Python agent stacks — 2026-11-19-Bima-Pangestu-vs-python

Apple M3 Pro, 11 logical CPUs, 18.0 GiB RAM, Darwin 25.6.0 arm64, rustc 1.95.0 (59807616e 2026-04-14), graphrt 0.2.0, commit 90d4db0.

Python: python 3.11.11, langgraph 1.2.23, langchain-core 1.8.3, langchain-openai 1.8.3, openai 3.55.1, httpx 0.28.1, pydantic 2.14.3.

Every model call goes to the same local fake OpenAI-compatible server (`rust/src/bin/fakeoai.rs`), which streams 10 deltas. Shared, noisy machine: treat differences under ~20% as noise.

## Cold start to first result (host)

Full process, start to exit, one run per process (hyperfine, 30 runs after 3 warm-up runs). Time: median (p25–p75). Peak RSS: median of 5 runs.

| Workflow | Stack | Time ms | vs graphrt | Peak RSS MiB |
|---|---|---:|---:|---:|
| llm3 | graphrt | 5.28 (5.19–5.60) | 1.0× | 12.7 |
| llm3 | langgraph | 727.9 (705.7–758.2) | 138× | 101.3 |
| llm3 | lcel | 618.7 (614.1–623.5) | 117× | 94.1 |
| chain-1000 | graphrt | 51.7 (51.5–51.8) | 1.0× | 23.9 |
| chain-1000 | langgraph | 734.3 (686.1–769.5) | 14× | 78.7 |
| chain-1000 | lcel | 551.7 (539.4–619.2) | 11× | 64.4 |
| baseline | python -c pass | 10.1 (9.89–10.5) | – | 11.5 |
| baseline | import langgraph + langchain_openai | 640.8 (614.2–692.8) | – | 94.3 |
| baseline | graphrt version | 3.43 (3.39–3.47) | – | 8.3 |

## Warm in-process latency

Graph compiled once, runs one after another in one process after 20 warm-up runs. graphrt: current-thread Tokio runtime. Python: one asyncio loop (`ainvoke`), or plain `invoke` for `langgraph-sync`. Median (p25–p75).

### 3-node LLM chain (fake server, 0 ms latency)

| Stack | ms / run | vs graphrt |
|---|---:|---:|
| graphrt | 0.069 (0.065–0.073) | 1.0× |
| langgraph | 3.09 (3.06–3.18) | 45× |
| lcel | 2.98 (2.91–3.12) | 43× |

### Orchestration overhead (passthrough nodes)

µs / node = median run time / N. Marginal = (time at 1,000 − time at 10) / 990, which removes fixed per-run cost.

| Shape | Stack | ms / run | µs / node | Marginal µs / node | vs graphrt (marginal) |
|---|---|---:|---:|---:|---:|
| chain-10 | graphrt | 0.007 (0.007–0.007) | 0.70 | – | – |
| chain-10 | langgraph | 1.83 (1.83–1.85) | 183.46 | – | – |
| chain-10 | langgraph-sync | 0.97 (0.96–0.98) | 96.56 | – | – |
| chain-10 | lcel | 1.86 (1.85–1.87) | 186.25 | – | – |
| chain-1000 | graphrt | 0.38 (0.38–0.38) | 0.38 | 0.37 | 1.0× |
| chain-1000 | langgraph | 214.7 (213.7–219.5) | 214.73 | 215.04 | 576× |
| chain-1000 | langgraph-sync | 135.3 (134.7–138.0) | 135.28 | 135.67 | 364× |
| chain-1000 | lcel | 179.7 (179.4–181.8) | 179.71 | 179.64 | 482× |
| fanout-10 | graphrt | 0.008 (0.008–0.009) | 0.85 | – | – |
| fanout-10 | langgraph | 1.19 (1.18–1.20) | 118.65 | – | – |
| fanout-10 | langgraph-sync | 1.26 (1.21–1.30) | 126.06 | – | – |
| fanout-10 | lcel | 1.34 (1.34–1.35) | 134.21 | – | – |
| fanout-1000 | graphrt | 0.51 (0.51–0.51) | 0.51 | 0.50 | 1.0× |
| fanout-1000 | langgraph | 106.2 (103.9–108.0) | 106.23 | 106.11 | 210× |
| fanout-1000 | langgraph-sync | 111.8 (87.9–117.3) | 111.84 | 111.70 | 221× |
| fanout-1000 | lcel | 98.1 (96.5–102.4) | 98.09 | 97.72 | 194× |

## Concurrent throughput (3-node LLM chain)

C runs kept in flight until the round's total is done; median of 3 rounds (min–max) after a warm-up round. Python: one asyncio event loop. graphrt: multi-thread Tokio runtime, all cores or one worker thread. Peak RSS: whole driver process.

| Server latency | Concurrent runs | Stack | Runs / s | Ideal | Peak RSS MiB |
|---:|---:|---|---:|---:|---:|
| 50 ms | 1 | graphrt (all cores) | 19 (19–19) | 20 | 10.3 |
| 50 ms | 1 | graphrt (1 worker thread) | 19 (19–19) | 20 | 10.0 |
| 50 ms | 1 | langgraph | 15 (15–16) | 20 | 101.2 |
| 50 ms | 1 | lcel | 16 (16–16) | 20 | 94.1 |
| 50 ms | 10 | graphrt (all cores) | 186 (186–186) | 200 | 12.8 |
| 50 ms | 10 | graphrt (1 worker thread) | 188 (187–188) | 200 | 10.7 |
| 50 ms | 10 | langgraph | 151 (150–157) | 200 | 102.9 |
| 50 ms | 10 | lcel | 149 (144–156) | 200 | 95.4 |
| 50 ms | 100 | graphrt (all cores) | 1,821 (1,818–1,843) | 2,000 | 19.3 |
| 50 ms | 100 | graphrt (1 worker thread) | 1,837 (1,835–1,844) | 2,000 | 18.0 |
| 50 ms | 100 | langgraph | 308 (295–315) | 2,000 | 114.2 |
| 50 ms | 100 | lcel | 338 (337–344) | 2,000 | 103.8 |
| 50 ms | 1,000 | graphrt (all cores) | 17,248 (16,770–17,345) | 20,000 | 81.4 |
| 50 ms | 1,000 | graphrt (1 worker thread) | 16,498 (16,417–16,551) | 20,000 | 75.9 |
| 50 ms | 1,000 | langgraph | 213 (206–214) | 20,000 | 222.8 |
| 50 ms | 1,000 | lcel | 239 (238–243) | 20,000 | 184.7 |
| 0 ms | 100 | graphrt (all cores) | 74,461 (73,977–77,717) | – | 19.6 |
| 0 ms | 100 | graphrt (1 worker thread) | 24,203 (20,663–24,393) | – | 16.6 |
| 0 ms | 100 | langgraph | 281 (160–293) | – | 116.6 |
| 0 ms | 100 | lcel | 289 (279–317) | – | 103.8 |

## Cold start and peak RSS in a Linux container (Linux 6.10.14-linuxkit aarch64, Python 3.11.18, `--cpus 1 --memory 1g`)

Full process, start to exit, one run per process (hyperfine, 30 runs after 3 warm-up runs). Time: median (p25–p75). Peak RSS: median of 5 runs.

| Workflow | Stack | Time ms | vs graphrt | Peak RSS MiB |
|---|---|---:|---:|---:|
| llm3 | graphrt | 6.52 (5.75–10.3) | 1.0× | 8.8 |
| llm3 | langgraph | 803.2 (689.2–913.4) | 123× | 96.4 |
| llm3 | lcel | 534.1 (526.9–553.7) | 82× | 89.5 |
| chain-1000 | graphrt | 99.6 (82.7–107.4) | 1.0× | 15.9 |
| chain-1000 | langgraph | 1,221.1 (1,135.6–1,292.0) | 12× | 74.3 |
| chain-1000 | lcel | 381.4 (357.8–411.3) | 3.8× | 59.8 |
| baseline | python -c pass | 5.87 (5.62–8.39) | – | 6.9 |
| baseline | import langgraph + langchain_openai | 546.4 (535.3–552.6) | – | 90.2 |
| baseline | graphrt version | 0.35 (0.33–0.37) | – | 4.1 |

## Install footprint

| What | Size | Dependencies |
|---|---:|---:|
| graphrt binary, default features (min-size) | 13.01 MiB | 0 (static binary, no runtime) |
| graphrt binary, `--no-default-features --features llm` (min-size) | 4.32 MiB | 0 (static binary, no runtime) |
| Python venv: langgraph + langchain-openai, excluding pip | 88.5 MiB | 43 distributions |
| Python venv, whole directory | 108.8 MiB | + a Python 3.11.11 interpreter |
| Container image: graphrt (distroless static) | 16.8 MiB | – |
| Container image: python:3.11-slim + requirements.txt | 245.2 MiB | – |
