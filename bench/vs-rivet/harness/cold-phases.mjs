// Side measurement for REPORT.md: where graphrt's cold start time goes.
//
//   GRAPHRT_BIN=... node cold-phases.mjs [samples]
//
// 1. Process phases, without the gateway (each `samples` times, sequential):
//    - `graphrt version`: exec to exit (process start, binary load, runtime init);
//    - `graphrt validate agent.yaml`: exec to exit (+ reading and building the flow);
//    - `graphrt serve agent.yaml`: exec to the first accepted TCP connection
//      (+ node registry, HTTP server, bind).
// 2. A cold wake through the gateway (a fresh deployment per sample, so its
//    agent process starts for the request), timed at the client on markers of
//    the SSE stream: response headers, `run_started`, the `node_started` of the
//    LLM node, the first model token.
import { spawn } from "node:child_process";
import { connect } from "node:net";
import { mkdirSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { request } from "undici";
import { Mock, Graphrt, freePort } from "./systems.mjs";
import { makeDispatcher, summarize, sleep } from "./client.mjs";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const samples = Number(process.argv[2] ?? 300);
const bin = process.env.GRAPHRT_BIN;
if (!bin) throw new Error("GRAPHRT_BIN is required");
const workflow = join(ROOT, "graphrt", "agent.yaml");
const work = process.env.BENCH_WORK ?? join(process.env.HOME, ".cache", "graphrt-vs-rivet", "work");
mkdirSync(work, { recursive: true });

function timeExit(args) {
	return new Promise((resolveExit, reject) => {
		const t0 = performance.now();
		const child = spawn(bin, args, { stdio: "ignore" });
		child.on("error", reject);
		child.on("exit", (code) => (code === 0 ? resolveExit(performance.now() - t0) : reject(new Error(`${args[0]} exited ${code}`))));
	});
}

function tryConnect(port) {
	return new Promise((resolveConnect) => {
		const socket = connect(port, "127.0.0.1");
		socket.once("connect", () => {
			socket.destroy();
			resolveConnect(true);
		});
		socket.once("error", () => resolveConnect(false));
	});
}

async function timeServeReady(mockUrl) {
	const port = await freePort();
	const t0 = performance.now();
	const child = spawn(bin, ["serve", workflow, "--listen", `127.0.0.1:${port}`], {
		stdio: "ignore",
		env: { ...process.env, OPENAI_API_KEY: "bench", OPENAI_BASE_URL: mockUrl },
	});
	let ready = null;
	while (ready === null) {
		if (await tryConnect(port)) ready = performance.now() - t0;
		else if (performance.now() - t0 > 10_000) throw new Error("serve did not listen");
	}
	child.kill("SIGKILL");
	await new Promise((r) => child.once("exit", r));
	return ready;
}

const MARKERS = { run_started: "event: run_started", llm_started: '"kind":"llm"', first_token: "tok0 " };

async function markedTurn(dispatcher, spec) {
	const t0 = performance.now();
	const at = {};
	const response = await request(`${spec.origin}${spec.path}`, {
		method: "POST",
		dispatcher,
		headers: { "content-type": "application/json", ...spec.headers },
		body: JSON.stringify(spec.body),
	});
	at.headers = performance.now() - t0;
	let text = "";
	for await (const chunk of response.body) {
		const now = performance.now() - t0;
		text += chunk.toString("utf8");
		for (const [name, marker] of Object.entries(MARKERS)) if (at[name] === undefined && text.includes(marker)) at[name] = now;
	}
	at.done = performance.now() - t0;
	at.ok = text.includes("run_succeeded");
	return at;
}

const mock = await new Mock({ work, mockBin: join(ROOT, "mock-openai", "target", "release", "mock-openai") }).start({});
const result = { samples, process: {}, gateway_wake: {} };
try {
	for (const [name, args] of [["version_exit_ms", ["version"]], ["validate_exit_ms", ["validate", workflow]]]) {
		for (let i = 0; i < 10; i++) await timeExit(args);
		const xs = [];
		for (let i = 0; i < samples; i++) xs.push(await timeExit(args));
		result.process[name] = summarize(xs);
	}
	for (let i = 0; i < 10; i++) await timeServeReady(mock.baseUrl);
	const ready = [];
	for (let i = 0; i < samples; i++) ready.push(await timeServeReady(mock.baseUrl));
	result.process.serve_listening_ms = summarize(ready);

	const graphrt = new Graphrt({ work, graphrtBin: bin, graphrtWorkflow: workflow }, mock);
	const names = Array.from({ length: samples + 10 }, (_, i) => `p${i}`);
	await graphrt.start({ deployments: names.map((name) => ({ name, pool: { max_active_runs: 8, min_replicas: 0, idle_timeout_secs: 0 } })) });
	const dispatcher = makeDispatcher();
	const rows = [];
	try {
		for (const [i, name] of names.entries()) {
			const row = await markedTurn(dispatcher, graphrt.turnSpec(name, name, "Hello, who are you?"));
			if (i >= 10) rows.push(row);
			// Keep the number of live agents small: wait for the idle release now and then.
			if (i % 50 === 49) {
				while (graphrt.agentPids().length > 0) await sleep(200);
			}
		}
	} finally {
		await graphrt.stop();
	}
	const ok = rows.filter((r) => r.ok && r.first_token !== undefined);
	const field = (f) => summarize(ok.map((r) => r[f]));
	const delta = (a, b) => summarize(ok.map((r) => r[b] - r[a]));
	result.gateway_wake = {
		ok: ok.length,
		failed: rows.length - ok.length,
		headers_ms: field("headers"),
		run_started_ms: field("run_started"),
		llm_started_ms: field("llm_started"),
		first_token_ms: field("first_token"),
		headers_to_run_started_ms: delta("headers", "run_started"),
		run_started_to_llm_started_ms: delta("run_started", "llm_started"),
		llm_started_to_first_token_ms: delta("llm_started", "first_token"),
		slow_first_token_over_20ms: ok.filter((r) => r.first_token > 20).length,
		slow_rows_example: ok.filter((r) => r.first_token > 20).slice(0, 5),
	};
} finally {
	await mock.stop();
}
console.log(JSON.stringify(result, null, 1));
