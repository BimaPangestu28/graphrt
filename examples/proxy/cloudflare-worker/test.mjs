// Offline test of worker.js under Node (Web Crypto, fetch, Request/Response):
//   node examples/proxy/cloudflare-worker/test.mjs
import assert from "node:assert/strict";
import http from "node:http";
import worker, { signToken, verifyToken } from "./worker.js";

const SECRET = "0123456789abcdef0123456789abcdef";

// Token unit checks.
const t = await signToken(SECRET, "u1", 2000);
assert.equal(await verifyToken(SECRET, t, 1000), "u1");
await assert.rejects(verifyToken(SECRET, t, 2000), /expired/);
await assert.rejects(verifyToken("another-secret-another-secret-xx", t, 1000), /signature/);

// Fake provider recording the authorization header.
const seen = [];
const upstream = http.createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    seen.push({ auth: req.headers.authorization, model: JSON.parse(body).model, path: req.url });
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.end('data: {"choices":[{"index":0,"delta":{"content":"proxied"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n');
  });
});
await new Promise((r) => upstream.listen(0, "127.0.0.1", r));
const env = {
  PROXY_JWT_SECRET: SECRET,
  UPSTREAM_API_KEY: "sk-server-side",
  UPSTREAM_BASE_URL: `http://127.0.0.1:${upstream.address().port}/v1`,
  ALLOWED_MODELS: "allowed-model",
  DEMO_USER_TOKEN: "user-session",
};
const call = (path, auth, body) =>
  worker.fetch(
    new Request(`https://proxy.example.com${path}`, {
      method: "POST",
      headers: { "content-type": "application/json", ...(auth ? { authorization: `Bearer ${auth}` } : {}) },
      body: JSON.stringify(body ?? {}),
    }),
    env,
  );

assert.equal((await call("/token", "wrong")).status, 401);
const issued = await (await call("/token", "user-session")).json();
assert.equal(issued.base_url, "https://proxy.example.com/v1");

const ok = await call("/v1/chat/completions", issued.token, { model: "allowed-model", messages: [] });
assert.equal(ok.status, 200);
assert.match(await ok.text(), /proxied/);
assert.deepEqual(seen, [{ auth: "Bearer sk-server-side", model: "allowed-model", path: "/v1/chat/completions" }]);

assert.equal((await call("/v1/chat/completions", issued.token, { model: "other" })).status, 400);
assert.equal((await call("/v1/chat/completions", "forged.token.x", { model: "allowed-model" })).status, 401);
assert.equal((await call("/v1/files", issued.token, { model: "allowed-model" })).status, 404);
assert.equal(seen.length, 1, "rejected requests never reach the provider");

upstream.close();
console.log("worker proxy + token broker OK");
