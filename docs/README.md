# graphrt documentation

Start with the [project README](../README.md) for install and a first workflow.

## Writing workflows

- [Workflow format](workflow-format.md): nodes, variables, templates, edges, conversation history
- [Model providers](providers.md): OpenAI-compatible, Anthropic, Gemini, Bedrock and more; live provider tests
- [TypeSafe System One (Jev)](typesafe.md): the fast classification model
- [Images in tool results](tool-images.md)

## Evaluating

- [Evaluation](evals.md): `graphrt eval` test suites, judges as workflows, trials, baselines, calibration, the `replay` provider, CI gates, and online evaluation with `serve --evaluators`

## Running and serving

- [The `graphrt` CLI](cli.md): every command and flag
- [Serving workflows](serving.md): HTTP API, SSE, run store, per-user tokens (`--verify-jwt`), webhooks, knowledge base
- [Deploying graphrt](deploy.md): container, Fly.io, Cloud Run, systemd
- [Gateway on Cloud Run](gateway-cloud-run.md): GCP auth, the Cloud Run provider and deploy driver, IAM roles, billing caveats
- [Kubernetes provider (gateway)](kubernetes-provider.md): scale-to-zero of agent Deployments, RBAC, cluster auth (in-cluster, kubeconfig, exec plugins), optional KEDA
- [Bundles (`.grt`)](bundles.md)
- [Observability](observability.md): OpenTelemetry traces and metrics, GenAI semantic conventions, presets for Langfuse, LangSmith, Braintrust, Opik, Phoenix and MLflow
- [Usage telemetry](telemetry.md)

## Guides

- [Chatbot: Flutter / React Native + Supabase](chatbot-supabase.md): multi-user chatbot with no secret in the app, RLS, prompt-injection defences, end-to-end security tests
- [Running graphrt on phones and desktops](mobile.md)
- [Model credentials on user devices](credentials.md)
- [Embedding graphrt](embedding.md) in a Rust program

## Extending

- [Plugins](plugins.md) and the [plugin manifest specification](plugin-manifest.md) ([JSON schema](schemas/plugin-manifest.schema.json))
- [WebAssembly plugins](wasm-plugins.md)

## Internals and security

- [Architecture](architecture.md)
- [Security guide](security.md)
- [Benchmarks](benchmarks.md)
