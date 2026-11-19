// node --test tools/telemetry-collector/test
import assert from "node:assert/strict";
import { test } from "node:test";

import worker, { validateReport } from "../src/index.js";

const TODAY = "2026-11-19";

const report = () => ({
  schema: 1,
  install_id: "0123456789abcdef0123456789abcdef",
  version: "0.3.0",
  os: "linux",
  arch: "aarch64",
  build: "cli",
  features: ["core", "llm", "serve", "telemetry"],
  days: [{ date: "2026-11-18", runs: 3, serve_requests: 10 }],
});

test("accepts the documented payload", () => {
  assert.equal(validateReport(report(), TODAY), null);
  assert.equal(validateReport({ ...report(), days: [] }, TODAY), null);
});

test("rejects extra, missing or malformed fields", () => {
  assert.notEqual(validateReport({ ...report(), hostname: "box" }, TODAY), null);
  const { os, ...missing } = report();
  assert.notEqual(validateReport(missing, TODAY), null);
  assert.notEqual(validateReport({ ...report(), install_id: "XYZ" }, TODAY), null);
  assert.notEqual(validateReport({ ...report(), build: "ffi" }, TODAY), null);
  assert.notEqual(validateReport({ ...report(), schema: 2 }, TODAY), null);
  assert.notEqual(validateReport({ ...report(), days: [{ date: "2026-11-20", runs: 1, serve_requests: 0 }] }, TODAY), null);
  assert.notEqual(validateReport({ ...report(), days: [{ date: "2026-11-18", runs: -1, serve_requests: 0 }] }, TODAY), null);
  assert.notEqual(validateReport({ ...report(), days: [{ date: "2026-11-18", runs: 1, serve_requests: 0, path: "/x" }] }, TODAY), null);
  assert.notEqual(validateReport(null, TODAY), null);
  assert.notEqual(validateReport([], TODAY), null);
});

/** A D1 stand-in recording the statements of a batch. */
function fakeDb() {
  const batches = [];
  return {
    batches,
    prepare: (sql) => ({ bind: (...values) => ({ sql, values }) }),
    batch: async (statements) => {
      batches.push(statements);
      return [];
    },
  };
}

test("stores a valid report without client metadata", async () => {
  const db = fakeDb();
  const request = new Request("https://collector.example/v1/report", {
    method: "POST",
    headers: { "content-type": "application/json", "cf-connecting-ip": "203.0.113.9" },
    body: JSON.stringify({ ...report(), days: [{ date: "2000-01-01", runs: 3, serve_requests: 10 }] }),
  });
  const response = await worker.fetch(request, { DB: db });
  assert.equal(response.status, 204);
  assert.equal(db.batches.length, 1);
  const [reportRow, countRow] = db.batches[0];
  assert.equal(reportRow.values[0], "0123456789abcdef0123456789abcdef");
  assert.deepEqual(countRow.values, ["0123456789abcdef0123456789abcdef", "2000-01-01", 3, 10]);
  assert.ok(!JSON.stringify(db.batches).includes("203.0.113.9"));
});

test("refuses other paths, methods, bad JSON and large bodies", async () => {
  const env = { DB: fakeDb() };
  const post = (body, path = "/v1/report") =>
    worker.fetch(new Request(`https://collector.example${path}`, { method: "POST", body }), env);
  assert.equal((await post("{}", "/other")).status, 404);
  assert.equal((await worker.fetch(new Request("https://collector.example/v1/report"), env)).status, 405);
  assert.equal((await post("not json")).status, 400);
  assert.equal((await post(JSON.stringify({ ...report(), extra: 1 }))).status, 400);
  assert.equal((await post("x".repeat(9000))).status, 413);
  assert.equal(env.DB.batches.length, 0);
});
