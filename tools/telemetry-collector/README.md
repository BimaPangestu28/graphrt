# graphrt telemetry collector

## Overview

A small Cloudflare Worker with a D1 database that receives the opt-in usage
reports of the `graphrt` CLI ([docs/telemetry.md](../../docs/telemetry.md))
and answers two questions: how many installations are active each week, and
how many runs and served requests they handle per day.

## Features

- `POST /v1/report` accepts exactly the documented payload; any other field,
  oversized body (8 KiB), future date or malformed value is refused with `400`.
- Stores one row per install per UTC day it reported, plus per-day counts.
- Never reads or stores IP addresses or request headers, keeps no timestamps
  finer than a day, and logs nothing (Workers Logs off in the template).
- No dependencies and no build step.

## Prerequisites

- A Cloudflare account and [wrangler](https://developers.cloudflare.com/workers/wrangler/)
  (`npm i -g wrangler`, then `wrangler login`).
- Node.js 20+ to run the tests.

## Installation

```sh
cd tools/telemetry-collector
wrangler d1 create graphrt-telemetry               # prints the database_id
cp wrangler.example.jsonc wrangler.jsonc           # paste the database_id
wrangler d1 execute graphrt-telemetry --remote --file=schema.sql
wrangler deploy                                    # prints the worker URL
```

`wrangler.jsonc` is your local copy; keep account-specific values out of the
repository.

Then build the CLI with the endpoint compiled in:

```sh
GRAPHRT_TELEMETRY_URL=https://graphrt-telemetry.<subdomain>.workers.dev/v1/report \
  cargo build -p graphrt-cli --profile min-size
```

For release binaries, set `GRAPHRT_TELEMETRY_URL` in the release workflow's
build environment; for the container image pass
`--build-arg GRAPHRT_TELEMETRY_URL=...`. Binaries built without it never send
anything.

## Quick start

Test locally against `wrangler dev` (plain `http://localhost` is accepted by
the CLI for this purpose only):

```sh
wrangler d1 execute graphrt-telemetry --local --file=schema.sql
wrangler dev                                       # http://localhost:8787
GRAPHRT_TELEMETRY=1 GRAPHRT_TELEMETRY_URL=http://localhost:8787/v1/report \
  GRAPHRT_CONFIG_DIR=/tmp/graphrt-telemetry-test \
  graphrt serve examples/hello.yaml               # sends a report at startup
wrangler d1 execute graphrt-telemetry --local --command "SELECT * FROM reports"
```

## Usage

`wrangler d1 execute --file` runs a file as an import and prints only a
summary, so pass a query with `--command` to see its rows:

```sh
# Weekly active installs
wrangler d1 execute graphrt-telemetry --remote --command \
  "SELECT COUNT(DISTINCT install_id) AS weekly_active FROM reports WHERE received_day >= date('now','-6 days')"
# Active installs per week, last 12 weeks
wrangler d1 execute graphrt-telemetry --remote --command \
  "SELECT strftime('%Y-W%W', received_day) AS week, COUNT(DISTINCT install_id) AS active FROM reports WHERE received_day >= date('now','-84 days') GROUP BY week ORDER BY week DESC"
# Runs and serve requests per day, last 30 days
wrangler d1 execute graphrt-telemetry --remote --command \
  "SELECT day, SUM(runs) AS runs, SUM(serve_requests) AS serve_requests FROM daily_counts WHERE day >= date('now','-29 days') GROUP BY day ORDER BY day DESC"
```

`queries/` holds these and a few more (version and build breakdowns); paste
one statement at a time into `--command`.

An install counts as active in a week when it sent a report that week. The
CLI reports at most once a day and only when `graphrt run` or `graphrt serve`
is used, so a report means the tool was used that day.

## Configuration

| Setting | Where |
| --- | --- |
| D1 binding `DB` | `wrangler.jsonc` (`d1_databases`) |
| Worker name, compatibility date | `wrangler.jsonc` |

Optionally add a Cloudflare rate-limiting rule on `/v1/report` (for example
10 requests per minute per client) to blunt junk submissions; the worker
itself keeps no per-client state.

Retention is up to you; for example, to keep 400 days:

```sh
wrangler d1 execute graphrt-telemetry --remote --command \
  "DELETE FROM reports WHERE received_day < date('now','-400 days'); DELETE FROM daily_counts WHERE day < date('now','-400 days');"
```

## API Reference

`POST /v1/report` with `Content-Type: application/json`:

```json
{
  "schema": 1,
  "install_id": "9f3c2a71d04b8e65a1c7f2d390e4b6a8",
  "version": "0.3.0", "os": "linux", "arch": "aarch64", "build": "cli",
  "features": ["core", "llm", "http", "serve", "telemetry"],
  "days": [{ "date": "2026-11-18", "runs": 3, "serve_requests": 120 }]
}
```

Responses: `204` stored; `400` invalid (the body says why); `404` other
paths; `405` other methods; `413` larger than 8 KiB. Counts for a day the
same install reports twice are added together.

## Contributing

Run the tests with `npm test` (Node's built-in test runner, no install
needed). Keep the validation in `src/index.js` in step with the payload in
`crates/graphrt-cli/src/telemetry/report.rs` and docs/telemetry.md.

## License

Apache-2.0, like graphrt.
