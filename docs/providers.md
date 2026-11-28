# Model providers

Model-backed nodes (`llm`, `question-classifier`, `parameter-extractor`,
`agent`) name a provider and a model:

```yaml
model: { provider: openai, name: gpt-4o-mini }
```

The provider name is resolved in the `ModelRegistry` host service. Providers
come from environment variables, from a providers file, or from code.

| Kind | Protocol | Cargo feature (`graphrt-llm`) |
| --- | --- | --- |
| `openai` | OpenAI-compatible chat completions | `openai` (default) |
| `anthropic` | Anthropic Messages API | `anthropic` (default) |
| `gemini` | Google Gemini `streamGenerateContent` (Gemini API or Vertex AI) | `gemini` |
| `bedrock` | AWS Bedrock Converse API (`converse-stream`) | `bedrock` |

All providers stream, support tool calling (parallel calls are assembled by
index), report token usage, map provider errors to typed errors (rate limits
carry the provider's retry delay, so the engine waits the right amount before
retrying), and never print keys in `Debug` output or error messages.

## OpenAI-compatible

One implementation covers every server that speaks the chat-completions
protocol: OpenAI, Azure OpenAI, Groq, DeepSeek, OpenRouter, Together, Mistral,
Fireworks, xAI, Gemini's OpenAI-compatible endpoint, vLLM, llama.cpp server,
LM Studio and Ollama.

| Variables | Provider |
| --- | --- |
| `OPENAI_API_KEY`, optional `OPENAI_BASE_URL` | `openai` |
| `OLLAMA_HOST` (default `http://localhost:11434`; always registered) | `ollama` |
| `GRAPHRT_OPENAI_COMPAT_<NAME>_BASE_URL`, optional `GRAPHRT_OPENAI_COMPAT_<NAME>_API_KEY` | `<name>` (lower-case) |

## Anthropic

| Variables | Provider |
| --- | --- |
| `ANTHROPIC_API_KEY`, optional `ANTHROPIC_BASE_URL` | `anthropic` |

Extended thinking is enabled through extra request parameters; signed thinking
blocks are kept and replayed automatically during tool use.

## Gemini

| Variables | Provider |
| --- | --- |
| `GEMINI_API_KEY` or `GOOGLE_API_KEY`, optional `GEMINI_BASE_URL` | `gemini` |

- The key is sent in the `x-goog-api-key` header, never in the URL.
- Function calling: Gemini function calls have no ids, so graphrt assigns
  stable ids and maps tool results back to function names.
- Thinking: thought parts stream as reasoning. Parts with a
  `thoughtSignature` are kept and replayed on the next turn, as Gemini
  requires for function calling with thinking models.
- Structured output: JSON Schemas are converted to Gemini's schema dialect
  (upper-case types, `nullable`, local `$ref`s inlined). Constraints Gemini
  cannot express (`additionalProperties`, non-string enums, unknown string
  formats...) are dropped and reported in the node's `warnings` metadata.
  Unresolvable `$ref`s and recursive schemas are configuration errors rather
  than an accept-anything schema.
- Finish reasons: a safety stop fails the node with `content_filter`
  (not retried); malformed function calls fail with a retryable `model_decode`.
- Images: `data:` URLs are sent inline; `https` URLs as `fileData`.
- Safety: a blocked prompt is an error naming the block reason; a response
  stopped by safety filters finishes with `content_filter`.
- Extra parameters: `safetySettings`, `cachedContent` and `labels` go to the
  request top level; anything else (`thinkingConfig`, `seed`, ...) goes into
  `generationConfig`.

**Vertex AI**: set `base_url` to
`https://<region>-aiplatform.googleapis.com/v1/projects/<project>/locations/<region>/publishers/google`
and `auth: bearer`. The bearer token is an OAuth access token; use a
credential source (see below) to refresh it, since access tokens expire.

## AWS Bedrock

