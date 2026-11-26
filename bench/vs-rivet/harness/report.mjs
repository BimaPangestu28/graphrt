// Aggregates the per-run JSON files of one results directory: the median of
// every metric across runs (and the min–max range), as summary.json and
// summary.md.
//
//   node report.mjs <results dir>
import { readdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const dir = process.argv[2];
if (!dir) {
	console.error("usage: node report.mjs <results dir>");
	process.exit(2);
}

const docs = readdirSync(dir)
	.filter((f) => /-run\d+\.json$/.test(f))
	.map((f) => JSON.parse(readFileSync(join(dir, f), "utf8")));

const label = (d) => `${d.meta.system}${d.meta.variant ? ` (${d.meta.variant})` : ""}`;
const groups = new Map();
for (const d of docs) {
	const key = `${d.meta.scenario}|${label(d)}`;
	if (!groups.has(key)) groups.set(key, []);
	groups.get(key).push(d);
}

function get(object, path) {
	return path.split(".").reduce((o, k) => (o == null ? undefined : o[k]), object);
}

function median(xs) {
	const s = xs.filter((x) => typeof x === "number").sort((a, b) => a - b);
	if (!s.length) return null;
	const m = Math.floor(s.length / 2);
	return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
}

function stat(runs, path) {
	const xs = runs.map((d) => get(d.summary, path)).filter((x) => typeof x === "number");
	if (!xs.length) return null;
	return { median: median(xs), min: Math.min(...xs), max: Math.max(...xs), runs: xs.length };
}

const METRICS = {
	cold: [
		["wake.ttft_ms.p50", "wake → first token p50 (ms)"],
		["wake.ttft_ms.p95", "wake → first token p95 (ms)"],
		["wake.ttft_ms.p99", "wake → first token p99 (ms)"],
		["wake.done_ms.p50", "wake → stream end p50 (ms)"],
		["wake.done_ms.p99", "wake → stream end p99 (ms)"],
		["wake.samples", "wake samples per run"],
		["wake.errors", "wake errors"],
		["create.ttft_ms.p50", "first use (create) → first token p50 (ms)"],
		["create.ttft_ms.p95", "first use (create) → first token p95 (ms)"],
		["create.ttft_ms.p99", "first use (create) → first token p99 (ms)"],
	],
	warm: [
		["ttft_ms.p50", "first token p50 (ms)"],
		["ttft_ms.p95", "first token p95 (ms)"],
		["ttft_ms.p99", "first token p99 (ms)"],
		["overhead_ttft_ms.p50", "overhead over the mock p50 (ms)"],
		["overhead_ttft_ms.p99", "overhead over the mock p99 (ms)"],
		["ttfb_ms.p50", "response headers p50 (ms)"],
		["done_ms.p50", "stream end p50 (ms)"],
		["done_ms.p99", "stream end p99 (ms)"],
		["direct_mock.ttft_ms.p50", "mock direct, first token p50 (ms)"],
		["samples", "samples per run"],
		["errors", "errors"],
	],
	memory: [
		["agents", "agents"],
		["baseline.total_mib", "RSS before (MiB)"],
		["awake.total_mib", "RSS with all agents just used (MiB)"],
		["asleep.total_mib", "RSS after the idle period (MiB)"],
		["asleep.per_agent_kib_total", "asleep: total RSS / N (KiB)"],
		["asleep.per_agent_kib_over_baseline", "asleep: (RSS − baseline) / N (KiB)"],
		["awake.per_agent_kib_over_baseline", "awake: (RSS − baseline) / N (KiB)"],
		["asleep.processes", "processes after the idle period"],
		["disk_bytes_per_agent", "disk per agent (bytes)"],
		["create_seconds", "time to give N agents one turn (s, concurrency 32)"],
		["turn_errors", "turn errors"],
	],
	concurrent: [
		["streams", "streams"],
		["usage.rssBeforeMib", "RSS before the burst (MiB)"],
		["usage.peakRssMib", "peak RSS (MiB)"],
		["usage.cpuSeconds", "CPU time (s)"],
		["usage.avgCores", "average cores busy"],
		["failed_streams", "failed streams"],
		["stalled_streams", "stalled streams (gap > 1 s)"],
		["ttft_ms.p50", "first token p50 (ms)"],
		["ttft_ms.p99", "first token p99 (ms)"],
		["inter_token_gap_ms.p50", "token gap p50 (ms, mock: 50)"],
		["inter_token_gap_ms.p99", "token gap p99 (ms)"],
		["max_gap_per_stream_ms.max", "worst gap (ms)"],
		["done_ms.p99", "stream end p99 (ms)"],
		["direct_mock.ttft_ms.p99", "mock direct, first token p99 (ms)"],
		["direct_mock.inter_token_gap_ms.p99", "mock direct, token gap p99 (ms)"],
	],
	recovery: [
		["first_turn_ms.p50", "restart → first good turn p50 (ms)"],
		["first_turn_ms.max", "restart → first good turn max (ms)"],
		["restart_ready_ms.p50", "restart → processes ready p50 (ms)"],
		["rounds", "rounds per run"],
	],
};

const BOOLEANS = { recovery: [["all_ok", "every turn after the restart succeeded"], ["data_kept", "earlier turns kept"]] };

function fmt(s) {
	if (!s) return "–";
	const n = (x) => (Math.abs(x) >= 100 ? x.toFixed(0) : Math.abs(x) >= 10 ? x.toFixed(1) : x.toFixed(2));
	return s.min === s.max ? n(s.median) : `${n(s.median)} (${n(s.min)}–${n(s.max)})`;
}

const summary = {};
let md = `# graphrt vs Rivet: medians across runs\n\nGenerated from \`${dir}\`. Cells: median across runs (min–max across runs).\n`;
if (existsSync(join(dir, "versions.json"))) {
	const v = JSON.parse(readFileSync(join(dir, "versions.json"), "utf8"));
	md += `\n${v.graphrt} · ${v.rivet_engine_image} + rivetkit ${v.rivetkit} · Node.js ${v.node} · ${v.cpu} (${v.logical_cpus} threads), ${v.memory_gib} GiB · ${v.kernel}\n`;
}
for (const [scenario, metrics] of Object.entries(METRICS)) {
	const labels = [...groups.keys()].filter((k) => k.startsWith(`${scenario}|`)).map((k) => k.split("|")[1]).sort();
	if (!labels.length) continue;
	summary[scenario] = {};
	md += `\n## ${scenario}\n\n| metric | ${labels.join(" | ")} |\n|---|${labels.map(() => "---:").join("|")}|\n`;
	for (const [path, title] of metrics) {
		const cells = labels.map((l) => {
			const s = stat(groups.get(`${scenario}|${l}`), path);
			(summary[scenario][l] ??= {})[path] = s;
			return fmt(s);
		});
		md += `| ${title} | ${cells.join(" | ")} |\n`;
	}
	for (const [path, title] of BOOLEANS[scenario] ?? []) {
		const cells = labels.map((l) => {
			const runs = groups.get(`${scenario}|${l}`);
			const ok = runs.filter((d) => get(d.summary, path) === true).length;
			(summary[scenario][l] ??= {})[path] = `${ok}/${runs.length}`;
			return `${ok}/${runs.length} runs`;
		});
		md += `| ${title} | ${cells.join(" | ")} |\n`;
	}
	const loads = labels.map((l) => {
		const xs = groups.get(`${scenario}|${l}`).map((d) => d.meta.loadavg_at_start?.[0]).filter((x) => typeof x === "number");
		return xs.length ? `${Math.min(...xs).toFixed(1)}–${Math.max(...xs).toFixed(1)}` : "–";
	});
	md += `| 1-min load average at start | ${loads.join(" | ")} |\n`;
}

writeFileSync(join(dir, "summary.json"), `${JSON.stringify(summary, null, 1)}\n`);
writeFileSync(join(dir, "summary.md"), md);
console.log(md);
