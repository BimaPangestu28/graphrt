# Linux musl baseline (2026-11-22)

First Linux measurement of the agent builds that `bench/budgets.toml` budgets,
taken for the shutdown drain (F0a). Static musl binaries, `--profile min-size`,
`--no-default-features --features <features>`, built with `cargo zigbuild` on
macOS. Raw numbers: `linux-musl-aarch64.json`, `linux-musl-x86_64.json`.
Reproduce with `scripts/budget-gate.py --target <triple> --build-cmd "cargo zigbuild"`.

| Build | Features | x86_64 binary | aarch64 binary | aarch64 idle RSS | aarch64 exec→/healthz p50 / p95 |
| --- | --- | ---: | ---: | ---: | ---: |
| agent-minimal | `serve` | 4.38 MiB (4,590,880 B) | 3.72 MiB (3,897,128 B) | 3.06 MiB | – |
| agent-slim | `serve,llm,plugins` | 7.40 MiB (7,761,440 B) | 6.16 MiB (6,461,976 B) | 4.90 MiB | 1.9 / 4.6 ms (n = 30) |

Same builds on `main` before the drain (aarch64): 3,852,656 B and 6,422,800 B,
so the drain adds 43 KiB and 38 KiB (33 KiB on macOS arm64: 3,352,128 →
3,385,456 B and 5,683,168 → 5,716,480 B). Idle RSS on `main`: 3.11 and 4.61 MiB.

- aarch64 run-time numbers: Lima VM (vz, 4 vCPU, 4 GiB) on an Apple M3 Pro,
  Ubuntu 26.04, binaries on the VM's local disk. Idle RSS is `VmRSS` one second
  after the first healthy `/healthz`.
- x86_64 run-time numbers are not recorded: under Rosetta they are not
  representative (RSS 8–10 MiB, start-up in the hundreds of ms from
  translation). The CI job `budgets` measures them on a native runner.
- The plan's binary budgets (3.4 / 5.6 MiB) came from macOS builds; musl
  binaries are 0.5–2 MiB larger. `bench/budgets.toml` sets the Linux budgets to
  this baseline plus about 3 % pending a decision on the final figures.

## Gateway (2026-11-23)

`--features gateway` (SQLite, mimalloc without transparent huge pages,
local provider, `trust-gateway` for its local agents) and `gateway-all` (every provider), min-size, `cargo zigbuild`:

| Build | x86_64 binary | aarch64 binary | aarch64 idle RSS |
| --- | ---: | ---: | ---: |
| gateway | 9.09 MiB (9,532,984 B) | 7.77 MiB (8,149,288 B) | 7.1 MiB |
| gateway-all | 10.08 MiB (10,572,088 B) | 8.56 MiB (8,979,624 B) | – |
| agent-slim-gateway | +88.1 KiB over agent-slim | +73.3 KiB over agent-slim | 4.86 MiB |

Run-time targets of the gateway: [docs/benchmarks.md](../../docs/benchmarks.md#gateway-targets-on-linux)
and `bench/results/targets-2026-11-23-lima-gbench-aarch64.md`.
