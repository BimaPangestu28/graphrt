// Start, stop and address the two runtimes under test, and the shared mock model.
//
// graphrt: the 0.6.2 release binary (gateway build) as `graphrt gateway` with
// local-process agents (`graphrt serve --trust-gateway` of the same binary,
// started on demand and stopped when idle).
//
// Rivet: the `rivetdev/engine` image (single instance, the Docker Compose
// guide's config) plus RivetKit node processes (`node dist/server.js`) that
// connect to it. Clients reach actors through the engine's gateway.
import { spawn, execFileSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { mkdirSync, writeFileSync, statSync, existsSync, openSync } from "node:fs";
import { join } from "node:path";
import { createServer } from "node:net";
import { request } from "undici";
import { descendants } from "./procs.mjs";
import { sleep, waitUntil, forEach } from "./client.mjs";

export async function freePort() {
	return new Promise((resolve, reject) => {
		const server = createServer();
		server.listen(0, "127.0.0.1", () => {
			const { port } = server.address();
			server.close(() => resolve(port));
		});
		server.on("error", reject);
	});
}

async function httpOk(url, init = {}) {
	try {
		const response = await request(url, init);
		const body = await response.body.text();
		return { ok: response.statusCode >= 200 && response.statusCode < 300, status: response.statusCode, body };
	} catch (error) {
		return { ok: false, status: 0, body: String(error) };
	}
}

function signal(pid, sig) {
	try {
		process.kill(pid, sig);
		return true;
	} catch {
		return false;
	}
}

function alive(pid) {
	return signal(pid, 0);
}

async function terminate(pids, graceMs = 10_000) {
	for (const pid of pids) signal(pid, "SIGTERM");
	const deadline = Date.now() + graceMs;
	while (pids.some(alive) && Date.now() < deadline) await sleep(20);
	for (const pid of pids.filter(alive)) signal(pid, "SIGKILL");
}

// --------------------------------------------------------------------------- mock model

export class Mock {
	constructor(ctx) {
		this.ctx = ctx;
	}

	async start({ chunks = 20, intervalMs = 0 } = {}) {
		this.port = await freePort();
		const log = openSync(join(this.ctx.work, `mock-${this.port}.log`), "a");
		this.process = spawn(this.ctx.mockBin, ["--port", String(this.port), "--chunks", String(chunks), "--interval-ms", String(intervalMs)], {
			stdio: ["ignore", log, log],
		});
		await waitUntil(async () => (await httpOk(`http://127.0.0.1:${this.port}/health`)).ok, 10_000, "mock model");
		this.baseUrl = `http://127.0.0.1:${this.port}/v1`;
		return this;
	}

	async requests() {
		const { body } = await httpOk(`http://127.0.0.1:${this.port}/stats`);
		return JSON.parse(body).requests;
	}

	turnSpec() {
		return {
			origin: `http://127.0.0.1:${this.port}`,
			path: "/v1/chat/completions",
			headers: { authorization: "Bearer bench" },
			body: { model: "gpt-4o-mini", stream: true, messages: [{ role: "user", content: "hi" }] },
			doneMarker: "[DONE]",
		};
	}

	async stop() {
		if (this.process) await terminate([this.process.pid], 2000);
	}
}

// --------------------------------------------------------------------------- graphrt

const ASSISTANT_TEXT = Array.from({ length: 20 }, (_, i) => `tok${i} `).join("");

export class Graphrt {
	name = "graphrt";

	constructor(ctx, mock) {
		this.ctx = ctx;
		this.mock = mock;
		this.apiKey = `bench-${randomBytes(16).toString("hex")}`;
		this.adminToken = randomBytes(16).toString("hex");
		this.agentKeys = `k1:${randomBytes(32).toString("hex")}`;
		// graphrt keeps no conversation state: the client holds each session's
		// history and sends it with every turn (sys.history).
		this.histories = new Map();
	}

	/** deployments: [{ name, pool, quotas }] */
	async start({ deployments }) {
		this.dir = join(this.ctx.work, `graphrt-${Date.now().toString(36)}-${randomBytes(3).toString("hex")}`);
		mkdirSync(this.dir, { recursive: true });
		this.port = await freePort();
		const digest = createHash("sha256").update(this.apiKey).digest("hex");
		const config = {
			listen: `127.0.0.1:${this.port}`,
			store: join(this.dir, "gateway.db"),
			socket_dir: join(this.dir, "sock"),
			admin_token_env: "BENCH_ADMIN_TOKEN",
			deployments: deployments.map((d) => ({ name: d.name, keys_env: "BENCH_AGENT_KEYS", api_keys: [digest], pool: d.pool, quotas: d.quotas })),
		};
		this.configPath = join(this.dir, "gateway.yaml");
		writeFileSync(this.configPath, JSON.stringify(config, null, 1));
		this.deployments = deployments;
		await this.spawnGateway();
		await forEach(deployments.length, 16, async (i) => {
			const d = deployments[i];
			const result = await httpOk(`http://127.0.0.1:${this.port}/admin/v1/deployments/${d.name}/versions`, {
				method: "POST",
				headers: { "content-type": "application/json", authorization: `Bearer ${this.adminToken}` },
				body: JSON.stringify({
					version: "v1",
					workflow: "agent",
					fingerprint: "",
					target: "local",
					// The agent's own cap (`serve --max-active-runs`, default 256) must be
					// at least the pool's per-replica limit, or the extra runs are refused.
					command: [this.ctx.graphrtBin, "serve", this.ctx.graphrtWorkflow, "--max-active-runs", String(d.pool.max_active_runs)],
					secret_inputs: [],
				}),
			});
			if (!result.ok) throw new Error(`registering ${d.name}: ${result.status} ${result.body}`);
		});
		return this;
	}

	async spawnGateway() {
		const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith("GRAPHRT_")));
		Object.assign(env, {
			BENCH_ADMIN_TOKEN: this.adminToken,
			BENCH_AGENT_KEYS: this.agentKeys,
			OPENAI_API_KEY: "bench",
			OPENAI_BASE_URL: this.mock.baseUrl,
		});
		const log = openSync(join(this.dir, "gateway.log"), "a");
		this.process = spawn(this.ctx.graphrtBin, ["gateway", "--config", this.configPath], { stdio: ["ignore", log, log], env });
		this.spawnedAt = performance.now();
		await waitUntil(async () => (await httpOk(`http://127.0.0.1:${this.port}/healthz`)).ok, 15_000, "graphrt gateway /healthz", 5);
	}

	roots() {
		return [this.process.pid];
	}

	rootNames() {
		return ["gateway (+ agents)"];
	}

	/** Agent processes the gateway runs right now. */
	agentPids() {
		return descendants([this.process.pid]).filter((pid) => pid !== this.process.pid);
	}

	turnSpec(deployment, session, text) {
		const history = this.histories.get(session) ?? [];
		return {
			origin: `http://127.0.0.1:${this.port}`,
			path: `/v1/deployments/${deployment}/runs`,
			headers: { "x-api-key": this.apiKey },
			body: { stream: true, session_id: session, sys: { query: text, history } },
			doneMarker: "run_succeeded",
		};
	}

	/** Records a finished turn in the client-held history (the last 10 turns). */
	remember(session, text) {
		const history = this.histories.get(session) ?? [];
		history.push({ role: "user", content: text }, { role: "assistant", content: ASSISTANT_TEXT });
		this.histories.set(session, history.slice(-20));
	}

	diskBytes() {
		let total = 0;
		for (const suffix of ["", "-wal", "-shm"]) {
			const path = join(this.dir, `gateway.db${suffix}`);
			if (existsSync(path)) total += statSync(path).size;
		}
		return total;
	}

	/** Stored runs of a session (the durable record the gateway keeps). */
	async storedRuns() {
		const db = join(this.dir, "gateway.db");
		try {
			return Number(execFileSync("sqlite3", [db, "SELECT count(*) FROM runs"]).toString().trim());
		} catch {
			return null;
		}
	}

	async kill9() {
		const pids = descendants([this.process.pid]);
		for (const pid of pids) signal(pid, "SIGKILL");
		await waitUntil(() => !pids.some(alive), 10_000, "graphrt processes to die", 5);
	}

	async restart() {
		await this.spawnGateway();
	}

	async stop() {
		if (!this.process) return;
		const pids = descendants([this.process.pid]);
		await terminate([this.process.pid], 20_000);
		await terminate(pids.filter(alive), 5000);
	}
}

