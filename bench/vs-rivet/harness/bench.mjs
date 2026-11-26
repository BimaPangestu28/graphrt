// graphrt vs Rivet: one scenario, one system, one run, on a fresh instance of
// the system. Writes <out>/<scenario>-<system>[-<variant>]-run<N>.json with the
// summary and the raw samples.
//
//   node bench.mjs <cold|warm|memory|concurrent|recovery> <graphrt|rivet> --run 1 --out DIR
//     [--quick] [--replicas N] [--transport http|ws]
//
// See ../README.md for what each scenario measures.
import { writeFileSync, mkdirSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { cpus, totalmem, loadavg } from "node:os";
import { streamTurn, makeDispatcher, forEach, sleep, waitUntil, summarize, preconnect } from "./client.mjs";
import { measure, startSamplerThread, round } from "./procs.mjs";
import { Mock, Graphrt, Rivet } from "./systems.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");

function parseArgs(argv) {
	const args = { _: [] };
	for (let i = 0; i < argv.length; i++) {
		const a = argv[i];
		if (a.startsWith("--")) {
			const key = a.slice(2);
			const next = argv[i + 1];
			args[key] = next && !next.startsWith("--") ? argv[++i] : "true";
		} else args._.push(a);
	}
	return args;
}

const args = parseArgs(process.argv.slice(2));
const [scenario, systemName] = args._;
const quick = args.quick === "true";
const runIndex = Number(args.run ?? 1);
const transport = args.transport ?? "http";
const outDir = resolve(args.out ?? join(ROOT, "results", "adhoc"));
mkdirSync(outDir, { recursive: true });

const ctx = {
	root: ROOT,
	work: process.env.BENCH_WORK ?? join(process.env.HOME, ".cache", "graphrt-vs-rivet", "work"),
	graphrtBin: process.env.GRAPHRT_BIN,
	graphrtWorkflow: join(ROOT, "graphrt", "agent.yaml"),
	mockBin: process.env.MOCK_BIN ?? join(ROOT, "mock-openai", "target", "release", "mock-openai"),
	nodeBin: process.env.RIVET_NODE_BIN ?? process.execPath,
	rivetApp: join(ROOT, "rivet", "app"),
	rivetConfig: join(ROOT, "rivet", "rivet.json"),
	rivetImage: process.env.RIVET_IMAGE ?? "rivetdev/engine:3.1.1",
};
mkdirSync(ctx.work, { recursive: true });
if (systemName === "graphrt" && !ctx.graphrtBin) throw new Error("GRAPHRT_BIN is required");

const log = (...parts) => console.error(`[${scenario}/${systemName}${transport === "ws" ? "-ws" : ""} run ${runIndex}]`, ...parts);

function okSamples(samples) {
	return samples.filter((s) => s.ok);
}

function latencySummary(samples) {
	const ok = okSamples(samples);
	return {
		samples: samples.length,
		errors: samples.length - ok.length,
		error_examples: samples.filter((s) => !s.ok).slice(0, 3).map((s) => s.error),
		ttft_ms: summarize(ok.map((s) => s.ttftMs)),
		ttfb_ms: summarize(ok.map((s) => s.ttfbMs)),
		done_ms: summarize(ok.map((s) => s.doneMs)),
	};
}

// --------------------------------------------------------------------------- Rivet over WebSocket (README client)

async function rivetClient(rivet) {
	const { createClient } = await import("rivetkit/client");
	return createClient(`http://default:${rivet.token}@127.0.0.1:6420`);
}

/** One `sendMessage` turn on a connection; `connect` opens the connection inside the timed interval. */
async function wsTurn(client, key, text, existing = null) {
	const t0 = performance.now();
	let ttftMs = null;
	let tokens = 0;
	let last = null;
	let maxGapMs = 0;
	let error = null;
	const conn = existing ?? client.agent.getOrCreate([key]).connect();
	const off = conn.on("token", () => {
		const at = performance.now();
		if (tokens === 0) ttftMs = at - t0;
		else maxGapMs = Math.max(maxGapMs, at - last);
		last = at;
		tokens += 1;
	});
	try {
		await conn.sendMessage(text);
		if (tokens < 20) error = `${tokens}/20 tokens`;
	} catch (e) {
		error = String(e?.message ?? e).slice(0, 300);
	}
	const doneMs = performance.now() - t0;
	if (typeof off === "function") off();
	if (!existing) await conn.dispose();
	return { ok: error === null, ttftMs, ttfbMs: null, doneMs, tokens, maxGapMs, error, cold: null };
}