| Variables | Provider |
| --- | --- |
| `AWS_REGION` (or `AWS_DEFAULT_REGION`) and `AWS_BEARER_TOKEN_BEDROCK` | `bedrock` (Bedrock API key) |
| `AWS_REGION` (or `AWS_DEFAULT_REGION`), `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional `AWS_SESSION_TOKEN` | `bedrock` (SigV4) |
| optional `BEDROCK_ENDPOINT_URL` | endpoint override (VPC endpoint, proxy) |

Without a region nothing is registered, so AWS keys set for other tools never
break startup.

- Authentication: Bedrock API keys are sent as `Authorization: Bearer`;
  access keys sign each request with AWS Signature Version 4. Only explicit
  credentials are read: there is no instance-metadata, SSO or profile-file
  credential chain. For temporary credentials, pass the session token.
- Model ids are inference profile or model ids such as
  `us.anthropic.claude-sonnet-4-5-20250929-v1:0` or `amazon.nova-pro-v1:0`.
- Images must be inline `data:` URLs (PNG, JPEG, GIF, WebP).
- JSON output is requested with a system instruction.
- `tool_choice: none` keeps the tool definitions but cannot forbid tool use
  (Converse has no such option).
- Extra parameters: `guardrailConfig`, `performanceConfig`,
  `promptVariables`, `requestMetadata` and `additionalModelRequestFields` go to
  the request top level; anything else (for example `thinking` for Claude
  models) is merged into `additionalModelRequestFields`. Signed reasoning
  blocks are replayed automatically during tool use.

## Providers file

`--providers <file>` (or `$GRAPHRT_PROVIDERS`) declares providers without code.
Secrets are always read from environment variables named in the file.

```yaml
providers:
  - name: openrouter
    kind: openai
    base_url: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_API_KEY
    aliases: [or]
    models: { fast: meta-llama/llama-3.1-8b-instruct }
    defaults: { temperature: 0.2 }

  - name: azure
    kind: openai
    base_url: https://my.openai.azure.com/openai/deployments/gpt4o
    api_key_env: AZURE_OPENAI_KEY
    auth_header: api-key
    query: { api-version: "2024-06-01" }

  - name: claude
    kind: anthropic
    api_key_env: ANTHROPIC_API_KEY

  - name: gemini
    kind: gemini
    api_key_env: GEMINI_API_KEY

  - name: vertex
    kind: gemini
    base_url: https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google
    api_key_env: VERTEX_ACCESS_TOKEN
    auth: bearer

  - name: bedrock
    kind: bedrock
    region: us-east-1
    access_key_id_env: AWS_ACCESS_KEY_ID
    secret_access_key_env: AWS_SECRET_ACCESS_KEY
    session_token_env: AWS_SESSION_TOKEN   # optional

  - name: bedrock-key
    kind: bedrock
    region: eu-central-1
    api_key_env: AWS_BEARER_TOKEN_BEDROCK
