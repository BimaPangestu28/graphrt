// Thin LLM proxy + token broker for apps that run graphrt agents on user devices.
//
//   POST /token            app's user session  ->  { token, expires_in, base_url }
//   POST /v1/<path>        Authorization: Bearer <token>  ->  provider (with the server-side key)
//
// The provider key lives only in this Worker's secrets. Apps hold a short-lived,
// HS256-signed token that the Worker verifies on every request.
//
// Secrets / vars (see wrangler.toml):
//   PROXY_JWT_SECRET   32+ random bytes, signs and verifies app tokens   (secret)
//   UPSTREAM_API_KEY   provider key                                       (secret)
//   UPSTREAM_BASE_URL  e.g. https://api.openai.com/v1 | https://api.anthropic.com/v1
//                      | https://generativelanguage.googleapis.com/v1beta/openai
//   UPSTREAM_AUTH      "bearer" (OpenAI-compatible, Gemini) or "x-api-key" (Anthropic)
//   ALLOWED_MODELS     comma-separated allow-list ("" = any)
//   ALLOWED_PATHS      default "/chat/completions,/messages,/embeddings"
//   TOKEN_TTL_SECS     default 900
//   DEMO_USER_TOKEN    demo only: the "session" /token accepts. Replace authenticateUser().

const enc = new TextEncoder();

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
  let header, payload;
  try {
    header = JSON.parse(new TextDecoder().decode(b64urlDecode(h)));
    payload = JSON.parse(new TextDecoder().decode(b64urlDecode(p)));
  } catch {
    throw new Error("malformed token");
  }
  if (header.alg !== "HS256") throw new Error("unsupported token algorithm");
  const ok = await crypto.subtle.verify("HMAC", await hmacKey(secret), b64urlDecode(s), enc.encode(`${h}.${p}`));
  if (!ok) throw new Error("invalid token signature");
  if (typeof payload.exp !== "number" || payload.exp <= nowSecs) throw new Error("token expired");
  return String(payload.sub ?? "");
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

/** Replace with your real session check (e.g. verify your identity provider's ID token). */
async function authenticateUser(request, env) {
  const presented = bearer(request);
  if (!presented || !env.DEMO_USER_TOKEN) return null;
  // Compare HMACs to avoid a timing side channel on the raw strings.
  const key = await hmacKey(env.PROXY_JWT_SECRET);
  const [a, b] = await Promise.all([
    crypto.subtle.sign("HMAC", key, enc.encode(presented)),
    crypto.subtle.sign("HMAC", key, enc.encode(env.DEMO_USER_TOKEN)),
  ]);
  return b64url(a) === b64url(b) ? "demo-user" : null;
}

async function issueToken(request, env) {
  const user = await authenticateUser(request, env);
  if (!user) return error(401, "sign in required");
  const ttl = Number(env.TOKEN_TTL_SECS ?? 900);
  const token = await signToken(env.PROXY_JWT_SECRET, user, Math.floor(Date.now() / 1000) + ttl);
  const base = new URL(request.url);
  return json(200, { token, expires_in: ttl, base_url: `${base.origin}/v1` });
}

async function forward(request, env, path) {
  const token = bearer(request);
  if (!token) return error(401, "missing token");
  let user;
  try {
    user = await verifyToken(env.PROXY_JWT_SECRET, token, Math.floor(Date.now() / 1000));
  } catch (e) {
    return error(401, e.message);
  }
  if (!list(env.ALLOWED_PATHS, "/chat/completions,/messages,/embeddings").includes(path)) {
    return error(404, "path not allowed");
  }
  let body;
  try {
    body = await request.json();
  } catch {
    return error(400, "body must be JSON");
  }
  const allowed = list(env.ALLOWED_MODELS, "");
  if (allowed.length && !allowed.includes(body.model)) return error(400, "model not allowed");
  // Per-user rate limiting belongs here, keyed by `user` (e.g. a Durable Object
  // counter or Cloudflare's Rate Limiting binding).
  void user;

  const headers = { "content-type": "application/json" };
  if ((env.UPSTREAM_AUTH ?? "bearer") === "x-api-key") headers["x-api-key"] = env.UPSTREAM_API_KEY;
  else headers.authorization = `Bearer ${env.UPSTREAM_API_KEY}`;
  for (const name of ["anthropic-version", "anthropic-beta", "accept"]) {
    const v = request.headers.get(name);
    if (v) headers[name] = v;
  }
  const upstreamBase = (env.UPSTREAM_BASE_URL ?? "https://api.openai.com/v1").replace(/\/+$/, "");
  const upstream = await fetch(`${upstreamBase}${path}`, {
    method: "POST",
    headers,
    body: JSON.stringify(body),
    redirect: "manual",
  });
  // A rejected provider key is a server problem, not the app's token: report 502.
  const status = upstream.status === 401 || upstream.status === 403 ? 502 : upstream.status;
  // Stream the body straight through (SSE tokens arrive as they are produced).
  return new Response(upstream.body, {
    status,
    headers: { "content-type": upstream.headers.get("content-type") ?? "application/json" },
  });
}

export default {
  async fetch(request, env) {
    if (!env.PROXY_JWT_SECRET || env.PROXY_JWT_SECRET.length < 32) {
      return error(500, "PROXY_JWT_SECRET is not configured");
    }
    const url = new URL(request.url);
    if (request.method !== "POST") return error(405, "method not allowed");
    if (url.pathname === "/token") return issueToken(request, env);
    if (url.pathname.startsWith("/v1/")) return forward(request, env, url.pathname.slice(3));
    return error(404, "not found");
  },
};