// --------------------------------------------------------------------------- scenarios

/**
 * Cold start to first token. Each sample is one agent that has been used once
 * and then put to sleep, so its state exists but nothing of it is running:
 *   graphrt: one deployment per agent; its `graphrt serve` process was stopped
 *            by the gateway's idle release (idle_timeout 0, released by the 5 s
 *            maintenance tick); the request makes the gateway start it again.
 *   Rivet:   one actor per agent, put to sleep with c.sleep() (as Rivet's own
 *            wake benchmark does); the request wakes it on the running node.
 * Both wait at least 10 s between sleeping and waking (Rivet's benchmark rule).
 * The first turn of every agent is reported too, as "create".
 */
async function cold(system, mock) {
	const samples = quick ? 100 : 1000;
	const batchSize = quick ? 50 : 100;
	const warmup = 20;
	const idleFloorMs = 10_000;
	const names = Array.from({ length: warmup + samples }, (_, i) => `a${i}`);
	if (system.name === "graphrt") {
		await system.start({ deployments: names.map((name) => ({ name, pool: { max_active_runs: 8, min_replicas: 0, idle_timeout_secs: 0 } })) });
	} else {
		await system.start({ nodes: 1 });
	}
	const client = system.name === "rivet" && transport === "ws" ? await rivetClient(system) : null;
	const dispatcher = makeDispatcher();
	const turn = async (name, text) => {
		if (client) return wsTurn(client, `cold-${name}`, text);
		if (system.name === "graphrt") {
			const sample = await streamTurn(dispatcher, system.turnSpec(name, name, text));
			if (sample.ok) system.remember(name, text);
			return sample;
		}
		return streamTurn(dispatcher, system.turnSpec(`cold-${name}`, text));
	};
	const batches = [names.slice(0, warmup)];
	for (let i = warmup; i < names.length; i += batchSize) batches.push(names.slice(i, i + batchSize));
	const create = [];
	const wake = [];
	for (const [index, batch] of batches.entries()) {
		const record = index > 0;
		for (const name of batch) {
			const sample = await turn(name, "Hello, who are you?");
			if (record) create.push({ agent: name, ...sample });
		}
		const sleptAt = Date.now();
		if (system.name === "graphrt") {
			await waitUntil(() => system.agentPids().length === 0, 60_000, "graphrt agents to stop", 50);
		} else {
			await forEach(batch.length, 8, async (i) => {
				const result = await system.sleepActor(`cold-${batch[i]}`);
				if (!result.ok) throw new Error(`sleep cold-${batch[i]}: ${result.status} ${result.body}`);
			});
		}
		const idleMs = idleFloorMs - (Date.now() - sleptAt);
		if (idleMs > 0) await sleep(idleMs);
		for (const name of batch) {
			const sample = await turn(name, "And what can you do?");
			if (record) wake.push({ agent: name, ...sample });
		}
		log(`batch ${index}/${batches.length - 1} done`);
	}
	const coldMarked = wake.filter((s) => s.cold === "1").length;
	return {
		summary: {
			wake: { ...latencySummary(wake), confirmed_cold: system.name === "rivet" && !client ? coldMarked : null },
			create: latencySummary(create),
			idle_floor_ms: idleFloorMs,
			mock_requests: await mock.requests(),
		},
		raw: { wake, create },
	};
}

/**
 * Warm requests: the agent is awake (graphrt: one deployment with a warm
 * replica; Rivet: the actors are awake on the node). 50 sessions round robin,
 * one request at a time. The same client also measures the mock directly, the
 * floor both runtimes add their overhead to.
 */
