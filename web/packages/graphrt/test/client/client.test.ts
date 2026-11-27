/**
 * The client against a real `graphrt serve` (see test-support/server.ts).
 */
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { API_KEY, startServer, type TestServer } from "../../../../test-support/server.ts";
import { GraphrtClient, GraphrtError, isTerminal, type RunEvent } from "../../src/client/index.ts";

const ORIGIN = "http://localhost:5173";

let server: TestServer;
let client: GraphrtClient;

before(async () => {
  server = await startServer([
    "--conversations",
    "--chat",
    "support=message",
    "--chat-access",
    "guests",
    "--user-resume",
    "--cors-origin",
    ORIGIN,
  ]);
  client = new GraphrtClient({ baseUrl: server.url, token: API_KEY });
});

after(async () => {
  await server.stop();
});

async function collect(events: AsyncIterable<RunEvent>): Promise<RunEvent[]> {
  const all: RunEvent[] = [];
  for await (const event of events) {
    all.push(event);
  }
  return all;
}

function answerOf(events: RunEvent[]): string {
  return events.map((event) => (event.type === "response" ? event.data.text : "")).join("");
}

function ids(events: RunEvent[]): string[] {
  return events.flatMap((event) => ("id" in event && event.id !== undefined ? [event.id] : []));
}

async function guestClient(): Promise<GraphrtClient> {
  const session = await new GraphrtClient({ baseUrl: server.url }).guestSession();
  return new GraphrtClient({ baseUrl: server.url, token: session.token });
}

