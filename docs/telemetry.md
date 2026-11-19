# Usage telemetry

The `graphrt` binary can send one small anonymous usage report a day, so the
project can see how many installations are active each week and how much they
are used. It is **off by default** and stays off until you opt in.

## Turning it on and off

```sh
graphrt telemetry status    # on or off, why, and the exact next report
graphrt telemetry enable    # opt in
graphrt telemetry disable   # opt out; deletes the install id and unsent counts
```

`GRAPHRT_TELEMETRY=1` opts in without the command (for example in a
container); `GRAPHRT_TELEMETRY=0` turns it off even after `enable`.

Telemetry is **always off**, whatever you enabled, when:

| Condition | Detected by |
| --- | --- |
| You asked not to be tracked | `DO_NOT_TRACK` set (to anything but `0`/`false`) |
| It runs in CI | `CI`, `CONTINUOUS_INTEGRATION`, `BUILD_NUMBER`, `GITHUB_ACTIONS`, `GITLAB_CI`, `CIRCLECI`, `TRAVIS`, `BUILDKITE`, `BUILDKITE_BUILD_ID`, `JENKINS_URL`, `TEAMCITY_VERSION`, `TF_BUILD`, `BITBUCKET_BUILD_NUMBER`, `CODEBUILD_BUILD_ID`, `DRONE`, `APPVEYOR`, `SEMAPHORE` |
| No endpoint is configured | neither the build-time nor the run-time `GRAPHRT_TELEMETRY_URL` is set (an empty run-time value also counts as none) |
| The endpoint is not `https://` | plain `http://` is accepted only for `localhost` (testing a collector) |

Without an endpoint telemetry is fully inert: nothing is counted to disk,
written or sent. Release builds get the endpoint at build time
(`GRAPHRT_TELEMETRY_URL=https://... cargo build`, read with `option_env!`);
`GRAPHRT_TELEMETRY_URL` at run time overrides it, e.g. to point at your own
collector.

## Exactly what is sent

`graphrt telemetry status` prints the next report verbatim (`--json` for a
machine-readable status). A report looks like this:

```json
{
  "schema": 1,
  "install_id": "9f3c2a71d04b8e65a1c7f2d390e4b6a8",
  "version": "0.3.0",
  "os": "linux",
  "arch": "aarch64",
  "build": "docker",
  "features": ["core", "llm", "http", "wasm", "serve", "telemetry"],
  "days": [
    { "date": "2026-11-17", "runs": 42, "serve_requests": 0 },
    { "date": "2026-11-18", "runs": 3, "serve_requests": 1250 }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `schema` | Payload version. |
| `install_id` | 32 random hex digits generated on this machine when telemetry is first on. Not derived from the hardware, the user or the network. `disable` deletes it; a later `enable` creates a new one. |
| `version` | graphrt version. |
| `os`, `arch` | Rust's `std::env::consts::OS` / `ARCH` (`linux`, `macos`, `windows`; `x86_64`, `aarch64`, ...). |
| `build` | `cli` (release binaries, `cargo install`) or `docker` (the container image). |
| `features` | Cargo features compiled into the binary (as `graphrt version` prints them). |
| `days` | For each complete UTC day not reported yet: how many runs `graphrt run` and `graphrt serve` started, and how many API requests `serve` handled (`/healthz` is not counted). |

That is all. Reports **never** contain workflow documents, node names,
inputs, outputs, prompts, model names, errors, file paths, host names, user
names, IP addresses or environment variables. The HTTP request carries only
`Content-Type: application/json` and `User-Agent: graphrt/<version>`. The
collector necessarily sees the connecting IP address, like any HTTP server;
the reference collector (`tools/telemetry-collector`) does not read or store
it.

## How it works

- `graphrt run` and `graphrt serve` count runs and requests in memory. When the
  process ends (and hourly for `serve`) the counts are added to today's entry in
  a small state file: `telemetry.json` in the user config directory
  (`~/Library/Application Support/graphrt` on macOS, `%APPDATA%\graphrt` on
  Windows, `$XDG_CONFIG_HOME/graphrt` or `~/.config/graphrt` elsewhere;
  `GRAPHRT_CONFIG_DIR` overrides it). No other command counts or sends
  anything.
- At most once per UTC day a background thread sends one report with the
  complete days, then removes them from the file. The day is claimed under a
  file lock before sending, so concurrent `graphrt` processes send once between
  them. If the send fails, its counts are kept for the next day's report; at
  most 31 days are kept.
- The send never blocks or slows a run: it happens on its own thread, with a
  5-second timeout, and the process does not wait for it on exit (a report cut
  off by a short `graphrt run` is simply retried next time).
- The request goes through the same egress policy as the `http-request` node
  (private addresses refused, no redirects, no environment proxies).
- Only the CLI binary reports. The embedded library (`graphrt-core` and
  friends) and the FFI library (`graphrt-ffi`) contain no telemetry; the
  `graphrt-cli` library exposes only in-memory counters, persisted and sent
  only by the binary's `main`.

## Building without it

Telemetry is the `telemetry` cargo feature of `graphrt-cli`, part of the
default set. Builds that list features explicitly leave it out, e.g. the
small-device server build:

```sh
cargo build -p graphrt-cli --profile min-size --no-default-features --features llm,http,wasm,serve
```

In that build the `graphrt telemetry` command does not exist and no
telemetry code is compiled in. Adding `telemetry` to that build costs about
49 KiB (5,883,760 -> 5,933,712 bytes with `--profile min-size` on macOS
arm64); it needs the `http` feature, whose HTTP client it reuses.

## The collector

`tools/telemetry-collector/` is a Cloudflare Worker with a D1 database that
accepts reports, validates them strictly and stores them without IP
addresses. Its README covers deployment and the weekly-active-installs and
daily-runs queries.