async function warm(system, mock) {
	const sessions = 50;
	const samples = quick ? 300 : 2000;
	const warmupTurns = 200;
	const dispatcher = makeDispatcher();
	const direct = [];
	for (let i = 0; i < warmupTurns + samples; i++) {
		const sample = await streamTurn(dispatcher, mock.turnSpec());
		if (i >= warmupTurns) direct.push(sample);
	}
	if (system.name === "graphrt") {
		await system.start({ deployments: [{ name: "chat", pool: { max_active_runs: 64, min_replicas: 1, idle_timeout_secs: 3600 } }] });
		await waitUntil(() => system.agentPids().length >= 1, 30_000, "graphrt warm replica");
	} else {
		await system.start({ nodes: 1 });
	}
	const client = system.name === "rivet" && transport === "ws" ? await rivetClient(system) : null;
	const conns = new Map();
	const turn = async (session, text) => {
		if (client) {
			if (!conns.has(session)) {
				const conn = client.agent.getOrCreate([session]).connect();
				conns.set(session, conn);
			}
			return wsTurn(client, session, text, conns.get(session));
		}
		if (system.name === "graphrt") {
			const sample = await streamTurn(dispatcher, system.turnSpec("chat", session, text));
			if (sample.ok) system.remember(session, text);
			return sample;
		}
		return streamTurn(dispatcher, system.turnSpec(session, text));
	};
	const measured = [];
	for (let i = 0; i < warmupTurns + samples; i++) {
		const session = `warm-${i % sessions}`;
		const sample = await turn(session, `Message ${i}`);
		if (i >= warmupTurns) measured.push({ session, ...sample });
	}
	for (const conn of conns.values()) await conn.dispose();
	const directSummary = latencySummary(direct);
	const summary = latencySummary(measured);
	summary.overhead_ttft_ms = {
		p50: round(summary.ttft_ms.p50 - directSummary.ttft_ms.p50),
		p95: round(summary.ttft_ms.p95 - directSummary.ttft_ms.p95),
		p99: round(summary.ttft_ms.p99 - directSummary.ttft_ms.p99),
	};
	return { summary: { ...summary, direct_mock: directSummary }, raw: { warm: measured, direct } };
}

/**
 * Memory per sleeping agent: N agents each get one turn (so each has history),
 * then everything goes idle with the production idle settings (Rivet:
 * sleepTimeout 30 s; graphrt: idle_timeout_secs 30, agents stopped). RSS is the
 * sum over every runtime process (graphrt gateway + agents; Rivet engine
 * container + node), sampled before, right after the turns (awake) and after
 * the idle period (asleep).
 */
