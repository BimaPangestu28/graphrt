// The Rivet side of the graphrt vs Rivet benchmark: the agent from the RivetKit
// README (SQLite history, `streamText`, tokens broadcast to connected clients),
// pointed at the shared mock model, plus an `onRequest` route that streams the
// same answer as server-sent events so both runtimes can be driven by the same
// HTTP client.
//
// Functionally equal to bench/vs-rivet/graphrt/agent.yaml: receive a message,
// call the model with the system prompt + the last 20 history messages + the
// message, stream the answer back, keep the history.
import { createOpenAI } from "@ai-sdk/openai";
import { streamText } from "ai";
import { actor, setup } from "rivetkit";
import { db } from "rivetkit/db";

const SYSTEM_PROMPT = "You are a concise, friendly assistant.";
// graphrt's `memory.window.size: 10` keeps 10 turns = 20 messages.
const HISTORY_MESSAGES = 20;

const openai = createOpenAI({
	baseURL: process.env.OPENAI_BASE_URL,
	apiKey: process.env.OPENAI_API_KEY ?? "bench",
});
// Chat Completions, the API the mock (and graphrt's `openai` provider) speak.
const model = openai.chat("gpt-4o-mini");

// Production default is 30 s; the harness lowers it only where it waits for
// actors to fall asleep on their own.
const sleepTimeout = Number(process.env.BENCH_SLEEP_TIMEOUT_MS ?? 30_000);

type Message = { role: "user" | "assistant"; content: string };

interface Db {
	execute(sql: string, ...args: unknown[]): Promise<unknown>;
}

async function remember(database: Db, text: string): Promise<Message[]> {
	await database.execute("INSERT INTO messages (role, content) VALUES (?, ?)", "user", text);
	const rows = (await database.execute(
		"SELECT role, content FROM (SELECT id, role, content FROM messages ORDER BY id DESC LIMIT ?) ORDER BY id",
		HISTORY_MESSAGES + 1,
	)) as Message[];
	return rows.map((row) => ({ role: row.role, content: row.content }));
}

const encoder = new TextEncoder();

export const agent = actor({
	options: { sleepTimeout },
	// Reset on every start of the actor: lets the harness check that a "wake"
	// sample really hit a freshly started (woken) actor.
	createVars: () => ({ served: 0 }),
	db: db({
		onMigrate: async (database) => {
			await database.execute(
				"CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, role TEXT NOT NULL, content TEXT NOT NULL)",
			);
		},
	}),

	actions: {
		// The README's `sendMessage`: tokens go to every connected client as `token` events.
		sendMessage: async (c, text: string) => {
			const messages = await remember(c.db, text);
			const response = streamText({ model, system: SYSTEM_PROMPT, messages });
			for await (const delta of response.textStream) {
				c.broadcast("token", delta);
			}
			const answer = await response.text;
			await c.db.execute("INSERT INTO messages (role, content) VALUES (?, ?)", "assistant", answer);
			return answer.length;
		},
		history: async (c) => {
			const rows = (await c.db.execute("SELECT count(*) AS n FROM messages")) as { n: number }[];
			return rows[0]?.n ?? 0;
		},
		sleep: (c) => {
			c.sleep();
			return true;
		},
	},

	// The same turn over plain HTTP: POST /chat {"text": "..."} answers SSE
	// (`event: token` per delta, then `event: done`).
	onRequest: async (c, request) => {
		const path = new URL(request.url).pathname;
		if (path.endsWith("/sleep")) {
			c.sleep();
			return new Response("sleeping");
		}
		if (path.endsWith("/history")) {
			const rows = (await c.db.execute("SELECT count(*) AS n FROM messages")) as { n: number }[];
			return Response.json({ messages: rows[0]?.n ?? 0 });
		}
		if (!path.endsWith("/chat") || request.method !== "POST") {
			return new Response("not found", { status: 404 });
		}
		const cold = c.vars.served === 0;
		c.vars.served += 1;
		const { text } = (await request.json()) as { text: string };
		const messages = await remember(c.db, text);
		const response = streamText({ model, system: SYSTEM_PROMPT, messages, abortSignal: request.signal });
		const body = new ReadableStream<Uint8Array>({
			async start(controller) {
				try {
					for await (const delta of response.textStream) {
						controller.enqueue(encoder.encode(`event: token\ndata: ${JSON.stringify({ delta })}\n\n`));
					}
					const answer = await response.text;
					await c.db.execute("INSERT INTO messages (role, content) VALUES (?, ?)", "assistant", answer);
					controller.enqueue(encoder.encode("event: done\ndata: {}\n\n"));
					controller.close();
				} catch (error) {
					controller.enqueue(encoder.encode(`event: error\ndata: ${JSON.stringify({ error: String(error) })}\n\n`));
					controller.close();
				}
			},
		});
		return new Response(body, {
			headers: {
				"Content-Type": "text/event-stream; charset=utf-8",
				"Cache-Control": "no-cache, no-transform",
				"X-Bench-Cold": cold ? "1" : "0",
			},
		});
	},
});

export const registry = setup({ use: { agent } });

registry.start();
