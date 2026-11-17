# Model credentials on user devices

graphrt runs agents inside apps, on the user's own phone or laptop. The model
itself is still an API (OpenAI-compatible, Anthropic, Gemini...), so every
model call needs a credential. This page explains how to provide one without
putting a provider key inside your app.

## Threat model

**Anything shipped in an app binary is public.** APKs and IPAs are routinely
unpacked; strings, config files and obfuscated constants are all recoverable.
A provider key extracted from an app lets anyone:

- spend your money at the provider's full rate limit,
- use models you never meant to expose,
- and it stays valid until you notice and rotate it, which breaks every
  installed copy of your app at once.

So the rule is: **the app never holds a long-lived key that you pay for.** It
holds either the user's *own* key, or a short-lived token that only your
backend can mint and that is useless outside the narrow purpose you allow.

graphrt supports three patterns. All of them plug into the same mechanism: a
provider asks a `CredentialSource` for a credential right before each request,
caches it until shortly before expiry, and on HTTP 401/403 drops it and retries
once with a fresh one.

Safety rules that apply to every pattern:

- **Endpoints that receive credentials must be `https://`** — the broker URL, any
  `base_url` in a credentials entry, and any `base_url` a broker or host returns.
  Plain `http://` is accepted only for `localhost`, `127.0.0.1`, `::1` and the
  Android emulator host `10.0.2.2`.
- **Redirects are never followed** by provider clients: a 3xx is an error, so a
  key header cannot follow a redirect to another host.
- **Operator headers stay home.** Static `headers` (and OpenAI `query`
  parameters) from the config are not sent when a credential moves the request
  to a different origin.
- **Signing out takes effect immediately.** Registering or clearing a host
  credential provider (`set_credential_provider` / `clear_credential_provider`)
  drops every cached credential; `clear_credential_cache` does the same without
  changing providers.
- **Hung sources fail fast.** A credential fetch that takes longer than 15 s
  (`fetch_timeout_ms`) fails; requests waiting behind a failed fetch get the
  same error instead of retrying it one after another.
- **The provider kind is explicit.** `kind` is inferred only for entries named
  `openai`, `anthropic`, `gemini`, `bedrock` or `typesafe`; any other name must
  set `kind`, so a credential is never sent to a vendor the entry was not
  written for. A `typesafe` entry configures the System One decision client,
  never a chat model.

| | 1. Bring your own key | 2. Thin proxy | 3. Token broker |
| --- | --- | --- | --- |
| Who pays | the user | you | you |
| Your server | none | a small proxy | a token endpoint (+ usually the proxy) |
| Key in the app | the user's key, in Keychain/Keystore | none (an app token) | none (a short-lived token) |
| Good for | power users, dev tools, self-hosters | consumer apps | consumer apps, multiple providers |

## 1. Bring your own key

```text
  ┌──────────── phone ────────────┐
  │ Keychain / Keystore            │
  │   └─ user's API key            │
  │        │ get_credential("openai")
  │ graphrt runtime ───────────────┼──────▶ provider API
  └────────────────────────────────┘
```

The user pastes their own key once; the app stores it in the platform's secure
storage and hands it to graphrt on demand. Nothing of yours can leak because
there is nothing of yours in the app.

```json
{ "credentials": { "openai": { "source": "external" } } }
```

