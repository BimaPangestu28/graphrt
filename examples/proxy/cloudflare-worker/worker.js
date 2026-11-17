// Thin LLM proxy + token broker for apps that run graphrt agents on user devices.
//
//   POST /token            app's user session  ->  { token, expires_in, base_url }
//   POST /v1/<path>        Authorization: Bearer <token> (OpenAI-compatible clients)
//                          or x-api-key: <token> (Anthropic clients)
//                          ->  provider (with the server-side key)
//
// The provider key lives only in this Worker's secrets. Apps hold a short-lived,
// HS256-signed token that the Worker verifies on every request.
//
// `/token` returns a base_url matching the client dialect: `<origin>/v1` for
// OpenAI-compatible clients and `<origin>` for Anthropic clients (they append
// `/v1/messages` themselves). The dialect comes from the request body's
// `provider` ("anthropic") or, by default, from UPSTREAM_AUTH.
//
// Secrets / vars (see wrangler.toml). Every value is validated per request; a
// misconfiguration answers 500 with the problem instead of silently defaulting.
//   PROXY_JWT_SECRET   32+ random bytes, signs and verifies app tokens   (secret)
//   UPSTREAM_API_KEY   provider key                                       (secret)
//   UPSTREAM_BASE_URL  e.g. https://api.openai.com/v1 | https://api.anthropic.com/v1
//                      | https://generativelanguage.googleapis.com/v1beta/openai
//   UPSTREAM_AUTH      "bearer" (OpenAI-compatible, Gemini) or "x-api-key" (Anthropic)
//   ALLOWED_MODELS     comma-separated allow-list ("" = any)
//   ALLOWED_PATHS      default "/chat/completions,/messages,/embeddings"
//   TOKEN_TTL_SECS     default 900 (1..86400)
//   DEMO_USER_TOKEN    demo only: the "session" /token accepts. Replace authenticateUser().

const enc = new TextEncoder();
const MAX_TTL_SECS = 86400;