// --------------------------------------------------------------------------- Rivet

export class Rivet {
	name = "rivet";

	constructor(ctx, mock) {
		this.ctx = ctx;
		this.mock = mock;
		this.token = randomBytes(32).toString("hex");
		this.id = `${Date.now().toString(36)}-${randomBytes(3).toString("hex")}`;
		this.container = `rivet-bench-${this.id}`;
		this.volumes = [`rivet-bench-log-${this.id}`, `rivet-bench-objects-${this.id}`];
		this.nodes = [];
		this.origin = "http://127.0.0.1:6420";
	}

	/** `config`: engine config file (default rivet/rivet.json, the Docker Compose guide's). */
	async start({ nodes = 1, sleepTimeoutMs = null, config = this.ctx.rivetConfig } = {}) {
		this.nodeCount = nodes;
		this.sleepTimeoutMs = sleepTimeoutMs;
		this.dir = join(this.ctx.work, `rivet-${this.id}`);
		mkdirSync(this.dir, { recursive: true });
		for (const port of [6420, 6421, 6430]) {
			const busy = await httpOk(`http://127.0.0.1:${port}/`);
			if (busy.status !== 0) throw new Error(`port ${port} is in use; stop the other Rivet engine first`);
		}
		execFileSync("docker", [
			"run", "-d", "--name", this.container,
			// Host networking: no docker-proxy hop on the measured path (graphrt runs natively too).
			"--network", "host",
			"--ulimit", "nofile=1048576:1048576",
			"-e", `RIVET__AUTH__ADMIN_TOKEN=${this.token}`,
			"-v", `${config}:/etc/rivet/config.json:ro`,
			"-v", `${this.volumes[0]}:/var/lib/rivet/log`,
			"-v", `${this.volumes[1]}:/var/lib/rivet/objects`,
			this.ctx.rivetImage,
		], { stdio: ["ignore", "ignore", "inherit"] });
		await this.waitEngine();
		// Create the default pool (RivetKit only does it itself in local dev);
		// the engine answers depot.overloaded for a few seconds after start.
		await waitUntil(async () => {
			const result = await httpOk(`${this.origin}/pools/default?namespace=default`, {
				method: "PUT",
				headers: { authorization: `Bearer ${this.token}`, "content-type": "application/json" },
				body: "{}",
			});
			return result.ok;
		}, 60_000, "Rivet pool creation", 250);
		await this.startNodes();
		return this;
	}

