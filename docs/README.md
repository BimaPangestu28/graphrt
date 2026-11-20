# graphrt documentation

Start with the [project README](../README.md) for install and a first workflow.

## Writing workflows

- [Workflow format](workflow-format.md): nodes, variables, templates, edges, conversation history
- [Model providers](providers.md): OpenAI-compatible, Anthropic, Gemini, Bedrock and more
- [TypeSafe System One (Jev)](typesafe.md): the fast classification model
- [Images in tool results](tool-images.md)

## Running and serving

- [The `graphrt` CLI](cli.md): every command and flag
- [Serving workflows](serving.md): HTTP API, SSE, run store, per-user tokens (`--verify-jwt`), webhooks, knowledge base
- [Deploying graphrt](deploy.md): container, Fly.io, Cloud Run, systemd
- [Bundles (`.grt`)](bundles.md)
- [Observability](observability.md): OpenTelemetry traces and metrics
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
