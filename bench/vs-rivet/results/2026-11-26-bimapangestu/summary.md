# graphrt vs Rivet: medians across runs

Generated from `results/2026-11-26-bimapangestu`. Cells: median across runs (min–max across runs).

graphrt 0.6.2 (core, llm, http, code, python, mcp, doc, rag, eval, bundle, plugins, wasm, otel, serve, trust-gateway, scores, online-eval, langfuse, telemetry, gateway, deploy-fly, mimalloc) · rivetdev/engine:3.1.1 + rivetkit 3.1.1 · Node.js v24.15.0 · 13th Gen Intel(R) Core(TM) i7-13700 (24 threads), 61.6 GiB · Linux 7.0.0-44-generic

## cold

| metric | graphrt | rivet | rivet (ws) |
|---|---:|---:|---:|
| wake → first token p50 (ms) | 7.83 (7.79–7.90) | 21.5 (19.0–25.8) | 19.6 (19.0–20.9) |
| wake → first token p95 (ms) | 12.3 (11.5–12.7) | 28.8 (24.4–59.0) | 26.4 (25.7–39.6) |
| wake → first token p99 (ms) | 47.7 (26.3–47.9) | 37.1 (29.3–123) | 32.1 (30.7–51.1) |
| wake → stream end p50 (ms) | 17.2 (14.4–22.8) | 26.7 (23.6–31.8) | 24.4 (23.6–26.1) |
| wake → stream end p99 (ms) | 53.4 (52.9–53.9) | 45.6 (35.2–142) | 39.5 (38.2–62.4) |
| wake samples per run | 1000 | 1000 | 1000 |
| wake errors | 0.00 | 0.00 | 0.00 |
| first use (create) → first token p50 (ms) | 7.78 (7.70–7.82) | 31.3 (27.6–35.0) | 27.7 (27.4–29.0) |
| first use (create) → first token p95 (ms) | 12.4 (11.6–36.1) | 48.6 (34.0–72.7) | 37.7 (33.1–39.8) |
| first use (create) → first token p99 (ms) | 46.3 (31.3–49.2) | 66.3 (42.4–101) | 45.2 (42.5–51.4) |
| 1-min load average at start | 5.8–6.0 | 5.1–6.0 | 5.9–6.0 |

## warm

| metric | graphrt | rivet | rivet (ws) |
|---|---:|---:|---:|
| first token p50 (ms) | 1.00 (0.98–1.02) | 5.86 (5.03–6.66) | 4.40 (4.07–4.61) |
| first token p95 (ms) | 1.30 (1.26–1.33) | 10.2 (6.82–10.4) | 6.14 (5.78–6.68) |
| first token p99 (ms) | 24.6 (17.0–30.6) | 13.7 (8.84–16.0) | 7.80 (7.19–8.37) |
| overhead over the mock p50 (ms) | 0.95 (0.92–0.95) | 5.77 (4.93–6.62) | 4.33 (4.03–4.56) |
| overhead over the mock p99 (ms) | 24.2 (16.8–30.1) | 13.4 (8.60–15.7) | 7.50 (6.99–8.01) |
| response headers p50 (ms) | 0.40 (0.39–0.41) | 5.38 (4.63–6.28) | – |
| stream end p50 (ms) | 25.6 (24.0–30.6) | 10.6 (9.10–12.3) | 8.52 (7.94–9.10) |
| stream end p99 (ms) | 43.0 (43.0–43.1) | 22.8 (15.7–25.9) | 14.0 (12.8–15.2) |
| mock direct, first token p50 (ms) | 0.08 (0.03–0.08) | 0.09 (0.04–0.10) | 0.05 (0.04–0.07) |
| samples per run | 2000 | 2000 | 2000 |
| errors | 0.00 (0.00–1.00) | 0.00 | 0.00 |
| 1-min load average at start | 5.0–6.0 | 5.8–6.0 | 5.7–6.0 |

## memory