describe("runs", () => {
  test("a blocking run returns its answer and outputs", async () => {
    const result = await client.run("support", { inputs: { message: "hello" } });
    assert.equal(result.status, "succeeded");
    assert.equal(result.answer, "You said hello");
    assert.equal(result.outputs?.echoed, "hello");
    assert.match(result.run_id, /^run-/);
  });

  test("a streamed run yields its answer text and ends with the terminal event", async () => {
    const events = await collect(client.stream("support", { inputs: { message: "streamed" } }));
    assert.equal(events[0]?.type, "run_started");
    assert.equal(answerOf(events), "You said streamed");
    const last = events.at(-1);
    assert.equal(last?.type, "run_succeeded");
    assert.ok(last && isTerminal(last));
    assert.equal(last.type === "run_succeeded" ? last.data.outputs?.echoed : undefined, "streamed");
    // Engine events come through with their name.
    assert.ok(events.some((event) => event.type === "engine" && event.name === "node_succeeded"));
  });

  test("an async run is followed by its events, with the answer text", async () => {
    const accepted = await client.startAsync("support", { inputs: { message: "later" } });
    assert.equal(accepted.status, "running");
    const events = await collect(client.events(accepted.run_id));
    assert.equal(events.at(-1)?.type, "run_succeeded");
    assert.equal(answerOf(events), "You said later");
    const record = await client.getRun(accepted.run_id);
    assert.equal(record.status, "succeeded");
    assert.equal(record.answer, "You said later");
  });

  test("events can be read without the answer text, and from an index", async () => {
    const { run_id } = await client.run("support", { inputs: { message: "x" } });
    const plain = await collect(client.events(run_id, { responses: false }));
    assert.ok(plain.every((event) => event.type !== "response"));
    const rest = await collect(client.events(run_id, { responses: false, from: 2 }));
    assert.deepEqual(ids(rest), ids(plain).slice(2));
  });

  test("a broken connection is reopened and every event arrives exactly once", async () => {
    const { run_id } = await client.run("support", { inputs: { message: "x" } });
    const whole = await collect(client.events(run_id, { responses: false }));
    assert.ok(whole.length >= 4, "the run has enough events to cut");
    // A network that drops every response after two events.
    let requests = 0;
    const flaky: typeof fetch = async (input, init) => {
      requests += 1;
      const response = await fetch(input, init);
      // Each server event ends with a blank line.
      const events = (await response.text()).split("\n\n").filter((event) => event !== "");
      let sent = 0;
      const cut = new ReadableStream<Uint8Array>({
        pull(controller) {
          const next = events[sent];
          if (sent >= 2 || next === undefined) {
            controller.error(new TypeError("network error"));
            return;
          }
          sent += 1;
          controller.enqueue(new TextEncoder().encode(`${next}\n\n`));
        },
      });
      return new Response(cut, { status: response.status, headers: response.headers });
    };
    const unlucky = new GraphrtClient({ baseUrl: server.url, token: API_KEY, fetch: flaky });
    const pieced = await collect(unlucky.events(run_id, { responses: false, retryDelayMs: 1 }));
    assert.deepEqual(ids(pieced), ids(whole));
    assert.ok(requests > 1, "the stream was reopened");
  });

  test("following gives up after the retries and says why", async () => {
    const dead: typeof fetch = () => Promise.reject(new TypeError("fetch failed"));
    const offline = new GraphrtClient({ baseUrl: server.url, token: API_KEY, fetch: dead });
    await assert.rejects(collect(offline.events("run-1", { retries: 2, retryDelayMs: 1 })), /fetch failed/);
  });

  test("a running run can be stopped", async () => {
    const accepted = await client.startAsync("slow");
    const events: RunEvent[] = [];
    for await (const event of client.events(accepted.run_id)) {
      events.push(event);
      if (event.type === "response" && event.data.text === "working") {
        assert.equal(await client.abort(accepted.run_id), true);
      }
    }
    assert.equal(events.at(-1)?.type, "run_aborted");
    assert.equal(answerOf(events), "working");
    assert.equal(await client.abort(accepted.run_id), false, "it no longer executes");
    assert.equal((await client.getRun(accepted.run_id)).status, "aborted");
  });

  test("closing a stream aborts its run", async () => {
    const controller = new AbortController();
    let runId = "";
    await assert.rejects(async () => {
      for await (const event of client.stream("slow", {}, { signal: controller.signal })) {
        if (event.type === "run_started") {
          runId = event.data.run_id;
        }
        if (event.type === "response") {
          controller.abort();
        }
      }
    });
    assert.notEqual(runId, "");
    // The server notices the closed connection a moment later.
    for (let attempt = 0; attempt < 50; attempt += 1) {
      if ((await client.getRun(runId)).status === "aborted") {
        return;
      }
      await new Promise((done) => setTimeout(done, 100));
    }
    assert.fail("the run was not aborted");
  });

  test("a paused run says what it waits for and resumes", async () => {
    const paused = await client.run("confirm");
    assert.equal(paused.status, "paused");
    assert.equal(paused.reasons?.[0]?.kind, "human_input");
    const record = await client.getRun(paused.run_id);
    assert.equal(record.resume_inputs?.[0]?.key, "ask");
    const resumed = await client.resume(paused.run_id, {
      inputs: { ask: { action: "yes", values: { note: "ok" } } },
    });
    assert.equal(resumed.status, "succeeded");
    assert.equal(resumed.answer, "sent (ok)");
    // A second resume of the same pause is refused.
    await assert.rejects(
      client.resume(paused.run_id, { inputs: { ask: { action: "yes" } } }),
      (error: unknown) => error instanceof GraphrtError && error.status === 409,
    );
  });

  test("a resume can be streamed or started in the background", async () => {
    const first = await client.run("confirm");
    const events = await collect(client.resumeStream(first.run_id, { inputs: { ask: { action: "no" } } }));
    assert.equal(answerOf(events), "not sent");
    assert.equal(events.at(-1)?.type, "run_succeeded");

    const second = await client.run("confirm");
    const accepted = await client.resumeAsync(second.run_id, { inputs: { ask: { action: "no" } } });
    // The events before `first_event` are the first execution's, which ended with the pause.
    assert.ok((accepted.first_event ?? 0) > 0);
    const followed = await collect(client.events(accepted.run_id, { from: accepted.first_event }));
    assert.equal(followed[0]?.type, "run_started");
    assert.equal(followed.at(-1)?.type, "run_succeeded");
    assert.equal((await client.getRun(second.run_id)).answer, "not sent");
  });

  test("runs are listed newest first, by workflow and status", async () => {
    const { run_id } = await client.run("support", { inputs: { message: "listed" } });
    const page = await client.listRuns({ workflow: "support", status: "succeeded", limit: 5 });
    assert.equal(page.runs[0]?.run_id, run_id);
    assert.ok(page.runs.every((run) => run.workflow === "support" && run.status === "succeeded"));
  });

  test("a run can be rated", async () => {
    const { run_id } = await client.run("support", { inputs: { message: "rate me" } });
    const score = await client.feedback(run_id, { name: "helpful", value: true, comment: "thanks" });
    assert.equal(score.name, "helpful");
    assert.equal(score.value, true);
    assert.equal(score.source, "human");
  });
});

