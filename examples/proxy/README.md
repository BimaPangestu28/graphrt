# Thin LLM proxy + token broker

Two equivalent, minimal implementations of the server side of the
[thin proxy](../../docs/credentials.md#2-thin-proxy) and
[token broker](../../docs/credentials.md#3-short-lived-tokens-from-a-token-broker)
patterns. Both expose:

| Endpoint | Auth | Does |
| --- | --- | --- |
| `POST /token` | the app's user session | issues an HS256 token valid for `TOKEN_TTL_SECS` (the token broker) |
| `POST /v1/<path>` | `Bearer <token>` | checks token, path and model allow-list, adds the provider key, streams the response (the proxy) |

The provider key exists only on the server. A leaked app token expires within
minutes and only works for allowed paths and models.

| Directory | Runs on | Test |
| --- | --- | --- |
| [`cloudflare-worker/`](cloudflare-worker/) | Cloudflare Workers (`wrangler deploy`) | `node examples/proxy/cloudflare-worker/test.mjs` |
| [`rust/`](rust/) | any server or container (axum) | `cargo test --manifest-path examples/proxy/rust/Cargo.toml` |

The Rust test drives the proxy with graphrt's own `TokenBroker` and
OpenAI-compatible client, i.e. exactly what an app does.

## Point graphrt at it

Runtime config of the embedded runtime (`graphrt-ffi`):

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

Workflows then use `model: { provider: proxy, name: gpt-4o-mini }`. The app
registers a `CredentialProvider` that answers `get_credential("token_broker")`
with the signed-in user's session token; graphrt fetches, caches and refreshes
the short-lived proxy token on its own.

## Production checklist

- Replace the demo `DEMO_USER_TOKEN` check (`authenticate_user` /
  `authenticateUser`) with your identity provider.
- Add per-user rate limits and spend quotas at the marked spot.
- Keep `ALLOWED_MODELS` and `ALLOWED_PATHS` tight.
- Rotate `PROXY_JWT_SECRET` to revoke every outstanding token at once.