async function memory(system, mock) {
	const n = Number(args.agents ?? (quick ? 1000 : 10_000));
	const concurrency = 32;
	const dispatcher = makeDispatcher();
	if (system.name === "graphrt") {
		await system.start({
			deployments: [{ name: "chat", pool: { max_active_runs: 64, min_replicas: 0, idle_timeout_secs: 30, queue_timeout_secs: 60 }, quotas: { max_queued: 4096 } }],
		});
	} else {
		// The default `pegboard.sqlite_sessions` (4,000 per node connection)
		// refuses the 4,001st awake actor (`actor stopping`); 65,536 is the value
		// of Rivet's own benchmark overrides. Nothing else differs from rivet.json.
		await system.start({ nodes: 1, config: join(ROOT, "rivet", "rivet-10k.json") });
	}
	const turn = (i) =>
		system.name === "graphrt"
			? streamTurn(dispatcher, system.turnSpec("chat", `mem-${i}`, "Hello, who are you?"))
			: streamTurn(dispatcher, system.turnSpec(`mem-${i}`, "Hello, who are you?"));
	// Warm-up: one agent, then wait for the system to be idle again.
	const first = await turn(-1);
	if (!first.ok) throw new Error(`warm-up turn failed: ${first.error}`);
	const settle = async () => {
		if (system.name === "graphrt") await waitUntil(() => system.agentPids().length === 0, 120_000, "graphrt agents to stop", 200);
		else await sleep(45_000); // sleepTimeout 30 s + the sleep sequence
		await sleep(5000);
	};
	await settle();
	// RSS per process tree (graphrt: gateway; Rivet: engine container, node).
	const breakdown = () =>
		Object.fromEntries(system.roots().map((pid, i) => [system.rootNames()[i], round(measure([pid]).rssKib / 1024, 2)]));
	const baseline = measure(system.roots());
	log(`baseline ${round(baseline.rssKib / 1024, 1)} MiB over ${baseline.pids} processes; creating ${n} agents`);
	let errors = 0;
	const errorExamples = [];
	const began = performance.now();
	await forEach(n, concurrency, async (i) => {
		const sample = await turn(i);
		if (!sample.ok) {
			errors += 1;
			if (errors <= 5) {
				errorExamples.push({ agent: i, error: sample.error });
				log(`turn ${i} failed: ${sample.error}`);
			}
		}
	});
	const createSeconds = (performance.now() - began) / 1000;
	const awake = measure(system.roots());
	const awakeBreakdown = breakdown();
	log(`awake ${round(awake.rssKib / 1024, 1)} MiB after ${round(createSeconds, 1)} s; waiting for the idle period`);
	await settle();
	const asleep = measure(system.roots());
	const asleepBreakdown = breakdown();
	// The history must survive the sleep: check a few agents.
	const checks = [];
	for (const i of [0, Math.floor(n / 2), n - 1]) {
		if (system.name === "rivet") checks.push({ agent: i, messages: await system.historyCount(`mem-${i}`) });
	}
	if (system.name === "graphrt") checks.push({ stored_runs: await system.storedRuns() });
	const perAgent = (m) => ({
		total_mib: round(m.rssKib / 1024, 2),
		processes: m.pids,
		per_agent_kib_total: round(m.rssKib / n, 3),
		per_agent_kib_over_baseline: round((m.rssKib - baseline.rssKib) / n, 3),
	});
	return {
		summary: {
			agents: n,
			turn_errors: errors,
			turn_error_examples: errorExamples,
			create_seconds: round(createSeconds, 2),
			baseline: perAgent(baseline),
			awake: { ...perAgent(awake), mib_by_process: awakeBreakdown },
			asleep: { ...perAgent(asleep), mib_by_process: asleepBreakdown },
			disk_bytes: system.diskBytes(),
			disk_bytes_per_agent: round(system.diskBytes() / n, 1),
			history_checks: checks,
			mock_requests: await mock.requests(),
		},
		raw: { baseline: { rssKib: baseline.rssKib, pids: baseline.pids }, awake: { rssKib: awake.rssKib, pids: awake.pids }, asleep: { rssKib: asleep.rssKib, pids: asleep.pids } },
	};
}

/**
 * 1,000 streaming sessions at once: 1,000 distinct sessions (Rivet: actors),
 * each with one earlier turn, all awake; then 1,000 turns start together, the
 * mock streams 20 tokens 50 ms apart (~1 s per stream). Peak RSS and CPU over
 * every runtime process, stalls (a gap of more than 1 s between two tokens, or
 * a failed stream), first-token and inter-token latency.
 * `--replicas N`: graphrt agent processes / Rivet node processes (default 1).
 */
