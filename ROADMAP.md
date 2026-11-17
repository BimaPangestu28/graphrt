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

## Next

- Verified mobile builds (Android emulator / device CI, iOS XCFramework on macOS CI).
- Live provider test matrix in CI (OpenAI, Anthropic, Gemini, Bedrock, TypeSafe) behind secrets.

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