describe("errors", () => {
  test("a refused request carries the server's message and status", async () => {
    const anonymous = new GraphrtClient({ baseUrl: server.url });
    await assert.rejects(anonymous.run("support", { inputs: { message: "x" } }), (error: unknown) => {
      assert.ok(error instanceof GraphrtError);
      assert.equal(error.status, 401);
      assert.match(error.message, /bearer token/);
      return true;
    });
    await assert.rejects(client.run("nope"), (error: unknown) => {
      assert.ok(error instanceof GraphrtError);
      assert.equal(error.status, 404);
      assert.match(error.message, /unknown workflow `nope`/);
      return true;
    });
    await assert.rejects(collect(client.stream("nope")), (error: unknown) => {
      return error instanceof GraphrtError && error.status === 404;
    });
    await assert.rejects(collect(client.events("run-unknown", { retries: 0 })), GraphrtError);
  });

  test("an answer of another shape is an error, not a wrong value", async () => {
    const lying: typeof fetch = async () => Response.json({ run_id: 7, status: "done" });
    const confused = new GraphrtClient({ baseUrl: server.url, fetch: lying });
    await assert.rejects(confused.run("support"), /unexpected answer .* at run_id/);
  });

  test("a token function is asked for every request", async () => {
    let asked = 0;
    const rotating = new GraphrtClient({
      baseUrl: `${server.url}/`,
      token: () => {
        asked += 1;
        return Promise.resolve(API_KEY);
      },
    });
    await rotating.listRuns({ limit: 1 });
    await rotating.listRuns({ limit: 1 });
    assert.equal(asked, 2);
  });
});

describe("conversations", () => {
  test("the server keeps the history between runs", async () => {
    const conversation = await client.conversations.create("memo");
    const first = await client.run("memo", { conversation_id: conversation.id, sys: { query: "one" } });
    assert.equal(first.answer, "one after []");
    const second = await client.run("memo", { conversation_id: conversation.id, sys: { query: "two" } });
    assert.equal(
      second.answer,
      'two after [{"role":"user","content":"one"},{"role":"assistant","content":"one after []"}]',
    );
    const stored = await client.conversations.get(conversation.id);
    assert.equal(stored.title, "one");
    assert.deepEqual(
      stored.messages.map((message) => [message.role, message.content.slice(0, 3)]),
      [
        ["user", "one"],
        ["assistant", "one"],
        ["user", "two"],
        ["assistant", "two"],
      ],
    );
    const listed = await client.conversations.list({ workflow: "memo" });
    assert.ok(listed.conversations.some((item) => item.id === conversation.id && item.message_count === 4));
    await client.conversations.delete(conversation.id);
    await assert.rejects(
      client.conversations.get(conversation.id),
      (error: unknown) => error instanceof GraphrtError && error.status === 404,
    );
  });

  test("a streamed turn is in the history as soon as its stream ends", async () => {
    const conversation = await client.conversations.create("memo");
    await collect(client.stream("memo", { conversation_id: conversation.id, sys: { query: "first" } }));
    const next = await client.run("memo", { conversation_id: conversation.id, sys: { query: "second" } });
    assert.match(next.answer ?? "", /"content":"first"/);
  });
});