The app registers a `CredentialProvider` (Kotlin / Swift examples in
[mobile.md](mobile.md#credentials)); `invalidate` is called when the provider
rejects the key, so the app can prompt the user to fix it.

## 2. Thin proxy

```text
  phone                        your proxy                      provider
  graphrt ── Bearer <app token> ──▶ verify token ── Bearer sk-… ──▶ API
          ◀──────── streamed tokens ◀──────────────────────────────
```

The app talks to *your* OpenAI-compatible endpoint. The proxy checks the app's
token, enforces a model and path allow-list and per-user quotas, adds the real
provider key and streams the response back. Point a provider's `base_url` at it:

```json
{
  "credentials": {
    "proxy": { "source": "external", "kind": "openai", "base_url": "https://llm-proxy.example.com/v1" }
  }
}
```

The "credential" the app supplies is now its own session token, which the proxy
verifies. Because graphrt speaks the plain OpenAI and Anthropic wire formats,
the proxy only forwards bytes; it needs no knowledge of graphrt.

Ready-made proxies: [`examples/proxy/`](../examples/proxy/) has a Cloudflare
Worker and a small Rust (axum) server, both with tests.

## 3. Short-lived tokens from a token broker

```text
  phone                                   your backend
  graphrt ── POST /token {provider} ───────▶ check user session
          ◀── {token, expires_in, base_url} ── mint 15-min token
  graphrt ── Bearer <token> ───────────────▶ proxy (or provider) ──▶ API
```

graphrt asks your broker for a token per provider, caches it until 60 s before
it expires, refreshes it in the background of the next request, and re-asks on
a 401. Concurrent requests share one refresh.

```json
{
  "credentials": {
    "proxy": {
      "source": "token_broker",
      "url": "https://llm-proxy.example.com/token",
      "app_auth": "external",
      "kind": "openai",
      "base_url": "https://llm-proxy.example.com/v1"
    }
  }
}
```

With `"app_auth": "external"` the broker request carries
`Authorization: Bearer <session>`, where the session token comes from the app's
`CredentialProvider` registered under exactly the name `token_broker`. The `*`
fallback is never used for the app session, so a user's own provider key
registered under `*` can never be sent to your broker.

### Broker contract

Request: `POST {url}`, JSON body `{"provider": "<name>", ...claims}`.

Response (2xx, JSON, ≤ 64 KiB):

```json
{
  "token": "eyJhbGciOi…",            // or "access_token"
  "expires_in": 900,                 // seconds (alias "expiresIn")
  "headers": { "x-tenant": "acme" }, // optional, added to model requests
  "base_url": "https://llm-proxy.example.com/v1" // optional endpoint override (https only)
}
```

Instead of `expires_in` the broker may send `expires_at` / `expiresAt` (Unix
time; values above 10^11 are read as milliseconds) or `expires_at_ms` /
`expiresAtMs`. A token without a usable expiry is cached for 5 minutes, never
indefinitely.

A 401/403 from the broker surfaces as an authentication error (the user must
sign in again).

For Anthropic the `base_url` must be the bare origin (the client appends
`/v1/messages`) and the token is sent as `x-api-key`. The bundled proxies
(`examples/proxy/`) accept the app token from `Authorization: Bearer` or
`x-api-key` and return the right `base_url` per `provider` in the `/token`
request.

Providers files (CLI, server) have no host app, so they accept only `env` and
`token_broker` sources with `app_auth: none`; `external` sources are rejected at
startup.

Some providers can mint scoped, short-lived credentials themselves (for
example cloud IAM session tokens); a broker can return those directly instead
of routing through a proxy.

## Rotation, revocation, limits

- **Rotation.** Short TTLs (5–15 minutes) make rotation automatic. Provider keys
  live only on the server and can be rotated there without touching apps.
- **Revocation.** Revoke a user by refusing to mint new tokens for them; their
  current token dies at expiry. Rotate the proxy's signing secret to revoke all
  tokens at once.
- **Rate limiting and quotas** belong in the proxy, keyed by the token's
  subject: requests per minute, tokens per day, allowed models.
- **Logging.** graphrt never logs credentials: `Debug` output redacts them and
  error messages strip URLs. Do the same in your proxy.

## Server-side and desktop use

On servers, keep using environment variables (`OPENAI_API_KEY`...) or a
providers file. `EnvCredentials` reads a variable per request, so rotating it
under a supervisor needs no restart, and `{ "source": "env", "var": "..." }`
does the same declaratively.

## Rust API

```rust
use std::sync::Arc;
use graphrt_llm::credentials::{FnCredentials, Credential, TokenBroker, TokenBrokerConfig, CachedCredentials};
use graphrt_llm::openai::{OpenAiConfig, OpenAiModel};

// Bring your own key from any async lookup.
let byok = CachedCredentials::new(FnCredentials::new(|provider: String| async move {
    Ok(Credential::new(read_from_secure_storage(&provider)?))
}));

// Or short-lived tokens from your broker.
let broker = TokenBroker::cached(TokenBrokerConfig::new("https://llm-proxy.example.com/token"))?;

let model = OpenAiModel::new(
    OpenAiConfig::new("https://llm-proxy.example.com/v1", None).with_credentials(Arc::new(broker)),
)?;
```

`graphrt_llm::credential_config` builds the same providers from the JSON
shown above (`apply(&mut models, &entries, &host_credentials)`).