	async waitEngine() {
		await waitUntil(async () => (await httpOk(`${this.origin}/health`)).ok, 60_000, "Rivet engine /health", 5);
	}

	enginePid() {
		const pid = Number(execFileSync("docker", ["inspect", "-f", "{{.State.Pid}}", this.container]).toString().trim());
		return pid > 0 ? pid : null;
	}

	async startNodes() {
		const env = {
			...process.env,
			RIVET_ENDPOINT: `http://default:${this.token}@127.0.0.1:6420`,
			RIVET_POOL: "default",
			OPENAI_API_KEY: "bench",
			OPENAI_BASE_URL: this.mock.baseUrl,
			NODE_ENV: "production",
		};
		if (this.sleepTimeoutMs != null) env.BENCH_SLEEP_TIMEOUT_MS = String(this.sleepTimeoutMs);
		this.nodes = [];
		for (let i = 0; i < this.nodeCount; i++) {
			const log = openSync(join(this.dir, `node-${i}.log`), "a");
			// The flags of Rivet's own benchmark node (rivet-node.service), minus the tsx loader: the app is compiled.
			const child = spawn(this.ctx.nodeBin, ["--max-old-space-size=16384", "dist/server.js"], {
				cwd: this.ctx.rivetApp,
				env,
				stdio: ["ignore", log, log],
			});
			this.nodes.push(child);
		}
		await waitUntil(async () => (await this.connectedNodes()) >= this.nodeCount, 60_000, "Rivet nodes to connect", 50);
	}

	async connectedNodes() {
		const result = await httpOk(`${this.origin}/pools/default/nodes?namespace=default`, { headers: { authorization: `Bearer ${this.token}` } });
		if (!result.ok) return 0;
		return JSON.parse(result.body).nodes.filter((n) => n.connected && !n.stopping).length;
	}

	roots() {
		return [this.enginePid(), ...this.nodes.map((n) => n.pid)].filter(Boolean);
	}

	rootNames() {
		return ["engine", ...this.nodes.map((_, i) => `node ${i}`)];
	}

	query(key) {
		return new URLSearchParams({ "rvt-namespace": "default", "rvt-method": "getOrCreate", "rvt-pool": "default", "rvt-key": key, "rvt-token": this.token }).toString();
	}

	turnSpec(key, text) {
		return {
			origin: this.origin,
			path: `/gateway/agent/request/chat?${this.query(key)}`,
			headers: {},
			body: { text },
			doneMarker: "event: done",
		};
	}

	async sleepActor(key) {
		return httpOk(`${this.origin}/gateway/agent/request/sleep?${this.query(key)}`);
	}

	async historyCount(key) {
		const result = await httpOk(`${this.origin}/gateway/agent/request/history?${this.query(key)}`);
		return result.ok ? JSON.parse(result.body).messages : null;
	}

	diskBytes() {
		const out = execFileSync("docker", [
			"run", "--rm", "--entrypoint", "/bin/sh",
			"-v", `${this.volumes[0]}:/a:ro`, "-v", `${this.volumes[1]}:/b:ro`,
			this.ctx.rivetImage, "-c", "du -sb /a /b",
		]).toString();
		return out.trim().split("\n").reduce((sum, line) => sum + Number(line.split(/\s+/)[0]), 0);
	}

	async kill9() {
		for (const node of this.nodes) signal(node.pid, "SIGKILL");
		execFileSync("docker", ["kill", "-s", "KILL", this.container], { stdio: "ignore" });
		await waitUntil(() => !this.nodes.some((n) => alive(n.pid)), 10_000, "Rivet nodes to die", 5);
	}

	async restart() {
		execFileSync("docker", ["start", this.container], { stdio: "ignore" });
		await this.waitEngine();
		await this.startNodes();
	}

	async stop() {
		await terminate(this.nodes.map((n) => n.pid).filter(alive), 15_000);
		try {
			execFileSync("docker", ["rm", "-f", this.container], { stdio: "ignore" });
		} catch {}
		for (const volume of this.volumes) {
			try {
				execFileSync("docker", ["volume", "rm", "-f", volume], { stdio: "ignore" });
			} catch {}
		}
	}
}