describe("the chat page and guests", () => {
  test("the config says who may chat with what", async () => {
    const config = await new GraphrtClient({ baseUrl: server.url }).chatConfig();
    assert.equal(config.access, "guests");
    assert.deepEqual(config.workflows, [{ name: "support", input: "message" }]);
    assert.equal(config.conversations, true);
    assert.equal(config.resume, true);
  });

  test("a guest chats, follows their run and keeps a conversation", async () => {
    const guest = await guestClient();
    const accepted = await guest.startAsync("support", { inputs: { message: "hi" } });
    const events = await collect(guest.events(accepted.run_id));
    assert.equal(answerOf(events), "You said hi");
    // A guest sees the answer and the run's own events, nothing of the nodes.
    assert.ok(events.every((event) => event.type !== "engine"));
    const record = await guest.getRun(accepted.run_id);
    assert.equal(record.answer, "You said hi");
    assert.equal(record.user_id, undefined);
    const conversation = await guest.conversations.create("support", { input: "message" });
    await guest.run("support", { conversation_id: conversation.id, inputs: { message: "remember me" } });
    assert.equal((await guest.conversations.get(conversation.id)).messages.length, 2);
  });

  test("a guest reaches only the offered workflow and their own runs", async () => {
    const guest = await guestClient();
    const other = await guestClient();
    const { run_id } = await guest.run("support", { inputs: { message: "mine" } });
    const refused = (status: number) => (error: unknown) =>
      error instanceof GraphrtError && error.status === status;
    await assert.rejects(other.getRun(run_id), refused(404));
    await assert.rejects(collect(other.events(run_id, { retries: 0 })), refused(404));
    await assert.rejects(guest.run("memo", { sys: { query: "x" } }), refused(403));
    await assert.rejects(guest.listRuns(), refused(401));
  });

  test("a renewed guest session is the same guest", async () => {
    const anonymous = new GraphrtClient({ baseUrl: server.url });
    const first = await anonymous.guestSession();
    const guest = new GraphrtClient({ baseUrl: server.url, token: first.token });
    const { run_id } = await guest.run("support", { inputs: { message: "x" } });
    const renewed = await anonymous.guestSession(first.token);
    assert.notEqual(renewed.token, "");
    const again = new GraphrtClient({ baseUrl: server.url, token: renewed.token });
    assert.equal((await again.getRun(run_id)).run_id, run_id);
  });
});

describe("cross-origin requests", () => {
  test("a listed origin passes the preflight and reads the answer", async () => {
    const preflight = await fetch(`${server.url}/v1/workflows/support/runs`, {
      method: "OPTIONS",
      headers: {
        Origin: ORIGIN,
        "Access-Control-Request-Method": "POST",
        "Access-Control-Request-Headers": "authorization, content-type",
      },
    });
    assert.equal(preflight.status, 204);
    assert.equal(preflight.headers.get("access-control-allow-origin"), ORIGIN);
    assert.match(preflight.headers.get("access-control-allow-headers") ?? "", /authorization/);
    assert.equal(preflight.headers.get("access-control-allow-credentials"), null);

    // The refusal of an unauthenticated request is readable too.
    const refused = await fetch(`${server.url}/v1/runs`, { headers: { Origin: ORIGIN } });
    assert.equal(refused.status, 401);
    assert.equal(refused.headers.get("access-control-allow-origin"), ORIGIN);
  });

  test("an event stream carries the headers too", async () => {
    const response = await fetch(`${server.url}/v1/workflows/support/runs`, {
      method: "POST",
      headers: { Origin: ORIGIN, Authorization: `Bearer ${API_KEY}`, "Content-Type": "application/json" },
      body: JSON.stringify({ inputs: { message: "x" }, stream: true }),
    });
    assert.equal(response.headers.get("content-type"), "text/event-stream");
    assert.equal(response.headers.get("access-control-allow-origin"), ORIGIN);
    assert.equal(response.headers.get("vary"), "origin");
    await response.body?.cancel();
  });

  test("another origin gets no headers and its preflight is refused", async () => {
    const preflight = await fetch(`${server.url}/v1/workflows/support/runs`, {
      method: "OPTIONS",
      headers: { Origin: "https://evil.example", "Access-Control-Request-Method": "POST" },
    });
    assert.equal(preflight.status, 403);
    assert.equal(preflight.headers.get("access-control-allow-origin"), null);
    const response = await fetch(`${server.url}/healthz`, { headers: { Origin: "https://evil.example" } });
    assert.equal(response.headers.get("access-control-allow-origin"), null);
  });
});
