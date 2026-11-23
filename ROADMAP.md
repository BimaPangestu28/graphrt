# Roadmap and governance

## Direction

**Run your agent everywhere.** graphrt stays a small, embeddable runtime: the
same workflow document runs from a laptop to a container, the edge and a phone.
Models come from API providers; the runtime itself needs no server.

## Done in 0.1

- OpenTelemetry traces and metrics for runs and nodes (`--otel`).
- Portable `.grt` bundles: one signed file holding a workflow, its plugins and configs.
- Run persistence for `graphrt serve` (resume by run id, async runs, restart-safe).
- Reproducible benchmarks (`bench/`, see docs/benchmarks.md).

## Done in 0.2

- Knowledge bases managed at runtime (`/v1/knowledge`): datasets, documents,
  chunking (including parent-child), enable/disable, local SQLite store and
  in-process embeddings.
- Small-device serving: bounded in-memory run store (`--max-runs`) and a trimmed
  server build that runs in 16 MiB of RAM.
- Engine and server roughly 2x / 5-9x faster than 0.1.

## Done in 0.3

- Workflow format v1 frozen, with a published JSON Schema
  ([schema/workflow-v1.schema.json](schema/workflow-v1.schema.json)), `graphrt schema`
  and editor autocomplete.
- Validation reports every error at once with file:line:column, the field path and
  "did you mean" suggestions (`graphrt validate --format json` for tools).
- Webhook triggers for `graphrt serve` (signatures, idempotency, signed completion
  callbacks).
- Opt-in anonymous usage telemetry (off by default) and a self-hostable collector.
- Security review and hardening (JS sandbox limits, secret scrubbing in errors,
  egress policy, serve admission and DNS-rebinding protection); see
  [docs/security.md](docs/security.md).
- Android and iOS sample apps on the Kotlin and Swift bindings
  ([examples/mobile](examples/mobile)).
- Reproducible comparison against LangGraph and LangChain LCEL
  ([bench/compare](bench/compare)).
- Engine, LLM path and server another 20-45% faster, with fewer allocations per node.

## Done in 0.4

- Evaluation: `graphrt eval` with datasets, deterministic, trajectory, retrieval
  and judge assertions (judges are graphrt workflows), trials with pass@k /
  pass^k, baselines, judge calibration and CI gates; a `replay` provider for
  deterministic runs ([docs/evals.md](docs/evals.md)).
- Online evaluation in `graphrt serve` (sampled judges, human feedback, scores
  API) and OpenTelemetry GenAI semantic conventions, with presets for Langfuse,
  LangSmith, Braintrust, Opik, Phoenix and MLflow.
- End-user JWT verification and per-user rate limits for `graphrt serve`.

## Done in 0.5

- `graphrt gateway` (preview): always-on front of scale-to-zero agents, sole
  writer of the run store, re-dispatch after drains, lost replicas and gateway
  restarts, on-prem active/passive HA, warm-up and cost guards
  ([docs/gateway.md](docs/gateway.md)).
- Providers and `graphrt deploy` for local processes, Kubernetes, Cloud Run,
  ECS Fargate, Lambda, Fly and Azure Container Apps (cloud targets tested
  against API fakes; real-cloud verification pending).
- Gateway release build (`graphrt-gateway-<target>`, `:gateway` image) next to
  the slim agent build.

## Next

- Real-cloud verification of the Cloud Run, ECS, Lambda, Fly and Azure
  Container Apps providers.

- Windows binaries tested on Windows, and release automation per tag (CI).
  The workflows are in place (tests on Linux, macOS and Windows gate every tag;
  binaries are smoke-tested on their OS); done once a run on GitHub Actions
  has passed.
- Mobile builds verified in CI (Android emulator, iOS simulator) and on devices.
  The workflow ([mobile.yml](.github/workflows/mobile.yml)), smoke tests and a
  device checklist ([docs/mobile.md](docs/mobile.md#testing-on-devices)) are in
  place; open until the jobs pass on GitHub Actions and the checklist has run
  on an Android phone.
- Live provider test matrix in CI (OpenAI, Anthropic, Gemini, Bedrock, TypeSafe) behind secrets:
  the tests and the nightly `Live providers` workflow are in place; done once the
  repository secrets are set and the first nightly run is green.

## Later

- Workflow versioning and migrations for builder platforms.
- More native providers where OpenAI-compatible endpoints are not enough.
- Signed plugin registry.

## Governance

graphrt is maintained by its original author. Decisions are made in public issues
and pull requests; larger changes start as an issue describing the problem and the
proposed design. Contributors with sustained, high-quality contributions may be
invited as maintainers. See [CONTRIBUTING.md](CONTRIBUTING.md) and
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

Versioning follows SemVer; until 1.0 breaking changes may land in minor releases
and are listed in [CHANGELOG.md](CHANGELOG.md).
