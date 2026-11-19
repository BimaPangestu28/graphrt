// graphrt telemetry collector: a Cloudflare Worker storing opt-in usage
// reports (docs/telemetry.md) in D1.
//
// POST /v1/report  body: one report  ->  204, or 400/404/405/413 with a reason.
//
// Privacy: the worker never reads the client address (`CF-Connecting-IP`) or
// any other header, stores only the validated report fields plus the UTC day
// it was received, and logs nothing.

const MAX_BODY_BYTES = 8 * 1024;
const MAX_DAYS = 31;
const MAX_FEATURES = 32;
const MAX_COUNT = 1_000_000_000;
const REPORT_KEYS = ["schema", "install_id", "version", "os", "arch", "build", "features", "days"];
const DAY_KEYS = ["date", "runs", "serve_requests"];
const BUILDS = ["cli", "docker"];

const isString = (value, pattern) => typeof value === "string" && pattern.test(value);
const isCount = (value) => Number.isInteger(value) && value >= 0 && value <= MAX_COUNT;
const hasExactly = (object, keys) =>
  object !== null &&
  typeof object === "object" &&
  !Array.isArray(object) &&
  Object.keys(object).length === keys.length &&
  keys.every((key) => Object.hasOwn(object, key));

/**
 * Validate a report strictly: exactly the documented fields, bounded sizes.
 *
 * @param {unknown} report - Parsed request body.
 * @param {string} today - Current UTC day (YYYY-MM-DD); later days are refused.
 * @returns {string | null} Why the report is invalid, or null when it is valid.
 */
export function validateReport(report, today) {
  if (!hasExactly(report, REPORT_KEYS)) return "unexpected or missing fields";
  if (report.schema !== 1) return "unsupported schema";
  if (!isString(report.install_id, /^[0-9a-f]{32}$/)) return "invalid install_id";
  if (!isString(report.version, /^[0-9A-Za-z.+-]{1,32}$/)) return "invalid version";
  if (!isString(report.os, /^[a-z0-9_]{1,16}$/)) return "invalid os";
  if (!isString(report.arch, /^[a-z0-9_]{1,16}$/)) return "invalid arch";
  if (!BUILDS.includes(report.build)) return "invalid build";
  const features = report.features;
  if (!Array.isArray(features) || features.length > MAX_FEATURES) return "invalid features";
  if (!features.every((feature) => isString(feature, /^[a-z0-9-]{1,24}$/))) return "invalid features";
  const days = report.days;
  if (!Array.isArray(days) || days.length > MAX_DAYS) return "invalid days";
  for (const day of days) {
    if (!hasExactly(day, DAY_KEYS)) return "invalid day entry";
    if (!isString(day.date, /^\d{4}-\d{2}-\d{2}$/) || day.date > today) return "invalid date";
    if (!isCount(day.runs) || !isCount(day.serve_requests)) return "invalid counts";
  }
  return null;
}

/**
 * D1 statements storing one valid report: one row per install and receiving
 * day, and additive per-day counts.
 */
export function reportStatements(db, report, today) {
  const statements = [
    db
      .prepare(
        `INSERT INTO reports (install_id, received_day, version, os, arch, build, features)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (install_id, received_day) DO NOTHING`,
      )
      .bind(
        report.install_id,
        today,
        report.version,
        report.os,
        report.arch,
        report.build,
        JSON.stringify(report.features),
      ),
  ];
  for (const day of report.days) {
    statements.push(
      db
        .prepare(
          `INSERT INTO daily_counts (install_id, day, runs, serve_requests)
           VALUES (?1, ?2, ?3, ?4)
           ON CONFLICT (install_id, day) DO UPDATE SET
             runs = runs + excluded.runs,
             serve_requests = serve_requests + excluded.serve_requests`,
        )
        .bind(report.install_id, day.date, day.runs, day.serve_requests),
    );
  }
  return statements;
}

const reply = (status, message) =>
  new Response(message ?? null, { status, headers: message ? { "content-type": "text/plain" } : {} });

export default {
  async fetch(request, env) {
    const { pathname } = new URL(request.url);
    if (pathname !== "/v1/report") return reply(404, "not found");
    if (request.method !== "POST") return reply(405, "use POST");
    if (Number(request.headers.get("content-length") ?? 0) > MAX_BODY_BYTES) return reply(413, "too large");
    const text = await request.text();
    if (text.length > MAX_BODY_BYTES) return reply(413, "too large");
    let report;
    try {
      report = JSON.parse(text);
    } catch {
      return reply(400, "invalid JSON");
    }
    const today = new Date().toISOString().slice(0, 10);
    const problem = validateReport(report, today);
    if (problem) return reply(400, problem);
    await env.DB.batch(reportStatements(env.DB, report, today));
    return reply(204);
  },
};