async function concurrent(system, mock) {
	const streams = quick ? 200 : 1000;
	const replicas = Number(args.replicas ?? 1);
	const stallMs = 1000;
	const dispatcher = makeDispatcher();
	// The floor: the same client, the same burst, straight to the mock.
	await preconnect(dispatcher, `http://127.0.0.1:${mock.port}`, "/health", streams);
	const direct = await Promise.all(Array.from({ length: streams }, () => streamTurn(dispatcher, mock.turnSpec(), { keepGaps: true })));
	const directOk = okSamples(direct);
	if (system.name === "graphrt") {
		await system.start({
			deployments: [{
				name: "chat",
				pool: { max_active_runs: Math.ceil(streams / replicas), min_replicas: replicas, idle_timeout_secs: 3600, queue_timeout_secs: 60 },
				quotas: { max_replicas: replicas, max_queued: streams * 2 },
			}],
		});
		await waitUntil(() => system.agentPids().length >= replicas, 30_000, "graphrt warm replicas");
	} else {
		await system.start({ nodes: replicas });
	}
	const spec = (i, text) => (system.name === "graphrt" ? system.turnSpec("chat", `s-${i}`, text) : system.turnSpec(`s-${i}`, text));
	// Every session gets one turn first (Rivet: creates its actor), well within
	// Rivet's 30 s sleepTimeout of the measured round.
	await forEach(streams, 32, async (i) => {
		const sample = await streamTurn(dispatcher, spec(i, "Hello, who are you?"), { expectedTokens: 20 });
		if (!sample.ok) throw new Error(`setup turn ${i}: ${sample.error}`);
		if (system.name === "graphrt") system.remember(`s-${i}`, "Hello, who are you?");
	});
	// One kept-alive connection per stream, opened before the burst (each
	// system's health route), so the burst measures streaming, not 1,000
	// simultaneous TCP handshakes from one client.
	const origin = system.name === "graphrt" ? `http://127.0.0.1:${system.port}` : system.origin;
	await preconnect(dispatcher, origin, system.name === "graphrt" ? "/healthz" : "/health", streams);
	await sleep(2000);
	const sampler = startSamplerThread(system.roots(), 100);
	await sampler.ready;
	const began = performance.now();
	const results = await Promise.all(Array.from({ length: streams }, (_, i) => streamTurn(dispatcher, spec(i, "Tell me more."), { keepGaps: true })));
	const wallMs = performance.now() - began;
	const usage = await sampler.stop();
	const ok = okSamples(results);
	const gaps = ok.flatMap((s) => s.gapsMs);
	return {
		summary: {
			streams,
			replicas,
			chunk_interval_ms: 50,
			wall_ms: round(wallMs, 1),
			failed_streams: results.length - ok.length,
			failure_examples: results.filter((s) => !s.ok).slice(0, 3).map((s) => s.error),
			stalled_streams: ok.filter((s) => s.maxGapMs > stallMs).length,
			ttft_ms: summarize(ok.map((s) => s.ttftMs)),
			inter_token_gap_ms: summarize(gaps),
			max_gap_per_stream_ms: summarize(ok.map((s) => s.maxGapMs)),
			done_ms: summarize(ok.map((s) => s.doneMs)),
			usage,
			direct_mock: {
				failed_streams: direct.length - directOk.length,
				ttft_ms: summarize(directOk.map((s) => s.ttftMs)),
				inter_token_gap_ms: summarize(directOk.flatMap((s) => s.gapsMs)),
				done_ms: summarize(directOk.map((s) => s.doneMs)),
			},
			mock_requests: await mock.requests(),
		},
		raw: { streams: results },
	};
}

/**
 * Recovery after kill -9 of every runtime process (graphrt: gateway and
 * agents; Rivet: engine container and node). Restart, then time from the
 * restart command to the first successful streamed turn of an existing agent,
 * and check that its earlier turn survived.
 */
async function recovery(system, mock) {
	const rounds = quick ? 2 : 5;
	const dispatcher = makeDispatcher();
	if (system.name === "graphrt") {
		await system.start({ deployments: [{ name: "chat", pool: { max_active_runs: 64, min_replicas: 0, idle_timeout_secs: 300 } }] });
	} else {
		await system.start({ nodes: 1 });
	}
	const spec = (text) => (system.name === "graphrt" ? system.turnSpec("chat", "survivor", text) : system.turnSpec("survivor", text));
	const out = [];
	for (let r = 0; r < rounds; r++) {
		const before = await streamTurn(dispatcher, spec(`Before crash ${r}`));
		if (!before.ok) throw new Error(`turn before the crash failed: ${before.error}`);
		if (system.name === "graphrt") system.remember("survivor", `Before crash ${r}`);
		const storedBefore = system.name === "graphrt" ? await system.storedRuns() : await system.historyCount("survivor");
		await system.kill9();
		const t0 = performance.now();
		await system.restart();
		const readyMs = performance.now() - t0;
		let sample;
		let attempts = 0;
		const fresh = makeDispatcher();
		for (;;) {
			attempts += 1;
			sample = await streamTurn(fresh, spec(`After crash ${r}`));
			if (sample.ok || performance.now() - t0 > 120_000) break;
			await sleep(10);
		}
		const firstTurnMs = performance.now() - t0;
		await fresh.close();
		if (system.name === "graphrt" && sample.ok) system.remember("survivor", `After crash ${r}`);
		const storedAfter = system.name === "graphrt" ? await system.storedRuns() : await system.historyCount("survivor");
		out.push({
			round: r,
			restart_ready_ms: round(readyMs, 1),
			first_turn_ms: round(firstTurnMs, 1),
			attempts,
			ok: sample.ok,
			error: sample.error,
			// graphrt: stored runs in the gateway store; Rivet: messages in the actor's SQLite.
			stored_before: storedBefore,
			stored_after: storedAfter,
		});
		log(`round ${r}: first turn ${round(firstTurnMs, 1)} ms after the restart (${attempts} attempts)`);
	}
	return {
		summary: {
			rounds,
			first_turn_ms: summarize(out.map((o) => o.first_turn_ms)),
			restart_ready_ms: summarize(out.map((o) => o.restart_ready_ms)),
			all_ok: out.every((o) => o.ok),
			data_kept: out.every((o) => o.stored_after != null && o.stored_before != null && o.stored_after > o.stored_before),
		},
		raw: { rounds: out },
	};
}

