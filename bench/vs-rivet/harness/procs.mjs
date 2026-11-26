// Process accounting from /proc: resident memory and CPU time of a set of
// process trees (a runtime and everything it started, including the processes
// of a Docker container, which are ordinary host processes).
import { readFileSync, readdirSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { Worker, isMainThread, parentPort, workerData } from "node:worker_threads";

const CLK_TCK = Number(execFileSync("getconf", ["CLK_TCK"]).toString().trim()) || 100;

function readStat(pid) {
	try {
		const text = readFileSync(`/proc/${pid}/stat`, "utf8");
		// The command name may contain spaces and parentheses: fields start after the last ')'.
		const fields = text.slice(text.lastIndexOf(")") + 2).split(" ");
		return { ppid: Number(fields[1]), utime: Number(fields[11]), stime: Number(fields[12]) };
	} catch {
		return null;
	}
}

function statusKib(pid, key) {
	try {
		const text = readFileSync(`/proc/${pid}/status`, "utf8");
		const match = text.match(new RegExp(`^${key}:\\s+(\\d+) kB`, "m"));
		return match ? Number(match[1]) : 0;
	} catch {
		return 0;
	}
}

/** Every live pid on the host with its parent. */
function processTable() {
	const table = new Map();
	for (const name of readdirSync("/proc")) {
		if (!/^\d+$/.test(name)) continue;
		const stat = readStat(name);
		if (stat) table.set(Number(name), stat);
	}
	return table;
}

/** The given roots and all their descendants (live ones only). */
export function descendants(roots) {
	const table = processTable();
	const children = new Map();
	for (const [pid, stat] of table) {
		if (!children.has(stat.ppid)) children.set(stat.ppid, []);
		children.get(stat.ppid).push(pid);
	}
	const out = new Set();
	const stack = roots.filter((pid) => pid && table.has(pid));
	while (stack.length) {
		const pid = stack.pop();
		if (out.has(pid)) continue;
		out.add(pid);
		for (const child of children.get(pid) ?? []) stack.push(child);
	}
	return [...out];
}

/** Sum of VmRSS (KiB) and of CPU time (seconds) over the trees of `roots`. */
export function measure(roots) {
	const pids = descendants(roots);
	let rssKib = 0;
	let cpuTicks = 0;
	const ticks = new Map();
	for (const pid of pids) {
		rssKib += statusKib(pid, "VmRSS");
		const stat = readStat(pid);
		if (stat) {
			cpuTicks += stat.utime + stat.stime;
			ticks.set(pid, stat.utime + stat.stime);
		}
	}
	return { pids: pids.length, rssKib, cpuSeconds: cpuTicks / CLK_TCK, ticks };
}

/**
 * Samples `measure(roots())` every `intervalMs` until stopped: peak RSS (sum
 * over the trees at one instant) and CPU time used during the window.
 */
export class Sampler {
	constructor(roots, intervalMs = 100) {
		this.roots = roots;
		this.intervalMs = intervalMs;
		this.peakRssKib = 0;
		this.samples = [];
		this.timer = null;
	}

	start() {
		this.first = measure(this.roots());
		// CPU ticks per pid, last seen: a process that exits mid-window keeps
		// the CPU time of its last sample.
		this.lastTicks = new Map(this.first.ticks);
		this.startedAt = performance.now();
		this.peakRssKib = this.first.rssKib;
		this.timer = setInterval(() => this.tick(), this.intervalMs);
		return this;
	}

	tick() {
		const m = measure(this.roots());
		this.peakRssKib = Math.max(this.peakRssKib, m.rssKib);
		for (const [pid, ticks] of m.ticks) this.lastTicks.set(pid, ticks);
		this.samples.push({ t: Math.round(performance.now() - this.startedAt), rssKib: m.rssKib, cpuSeconds: m.cpuSeconds, pids: m.pids });
	}

	stop() {
		clearInterval(this.timer);
		this.tick();
		const last = this.samples[this.samples.length - 1];
		const wallSeconds = (performance.now() - this.startedAt) / 1000;
		let ticks = 0;
		for (const [pid, value] of this.lastTicks) ticks += value - (this.first.ticks.get(pid) ?? 0);
		const cpuSeconds = Math.max(0, ticks / CLK_TCK);
		return {
			wallSeconds: round(wallSeconds, 3),
			cpuSeconds: round(cpuSeconds, 3),
			avgCores: round(cpuSeconds / wallSeconds, 3),
			rssBeforeMib: round(this.first.rssKib / 1024, 2),
			peakRssMib: round(this.peakRssKib / 1024, 2),
			rssAfterMib: round(last.rssKib / 1024, 2),
			maxPids: Math.max(...this.samples.map((s) => s.pids), this.first.pids),
		};
	}
}

export function round(x, digits = 3) {
	if (x == null || Number.isNaN(x)) return null;
	const f = 10 ** digits;
	return Math.round(x * f) / f;
}

/**
 * A `Sampler` over fixed `roots` in a worker thread, so reading /proc never
 * blocks the event loop of the load generator. `stop()` resolves to the summary.
 */
export function startSamplerThread(roots, intervalMs = 100) {
	const worker = new Worker(new URL(import.meta.url), { workerData: { roots, intervalMs } });
	const ready = new Promise((resolve, reject) => {
		worker.once("message", resolve);
		worker.once("error", reject);
	});
	return {
		ready,
		stop: async () => {
			await ready;
			const result = new Promise((resolve, reject) => {
				worker.once("message", resolve);
				worker.once("error", reject);
			});
			worker.postMessage("stop");
			const summary = await result;
			await worker.terminate();
			return summary;
		},
	};
}

if (!isMainThread && workerData?.roots) {
	const sampler = new Sampler(() => workerData.roots, workerData.intervalMs).start();
	parentPort.postMessage("ready");
	parentPort.on("message", () => parentPort.postMessage(sampler.stop()));
}
