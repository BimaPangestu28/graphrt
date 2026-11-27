# graphrt documentation

Start with the [project README](../README.md) for install and a first workflow.

## Writing workflows

- [Workflow format](workflow-format.md): nodes, variables, templates, edges, conversation history
- [Model providers](providers.md): OpenAI-compatible, Anthropic, Gemini, Bedrock and more; live provider tests
- [TypeSafe System One (Jev)](typesafe.md): the fast classification model
- [Files and images](files-and-images.md): agent workspaces, the `files` tools, limits, one workspace per user, vision input, the `image` tools, image generation
- [Images in tool results](tool-images.md)

## Evaluating

- [Evaluation](evals.md): `graphrt eval` test suites, judges as workflows, trials, baselines, calibration, the `replay` provider, CI gates, and online evaluation with `serve --evaluators`

## Running and serving

- [The `graphrt` CLI](cli.md): every command and flag
- [Serving workflows](serving.md): HTTP API, SSE, run store, per-user tokens (`--verify-jwt`), webhooks, knowledge base
- [Monitoring and approving from a phone](monitoring.md): monitoring page (installable), device tokens, approvals inbox, requests that expire, push notifications
- [Using a served agent from a browser](web.md): chat page, chat widget, guests, CORS, conversations on the server, browser client, UI blocks renderer
- [Deploying graphrt](deploy.md): container, Fly.io, Cloud Run, systemd
- [Direct deploys to Fly.io](deploy-direct.md) (preview): `graphrt deploy --target fly --direct`, no gateway; JSON Lines contract for apps (status, versions, rollback, pause/resume, remove), API key, least-privilege tokens
- [Gateway](gateway.md) (preview): always-on front of scale-to-zero agents, SQLite store, auth, limits, webhooks, cron, `graphrt deploy --target local|k8s|cloud-run|aca|ecs|lambda|fly`, measured cold starts
- [Gateway active/passive](gateway-ha.md) (preview): on-prem HA on a shared volume, fenced lease, systemd units, volumes not to use
- [Gateway protocol](gateway-protocol.md): `serve --trust-gateway`, signed `/internal/v1` API, run store contract (epochs, idempotent appends, coordination store), SSE ids and `Last-Event-ID`
- [Kubernetes provider (gateway)](kubernetes-provider.md): scale-to-zero of agent Deployments, RBAC, cluster auth (in-cluster, kubeconfig, exec plugins), optional KEDA
- [Gateway on Cloud Run](gateway-cloud-run.md): GCP auth, the Cloud Run provider and deploy driver, IAM roles, billing caveats
- [Gateway on AWS](gateway-aws.md): ECS Fargate and Lambda targets of the gateway, credentials, IAM policies, limits
- [Fly Machines provider of the gateway](gateway-fly.md): Machine pools, suspend/stop, 6PN, Fly secrets, `kill_timeout` (preview)
- [Azure Container Apps and Entra ID](deploy-azure.md): the gateway's Azure provider (`graphrt-provider-azure`), auth, RBAC, ingress timeouts, conformance
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
- [Skills](skills.md): `SKILL.md` packages loaded on demand, tool grants, pinned content, `graphrt skill`
- [Self learning](learning.md): lessons with provenance, approval, isolation per agent and user, `graphrt learn`

## Internals and security

- [Architecture](architecture.md)
- [Skills and self learning](skills-and-learning.md) (design): research, threats, decisions and the contract for user interfaces
- [Security guide](security.md)
- [Files and images: research, design and threat model](files-and-images-design.md)
- [Benchmarks](benchmarks.md)