const SCENARIOS = { cold, warm, memory, concurrent, recovery };

/**
 * The machine may run other work. Before a run, wait (up to 20 min) until the
 * 1-minute load average is at most BENCH_MAX_LOAD (default 6 on this 24-thread
 * host), so both systems are measured on a similarly quiet machine.
 */
async function waitForQuietMachine() {
	const maxLoad = Number(process.env.BENCH_MAX_LOAD ?? 6);
	const began = Date.now();
	let warned = false;
	while (loadavg()[0] > maxLoad && Date.now() - began < 20 * 60_000) {
		if (!warned) {
			log(`waiting for the 1-min load average (${loadavg()[0].toFixed(2)}) to drop to ${maxLoad}`);
			warned = true;
		}
		await sleep(5000);
	}
	return { waitedSeconds: Math.round((Date.now() - began) / 1000), maxLoad };
}

async function main() {
	if (!SCENARIOS[scenario] || !["graphrt", "rivet"].includes(systemName)) {
		console.error("usage: node bench.mjs <cold|warm|memory|concurrent|recovery> <graphrt|rivet> [--run N] [--out DIR] [--quick]");
		process.exit(2);
	}
	const quiet = await waitForQuietMachine();
	const mock = await new Mock(ctx).start(scenario === "concurrent" ? { chunks: 20, intervalMs: 50 } : { chunks: 20, intervalMs: 0 });
	const system = systemName === "graphrt" ? new Graphrt(ctx, mock) : new Rivet(ctx, mock);
	const startedAt = new Date().toISOString();
	const load = loadavg();
	let result;
	try {
		result = await SCENARIOS[scenario](system, mock);
	} finally {
		await system.stop();
		await mock.stop();
	}
	const variant = [transport === "ws" ? "ws" : null, args.replicas ? `x${args.replicas}` : null].filter(Boolean).join("-");
	const file = join(outDir, `${scenario}-${systemName}${variant ? `-${variant}` : ""}-run${runIndex}.json`);
	const document = {
		meta: {
			scenario,
			system: systemName,
			transport: systemName === "rivet" ? transport : "http",
			variant: variant || null,
			run: runIndex,
			quick,
			started_at: startedAt,
			ended_at: new Date().toISOString(),
			loadavg_at_start: load.map((x) => round(x, 2)),
			loadavg_at_end: loadavg().map((x) => round(x, 2)),
			waited_for_quiet_s: quiet.waitedSeconds,
			max_load: quiet.maxLoad,
			host: { cpu: cpus()[0]?.model, logical_cpus: cpus().length, memory_gib: round(totalmem() / 2 ** 30, 1), kernel: execFileSync("uname", ["-sr"]).toString().trim() },
		},
		summary: result.summary,
		raw: result.raw,
	};
	writeFileSync(file, `${JSON.stringify(document)}\n`);
	console.log(JSON.stringify({ file, summary: result.summary }, null, 1));
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});