| metric | graphrt | rivet |
|---|---:|---:|
| agents | 10000 | 10000 |
| RSS before (MiB) | 14.9 (14.9–14.9) | 176 (175–180) |
| RSS with all agents just used (MiB) | 241 (239–243) | 7084 (6651–7145) |
| RSS after the idle period (MiB) | 139 (137–143) | 3965 (3915–4108) |
| asleep: total RSS / N (KiB) | 14.2 (14.0–14.6) | 406 (401–421) |
| asleep: (RSS − baseline) / N (KiB) | 12.7 (12.5–13.1) | 388 (383–403) |
| awake: (RSS − baseline) / N (KiB) | 23.1 (22.9–23.4) | 707 (663–714) |
| processes after the idle period | 1.00 | 2.00 |
| disk per agent (bytes) | 7891 (7880–7898) | 77598 (77078–77606) |
| time to give N agents one turn (s, concurrency 32) | 16.4 (16.1–16.5) | 54.0 (52.8–55.3) |
| turn errors | 0.00 | 0.00 |
| 1-min load average at start | 4.3–5.9 | 5.2–6.0 |

## concurrent

| metric | graphrt (x1) | graphrt (x4) | rivet (x1) | rivet (x4) |
|---|---:|---:|---:|---:|
| streams | 1000 | 1000 | 1000 | 1000 |
| RSS before the burst (MiB) | 98.4 (97.7–98.8) | 150 (149–151) | 1366 (1334–1371) | 1917 (1817–1960) |
| peak RSS (MiB) | 322 (311–322) | 377 (375–381) | 1979 (1956–1994) | 2693 (2668–2707) |
| CPU time (s) | 2.93 (2.80–2.94) | 3.03 (3.00–3.23) | 10.9 (10.1–11.4) | 16.5 (16.4–16.9) |
| average cores busy | 1.57 (1.48–1.84) | 1.96 (1.94–2.06) | 3.02 (2.96–3.17) | 6.11 (5.46–6.17) |
| failed streams | 0.00 | 0.00 | 0.00 (0.00–27.0) | 0.00 (0.00–3.00) |
| stalled streams (gap > 1 s) | 0.00 | 0.00 | 0.00 | 0.00 |
| first token p50 (ms) | 166 (147–171) | 153 (140–160) | 801 (723–817) | 407 (387–433) |
| first token p99 (ms) | 319 (286–345) | 261 (254–297) | 2448 (2428–2681) | 1680 (1594–1774) |
| token gap p50 (ms, mock: 50) | 51.0 (51.0–51.1) | 51.1 (51.1–51.1) | 37.7 (31.0–41.5) | 49.8 (49.7–50.7) |
| token gap p99 (ms) | 273 (123–283) | 132 (129–176) | 517 (503–616) | 190 (175–195) |
| worst gap (ms) | 320 (222–345) | 215 (203–243) | 519 (505–618) | 246 (221–302) |
| stream end p99 (ms) | 1843 (1566–1871) | 1534 (1508–1546) | 3402 (3317–3632) | 2658 (2568–2747) |
| mock direct, first token p99 (ms) | 76.3 (72.2–82.0) | 71.7 (70.0–111) | 73.1 (59.2–74.3) | 75.2 (67.1–84.8) |
| mock direct, token gap p99 (ms) | 54.5 (54.2–55.3) | 53.7 (53.5–54.8) | 55.1 (54.2–57.5) | 53.5 (53.1–55.6) |
| 1-min load average at start | 5.9–6.0 | 5.1–5.5 | 5.8–6.0 | 4.5–5.5 |

## recovery

| metric | graphrt | rivet |
|---|---:|---:|
| restart → first good turn p50 (ms) | 22.3 (19.8–25.6) | 46382 (46379–46384) |
| restart → first good turn max (ms) | 23.2 (21.9–27.5) | 46413 (46411–46485) |
| restart → processes ready p50 (ms) | 13.6 (12.6–13.7) | 1421 (1419–1423) |
| rounds per run | 5.00 | 5.00 |
| every turn after the restart succeeded | 3/3 runs | 3/3 runs |
| earlier turns kept | 3/3 runs | 3/3 runs |
| 1-min load average at start | 4.7–6.0 | 4.7–5.8 |