function b64url(bytes) {
  let s = "";
  for (const b of new Uint8Array(bytes)) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function b64urlDecode(text) {
  const pad = text.length % 4 === 0 ? "" : "=".repeat(4 - (text.length % 4));
  const bin = atob(text.replace(/-/g, "+").replace(/_/g, "/") + pad);
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

async function hmacKey(secret) {
  return crypto.subtle.importKey("raw", enc.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [
    "sign",
    "verify",
  ]);
}

export async function signToken(secret, sub, exp) {
  const header = b64url(enc.encode(JSON.stringify({ alg: "HS256", typ: "JWT" })));
  const payload = b64url(enc.encode(JSON.stringify({ sub, exp })));
  const sig = await crypto.subtle.sign("HMAC", await hmacKey(secret), enc.encode(`${header}.${payload}`));
  return `${header}.${payload}.${b64url(sig)}`;
}

/** Returns the subject, or throws with a reason. `crypto.subtle.verify` is constant-time. */
export async function verifyToken(secret, token, nowSecs) {
  const parts = token.split(".");
  if (parts.length !== 3) throw new Error("malformed token");
  const [h, p, s] = parts;
  let header, payload, signature;
  try {
    header = JSON.parse(new TextDecoder().decode(b64urlDecode(h)));
    payload = JSON.parse(new TextDecoder().decode(b64urlDecode(p)));
    signature = b64urlDecode(s);
  } catch {
    throw new Error("malformed token");
  }
  if (header.alg !== "HS256") throw new Error("unsupported token algorithm");
  const ok = await crypto.subtle.verify("HMAC", await hmacKey(secret), signature, enc.encode(`${h}.${p}`));
  if (!ok) throw new Error("invalid token signature");
  if (typeof payload.exp !== "number") throw new Error("token has no expiry");
  if (payload.exp <= nowSecs) throw new Error("token expired");
  if (typeof payload.sub !== "string" || payload.sub === "") throw new Error("token has no subject");
  return payload.sub;
}

function json(status, body) {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

const error = (status, message) => json(status, { error: { message } });

function list(value, fallback) {
  return (value ?? fallback)
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

function bearer(request) {
  const value = request.headers.get("authorization") ?? "";
  return value.startsWith("Bearer ") ? value.slice(7) : null;
}

/** App token: `Authorization: Bearer` (OpenAI-compatible) or `x-api-key` (Anthropic). */
function appToken(request) {
  return bearer(request) || request.headers.get("x-api-key") || null;
}

function isLoopbackHttp(url) {
  return /^http:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?(\/|$)/.test(url);
}

/** Validate the Worker's settings; returns the parsed config or throws with the problem. */
export function readConfig(env) {
  if (!env.PROXY_JWT_SECRET || env.PROXY_JWT_SECRET.length < 32) {
    throw new Error("PROXY_JWT_SECRET is not configured (32+ characters)");
  }
  if (!env.UPSTREAM_API_KEY) throw new Error("UPSTREAM_API_KEY is not configured");
  const auth = env.UPSTREAM_AUTH ?? "bearer";
  if (auth !== "bearer" && auth !== "x-api-key") {
    throw new Error(`UPSTREAM_AUTH must be "bearer" or "x-api-key", got "${auth}"`);
  }
  const ttl = env.TOKEN_TTL_SECS === undefined ? 900 : Number(env.TOKEN_TTL_SECS);
  if (!Number.isInteger(ttl) || ttl < 1 || ttl > MAX_TTL_SECS) {
    throw new Error(`TOKEN_TTL_SECS must be a whole number between 1 and ${MAX_TTL_SECS}`);
  }
  const upstreamBase = (env.UPSTREAM_BASE_URL ?? "https://api.openai.com/v1").replace(/\/+$/, "");
  if (!upstreamBase.startsWith("https://") && !isLoopbackHttp(upstreamBase)) {
    throw new Error("UPSTREAM_BASE_URL must be https:// (the provider key is sent there)");
  }
  return {
    secret: env.PROXY_JWT_SECRET,
    upstreamKey: env.UPSTREAM_API_KEY,
    auth,
    ttl,
    upstreamBase,
    allowedModels: list(env.ALLOWED_MODELS, ""),
    allowedPaths: list(env.ALLOWED_PATHS, "/chat/completions,/messages,/embeddings"),
  };
}

/** Replace with your real session check (e.g. verify your identity provider's ID token). */
async function authenticateUser(request, env, config) {
  const presented = bearer(request);
  if (!presented || !env.DEMO_USER_TOKEN) return null;
  // Compare HMACs to avoid a timing side channel on the raw strings.
  const key = await hmacKey(config.secret);
  const [a, b] = await Promise.all([
    crypto.subtle.sign("HMAC", key, enc.encode(presented)),
    crypto.subtle.sign("HMAC", key, enc.encode(env.DEMO_USER_TOKEN)),
  ]);
  return b64url(a) === b64url(b) ? "demo-user" : null;
}

async function issueToken(request, env, config) {
  const user = await authenticateUser(request, env, config);
  if (!user) return error(401, "sign in required");
  let provider = null;
  const text = await request.text();
  if (text) {
    try {
      provider = JSON.parse(text).provider ?? null;
    } catch {
      return error(400, "body must be JSON");
    }
  }
  const token = await signToken(config.secret, user, Math.floor(Date.now() / 1000) + config.ttl);
  const origin = new URL(request.url).origin;
  const anthropic = provider ? provider === "anthropic" : config.auth === "x-api-key";
  return json(200, { token, expires_in: config.ttl, base_url: anthropic ? origin : `${origin}/v1` });
}

async function forward(request, config, path) {
  const token = appToken(request);
  if (!token) return error(401, "missing token");
  let user;
  try {
    user = await verifyToken(config.secret, token, Math.floor(Date.now() / 1000));
  } catch (e) {
    return error(401, e.message);
  }
  if (!config.allowedPaths.includes(path)) return error(404, "path not allowed");
  let body;
  try {
    body = await request.json();
  } catch {
    return error(400, "body must be JSON");
  }
  if (config.allowedModels.length && !config.allowedModels.includes(body.model)) {
    return error(400, "model not allowed");
  }
  // Per-user rate limiting belongs here, keyed by `user` (e.g. a Durable Object
  // counter or Cloudflare's Rate Limiting binding).

  const headers = { "content-type": "application/json" };
  if (config.auth === "x-api-key") headers["x-api-key"] = config.upstreamKey;
  else headers.authorization = `Bearer ${config.upstreamKey}`;
  // Forward only provider headers the app may set; never its token.
  for (const name of ["anthropic-version", "anthropic-beta", "accept"]) {
    const v = request.headers.get(name);
    if (v) headers[name] = v;
  }
  let upstream;
  try {
    // The parsed body is re-serialized, so duplicate keys cannot smuggle a model
    // past the allow-list.
    upstream = await fetch(`${config.upstreamBase}${path}`, {
      method: "POST",
      headers,
      body: JSON.stringify(body),
      redirect: "manual",
    });
  } catch (e) {
    console.error(`upstream request failed for ${user} on ${path}: ${e.message}`);
    return error(502, "upstream unreachable");
  }
  let status = upstream.status;
  if (status === 401 || status === 403) {
    // A rejected provider key is a server problem, not the app's token: report 502.
    console.error(`upstream rejected the provider key (HTTP ${status}) on ${path}: check UPSTREAM_API_KEY`);
    status = 502;
  } else if (status >= 500 || status === 429) {
    console.error(`upstream answered HTTP ${status} on ${path} for ${user}`);
  }
  // Stream the body straight through (SSE tokens arrive as they are produced).
  return new Response(upstream.body, {
    status,
    headers: { "content-type": upstream.headers.get("content-type") ?? "application/json" },
  });
}

export default {
  async fetch(request, env) {
    let config;
    try {
      config = readConfig(env);
    } catch (e) {
      console.error(`proxy misconfigured: ${e.message}`);
      return error(500, `proxy misconfigured: ${e.message}`);
    }
    const url = new URL(request.url);
    if (request.method !== "POST") return error(405, "method not allowed");
    if (url.pathname === "/token") return issueToken(request, env, config);
    if (url.pathname.startsWith("/v1/")) return forward(request, config, url.pathname.slice(3));
    return error(404, "not found");
  },
};
