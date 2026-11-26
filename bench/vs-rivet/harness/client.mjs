// The one HTTP client both runtimes are measured with: a streamed chat turn
// over HTTP/1.1 (undici, kept-alive connections), timed from just before the
// request is written to the response headers, the first model token and the
// end of the stream.
//
// Tokens are recognized by the mock's text (`tok0 `, `tok1 `, ...), which both
// runtimes forward unchanged, so "first token" means the same bytes on both
// sides regardless of each runtime's event format.
import { Agent, request } from "undici";

export function makeDispatcher(connections = null) {
	return new Agent({
		keepAliveTimeout: 60_000,
		keepAliveMaxTimeout: 600_000,
		headersTimeout: 120_000,
		bodyTimeout: 120_000,
		connections,
		pipelining: 1,
	});
}

const TOKEN = /tok(\d+) /g;
const nowMs = () => performance.now();

/**
 * One streamed turn. `spec` = { origin, path, headers, body, doneMarker }.
 * Resolves (never rejects) to a sample: { ok, status, ttfbMs, ttftMs, doneMs,
 * tokens, gapsMs?, maxGapMs, cold, error }.
 */
export async function streamTurn(dispatcher, spec, { keepGaps = false, expectedTokens = 20 } = {}) {
	const t0 = nowMs();
	let ttfbMs = null;
	let ttftMs = null;
	let status = 0;
	let error = null;
	let text = "";
	let maxIndex = -1;
	let lastTokenAt = null;
	let maxGapMs = 0;
	const gaps = keepGaps ? [] : null;
	let cold = null;
	try {
		const response = await request(`${spec.origin}${spec.path}`, {
			method: "POST",
			dispatcher,
			headers: { "content-type": "application/json", accept: "text/event-stream", ...spec.headers },
			body: JSON.stringify(spec.body),
		});
		ttfbMs = nowMs() - t0;
		status = response.statusCode;
		cold = response.headers["x-bench-cold"] ?? null;
		for await (const chunk of response.body) {
			const at = nowMs();
			const before = text.length;
			text += chunk.toString("utf8");
			TOKEN.lastIndex = Math.max(0, before - 12);
			let match;
			while ((match = TOKEN.exec(text)) !== null) {
				const index = Number(match[1]);
				if (index <= maxIndex) continue;
				// A delta arrives once per new index; a runtime that repeats the text
				// (graphrt sends each delta as `response` and `node_chunk`) is counted once.
				for (let i = maxIndex + 1; i <= index; i++) {
					if (i === 0) ttftMs = at - t0;
					if (lastTokenAt !== null && i > 0) {
						const gap = at - lastTokenAt;
						if (gap > maxGapMs) maxGapMs = gap;
						if (gaps) gaps.push(gap);
					}
					lastTokenAt = at;
				}
				maxIndex = index;
			}
			// Keep the buffer small: only the tail matters for token matching and
			// the done marker.
			if (text.length > 65_536) text = text.slice(-4096);
		}
		if (status < 200 || status >= 300) error = `HTTP ${status}: ${text.slice(0, 300)}`;
		else if (!text.includes(spec.doneMarker)) error = `no ${spec.doneMarker}: ${text.slice(-300)}`;
		else if (maxIndex + 1 < expectedTokens) error = `${maxIndex + 1}/${expectedTokens} tokens`;
	} catch (e) {
		error = String(e?.message ?? e).slice(0, 300);
	}
	const doneMs = nowMs() - t0;
	const sample = {
		ok: error === null,
		status,
		ttfbMs: r3(ttfbMs),
		ttftMs: r3(ttftMs),
		doneMs: r3(doneMs),
		tokens: maxIndex + 1,
		maxGapMs: r3(maxGapMs),
		cold,
		error,
	};
	if (gaps) sample.gapsMs = gaps.map(r3);
	return sample;
}

function r3(x) {
	return x == null ? null : Math.round(x * 1000) / 1000;
}

/**
 * Opens `count` kept-alive connections to `origin` (GET `path`, all at once)
 * so a following burst reuses them instead of racing 1,000 TCP handshakes.
 */
export async function preconnect(dispatcher, origin, path, count) {
	await Promise.all(
		Array.from({ length: count }, async () => {
			const response = await request(`${origin}${path}`, { dispatcher });
			await response.body.text();
		}),
	);
}

/** Runs `f(i)` for i in [0, n) with `concurrency` workers. */
export async function forEach(n, concurrency, f) {
	let next = 0;
	const worker = async () => {
		for (;;) {
			const i = next++;
			if (i >= n) return;
			await f(i);
		}
	};
	await Promise.all(Array.from({ length: Math.max(1, Math.min(concurrency, n)) }, worker));
}

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export async function waitUntil(condition, timeoutMs, what, intervalMs = 50) {
	const deadline = Date.now() + timeoutMs;
	for (;;) {
		if (await condition()) return;
		if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
		await sleep(intervalMs);
	}
}

/** Nearest-rank percentiles (the method of Rivet's published benchmark). */
export function summarize(values) {
	const xs = values.filter((x) => typeof x === "number").sort((a, b) => a - b);
	if (xs.length === 0) return { n: 0 };
	const pct = (p) => xs[Math.min(xs.length, Math.max(1, Math.ceil((p / 100) * xs.length))) - 1];
	const mean = xs.reduce((a, b) => a + b, 0) / xs.length;
	return { n: xs.length, p50: r3(pct(50)), p95: r3(pct(95)), p99: r3(pct(99)), max: r3(xs[xs.length - 1]), mean: r3(mean) };
}