```

Fields:

| Field | Kinds | Meaning |
| --- | --- | --- |
| `name`, `kind` | all | Provider name used by nodes; protocol. |
| `base_url` | all | Endpoint override (`${VAR}` allowed). For Bedrock it replaces `https://bedrock-runtime.<region>.amazonaws.com`. |
| `api_key_env`, `api_key_optional` | all | Variable holding the key (Gemini key or token, Bedrock API key). |
| `headers` | all | Extra headers (`${VAR}` allowed). |
| `aliases`, `models`, `defaults` | all | Extra provider names, model-name aliases, default parameters. |
| `prices` | all | Price per model name, `{gpt-5: {input_per_million: 1.25, output_per_million: 10}}`, in any unit (a currency). The run's cost budget (`--max-cost-per-run`) is charged with it after every model response. Without a price a model costs nothing. |
| `auth_header`, `query` | `openai` | Custom key header, query parameters. |
| `auth` | `gemini` | `api-key` (default) or `bearer`. |
| `region`, `access_key_id_env`, `secret_access_key_env`, `session_token_env` | `bedrock` | Region and SigV4 credentials. |
| `cassettes`, `mode`, `upstream` | `replay` | Recorded responses: cassette directory (relative to the providers file), `replay` / `record` / `auto`, and the provider to record from. See [evals.md](evals.md#replay-deterministic-model-responses). |

Unknown fields and fields that do not apply to the provider kind are errors.

## Dynamic credentials

Every built-in provider accepts an optional `CredentialSource` (field
`credentials` on its config). It is asked for a credential before each
request, so apps can read the user's own key from the platform keychain or
fetch short-lived tokens from a backend instead of shipping a key. When the
provider answers 401/403, the source is told to drop its cached credential and
the request is retried once.

For Gemini the credential is the API key (or the bearer token with
`auth: bearer`); for Bedrock it is sent as a bearer token (Bedrock API key).

Every provider (OpenAI-compatible, Anthropic, Gemini, Bedrock with a credential
source, TypeSafe) honours a `base_url` returned with the credential, so the
thin-proxy pattern works for all of them. That URL must be `https://` (plain
http only for localhost); static config headers are not sent when it points to
a different origin. Provider clients never follow redirects.

A providers file may also hold a `credentials` section (see
[credentials.md](credentials.md)). There, `kind` must be set unless the entry is
named `openai`, `anthropic`, `gemini`, `bedrock` or `typesafe`; only `env` and
`token_broker` sources are allowed (no host app); and an entry may not share its
name with a `providers` entry. A `typesafe` entry configures the System One
client used by `question-classifier` (typesafe engine) and `system-one` nodes.

## Live provider tests

Every provider is covered offline by in-test HTTP servers. On top of that,
`crates/graphrt-llm/tests/live_providers.rs` sends tiny real requests (at most
48 output tokens each) to OpenAI, Anthropic, Gemini, Bedrock and TypeSafe: a
streamed chat answer whose deltas must add up to the final text with usage
reported, and a forced tool call whose result is sent back and answered
(TypeSafe: one yes/no and one choice question). The tests only run with
`GRAPHRT_LIVE_PROVIDERS=1`, and each provider only when its credentials are set;
otherwise they print a skip line and pass. Credential values are scrubbed from
every failure message.

```sh
GRAPHRT_LIVE_PROVIDERS=1 OPENAI_API_KEY=... \
  cargo test -p graphrt-llm --features gemini,bedrock --test live_providers -- --nocapture openai::
```

| Provider | Credentials | Default model (override) |
| --- | --- | --- |
| `openai` | `OPENAI_API_KEY` | `gpt-4o-mini` (`GRAPHRT_LIVE_OPENAI_MODEL`) |
| `anthropic` | `ANTHROPIC_API_KEY` | `claude-haiku-4-5` (`GRAPHRT_LIVE_ANTHROPIC_MODEL`) |
| `gemini` | `GEMINI_API_KEY` | `gemini-2.5-flash-lite` (`GRAPHRT_LIVE_GEMINI_MODEL`) |
| `bedrock` | `AWS_BEARER_TOKEN_BEDROCK`, or `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`; `AWS_REGION` (default `us-east-1`) | `us.amazon.nova-micro-v1:0` (`GRAPHRT_LIVE_BEDROCK_MODEL`) |
| `typesafe` | `TYPESAFE_API_KEY` (optional `TYPESAFE_BASE_URL`) | `jev-latest` (`GRAPHRT_LIVE_TYPESAFE_MODEL`) |

The [`Live providers` workflow](../.github/workflows/live-providers.yml) runs
this matrix nightly and on manual dispatch (never on pull requests), one job per
provider, each job receiving only its own provider's secrets. A provider whose
repository secret is missing is skipped before anything is built; dispatching
with `require_secrets: true` turns a missing secret into a failure. The secrets
above are repository secrets; `AWS_REGION`, `TYPESAFE_BASE_URL` and the
`GRAPHRT_LIVE_<PROVIDER>_MODEL` overrides are repository variables.
