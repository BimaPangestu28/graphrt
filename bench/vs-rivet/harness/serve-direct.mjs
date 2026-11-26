// Side measurement for REPORT.md: the warm turn against `graphrt serve`
// directly (no gateway), with the same client and mock, to show how much of
// graphrt's stream-end time the gateway adds.
//
//   GRAPHRT_BIN=... node serve-direct.mjs
import { spawn } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { Mock, freePort } from "./systems.mjs";
import { streamTurn, makeDispatcher, summarize, waitUntil } from "./client.mjs";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const work = process.env.BENCH_WORK ?? join(process.env.HOME, ".cache", "graphrt-vs-rivet", "work");
mkdirSync(work, { recursive: true });
if (!process.env.GRAPHRT_BIN) throw new Error("GRAPHRT_BIN is required");

const mock = await new Mock({ work, mockBin: join(ROOT, "mock-openai", "target", "release", "mock-openai") }).start({});
const port = await freePort();
const serve = spawn(process.env.GRAPHRT_BIN, ["serve", join(ROOT, "graphrt", "agent.yaml"), "--listen", `127.0.0.1:${port}`], {
	env: { ...process.env, OPENAI_API_KEY: "bench", OPENAI_BASE_URL: mock.baseUrl },
	stdio: "ignore",
});
try {
	const dispatcher = makeDispatcher();
	const spec = () => ({
		origin: `http://127.0.0.1:${port}`,
		path: "/v1/workflows/agent/runs",
		headers: {},
		body: { stream: true, sys: { query: "hi" } },
		doneMarker: "run_succeeded",
	});
	await waitUntil(async () => (await streamTurn(dispatcher, spec())).ok, 10_000, "graphrt serve");
	const samples = [];
	for (let i = 0; i < 2200; i++) {
		const sample = await streamTurn(dispatcher, spec());
		if (i >= 200) samples.push(sample);
	}
	console.log(JSON.stringify({
		what: "graphrt serve directly, warm, sequential",
		samples: samples.length,
		errors: samples.filter((s) => !s.ok).length,
		ttft_ms: summarize(samples.map((s) => s.ttftMs)),
		done_ms: summarize(samples.map((s) => s.doneMs)),
	}, null, 1));
} finally {
	serve.kill();
	await mock.stop();
}
