/**
 * The chat session against a real `graphrt serve` (see test-support/server.ts).
 */
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { API_KEY, startServer, type TestServer } from "../../../../test-support/server.ts";
import { checkChatContext, MAX_CONTEXT_BYTES } from "../../src/chat/context.ts";
import type { SessionEvent } from "../../src/chat/events.ts";
import { ChatSession, type ChatState, type KeyValueStore, questionBlock } from "../../src/chat/session.ts";

function memoryStorage(): KeyValueStore & { entries: Map<string, string> } {
  const entries = new Map<string, string>();
  return {
    entries,
    getItem: (key) => entries.get(key) ?? null,
    setItem: (key, value) => void entries.set(key, value),
    removeItem: (key) => void entries.delete(key),
  };
}

function last(state: ChatState) {
  const message = state.messages.at(-1);
  assert.ok(message, "there is a message");
  return message;
}

describe("a public chat with conversations", () => {
  let server: TestServer;
  before(async () => {
    server = await startServer([
      "--conversations",
      "--chat",
      "support=message",
      "--chat",
      "confirm",
      "--chat",
      "report",
      "--chat",
      "slow",
      "--chat-access",
      "guests",
      "--user-resume",
    ]);
  });
  after(() => server.stop());

  test("a guest sends a message and the answer streams in", async () => {
    const session = new ChatSession({ baseUrl: server.url, storage: memoryStorage() });
    const states: ChatState[] = [];
    session.subscribe((state) => states.push(state));
    await session.start();
    assert.equal(session.state.status, "ready");
    assert.equal(session.state.title, "support");
    await session.send("  hello  ");
    const [question, answer] = session.state.messages;
    assert.equal(question?.text, "hello");
    assert.equal(answer?.text, "You said hello");
    assert.equal(answer?.state, "done");
    assert.match(answer?.runId ?? "", /^run-/);
    assert.equal(session.state.busy, false);
    // The view saw that an answer was on its way.
    assert.ok(states.some((state) => state.busy && state.messages.at(-1)?.state === "streaming"));
  });

  test("the conversation survives a reload, on the server", async () => {
    const storage = memoryStorage();
    const first = new ChatSession({ baseUrl: server.url, storage });
    await first.start();
    await first.send("remember this");
    // A new session with the same storage: the same guest and conversation.
    const second = new ChatSession({ baseUrl: server.url, storage });
    await second.start();
    assert.deepEqual(
      second.state.messages.map((message) => [message.role, message.text]),
      [
        ["user", "remember this"],
        ["assistant", "You said remember this"],
      ],
    );
    // Another visitor starts empty and never sees it.
    const stranger = new ChatSession({ baseUrl: server.url, storage: memoryStorage() });
    await stranger.start();
    assert.equal(stranger.state.messages.length, 0);
    // "New chat" starts over for the same guest.
    second.reset();
    assert.equal(second.state.messages.length, 0);
    const third = new ChatSession({ baseUrl: server.url, storage });
    await third.start();
    assert.equal(third.state.messages.length, 0);
  });

  test("UI blocks of the outputs come with the answer", async () => {
    const session = new ChatSession({ baseUrl: server.url, workflow: "report" });
    await session.start();
    await session.send("numbers please");
    const answer = last(session.state);
    assert.equal(answer.text, "Here are the numbers.");
    assert.equal(answer.blocks.length, 1);
    assert.equal((answer.blocks[0] as { $ui: string }).$ui, "stats");
  });

  test("a paused run asks its question and goes on with the answer", async () => {
    const session = new ChatSession({ baseUrl: server.url, workflow: "confirm" });
    await session.start();
    await session.send("send it");
    const waiting = last(session.state);
    assert.equal(waiting.state, "waiting");
    assert.equal(waiting.questions.length, 1);
    const question = waiting.questions[0];
    assert.ok(question);
    assert.equal(question.key, "ask");
    const { card } = question.block as { card: { body: { type: string }[]; actions: { id: string }[] } };
    assert.deepEqual(
      card.body.map((element) => element.type),
      ["TextBlock", "Input.Text"],
    );
    assert.deepEqual(
      card.actions.map((action) => action.id),
      ["yes", "no"],
    );
    await session.answer(waiting.id, "ask", "yes", { note: "go" });
    const done = last(session.state);
    assert.equal(done.state, "done");
    assert.equal(done.text, "sent (go)");
    assert.equal(done.questions.length, 0);
  });

  test("an answer can be stopped", async () => {
    const session = new ChatSession({ baseUrl: server.url, workflow: "slow" });
    await session.start();
    session.subscribe((state) => {
      if (state.busy && last(state).text === "working") {
        void session.stop();
      }
    });
    await session.send("go");
    assert.equal(last(session.state).state, "stopped");
    assert.equal(last(session.state).text, "working");
    assert.equal(session.state.busy, false);
  });

  test("an answer can be rated", async () => {
    const session = new ChatSession({ baseUrl: server.url });
    await session.start();
    await session.send("rate me");
    const answer = last(session.state);
    await session.rate(answer.id, "up");
    assert.equal(last(session.state).rating, "up");
  });

  test("a workflow the server does not offer is not available", async () => {
    const session = new ChatSession({ baseUrl: server.url, workflow: "memo" });
    await session.start();
    assert.equal(session.state.status, "unavailable");
    await session.send("hello");
    assert.equal(session.state.messages.length, 0);
  });

  test("a failed request is shown on the answer, in words", async () => {
    let fail = false;
    const flaky: typeof fetch = (input, init) => {
      if (fail && String(input).includes("/runs")) {
        return Promise.resolve(
          Response.json({ error: "busy" }, { status: 429, headers: { "Retry-After": "7" } }),
        );
      }
      return fetch(input, init);
    };
    const session = new ChatSession({ baseUrl: server.url, fetch: flaky });
    await session.start();
    fail = true;
    await session.send("hello");
    const answer = last(session.state);
    assert.equal(answer.state, "failed");
    assert.equal(answer.error, "Too many messages. Try again in 7 seconds.");
    assert.equal(session.state.busy, false);
  });
});

describe("a chat for holders of the key, without conversations", () => {
  let server: TestServer;
  before(async () => {
    server = await startServer(["--chat", "memo"]);
  });
  after(() => server.stop());

  test("the chat is locked until the right key is entered", async () => {
    const storage = memoryStorage();
    const session = new ChatSession({ baseUrl: server.url, storage });
    await session.start();
    assert.equal(session.state.status, "locked");
    assert.equal(await session.unlock("wrong-key"), false);
    assert.equal(session.state.status, "locked");
    assert.equal(session.state.notice, "This key was not accepted.");
    assert.equal(await session.unlock(API_KEY, true), true);
    assert.equal(session.state.status, "ready");
    // Remembered on this device only when asked.
    const again = new ChatSession({ baseUrl: server.url, storage });
    await again.start();
    assert.equal(again.state.status, "ready");
    again.lock();
    assert.equal(again.state.status, "locked");
    assert.equal(
      [...storage.entries.keys()].some((key) => key.endsWith(":key")),
      false,
    );
  });

  test("without conversations on the server, the history goes with each message", async () => {
    const session = new ChatSession({ baseUrl: server.url, apiKey: API_KEY });
    await session.start();
    assert.equal(session.state.status, "ready");
    await session.send("one");
    assert.equal(last(session.state).text, "one after []");
    await session.send("two");
    assert.equal(
      last(session.state).text,
      'two after [{"role":"user","content":"one"},{"role":"assistant","content":"one after []"}]',
    );
  });
});

describe("servers that cannot chat", () => {
  test("a server without a chat page says so", async () => {
    const server = await startServer();
    try {
      const session = new ChatSession({ baseUrl: server.url });
      await session.start();
      assert.equal(session.state.status, "unavailable");
      assert.equal(session.state.notice, "This server has no chat.");
    } finally {
      await server.stop();
    }
  });

  test("a server that cannot be reached says so", async () => {
    const session = new ChatSession({ baseUrl: "http://127.0.0.1:9" });
    await session.start();
    assert.equal(session.state.status, "unavailable");
    assert.ok(session.state.notice);
  });
});

describe("questions of paused runs", () => {
  test("a node's own card is shown as it is", () => {
    const card = { $ui: "adaptive_card", version: 1, card: { type: "AdaptiveCard", version: "1.5" } };
    assert.deepEqual(questionBlock({ kind: "human_input", data: { card, prompt: "ignored" } }), card);
  });

  test("a form becomes a card whose actions are the node's actions", () => {
    const block = questionBlock({
      kind: "human_input",
      data: {
        // biome-ignore lint/suspicious/noTemplateCurlyInString: the text a card must not evaluate
        prompt: "Pay ${amount}?",
        fields: [
          { name: "amount", type: "number", required: true, default: 5 },
          { name: "note", label: "Note", type: "paragraph" },
          { name: "method", options: ["card", { value: "cash", label: "Cash" }] },
          { name: "agree", type: "boolean" },
          { broken: true },
        ],
        actions: [],
      },
    }) as { card: { body: Record<string, unknown>[]; actions: Record<string, unknown>[] } };
    // A prompt cannot smuggle a template expression into the card.
    assert.equal(block.card.body[0]?.text, "Pay $ {amount}?");
    assert.deepEqual(
      block.card.body.slice(1).map((input) => [input.type, input.id]),
      [
        ["Input.Number", "amount"],
        ["Input.Text", "note"],
        ["Input.ChoiceSet", "method"],
        ["Input.Toggle", "agree"],
      ],
    );
    assert.equal(block.card.body[1]?.isRequired, true);
    assert.equal(block.card.body[2]?.isMultiline, true);
    assert.deepEqual(block.card.body[3]?.choices, [
      { title: "card", value: "card" },
      { title: "Cash", value: "cash" },
    ]);
    // Without actions there is still a way to answer.
    assert.deepEqual(block.card.actions, [{ type: "Action.Submit", id: "submit", title: "Send" }]);
  });

  test("other pauses are not questions to the person chatting", () => {
    assert.equal(questionBlock({ kind: "tool_approval", data: { tool: "refund" } }), undefined);
    assert.equal(questionBlock({ kind: "human_input", data: "text" }), undefined);
  });
});

describe("events and the page context", () => {
  let server: TestServer;
  before(async () => {
    server = await startServer([
      "--chat",
      "shop",
      "--chat",
      "confirm",
      "--chat",
      "report",
      "--chat-access",
      "guests",
      "--user-resume",
    ]);
  });
  after(() => server.stop());

  function record(session: ChatSession): SessionEvent[] {
    const events: SessionEvent[] = [];
    session.on((event) => events.push(event));
    return events;
  }

  test("ready, message and answer come in order, with the answer's text and blocks", async () => {
    const session = new ChatSession({ baseUrl: server.url, workflow: "report" });
    const events = record(session);
    await session.start();
    await session.send("numbers please");
    assert.deepEqual(
      events.map((event) => event.type),
      ["ready", "message", "answer"],
    );
    const [ready, message, answer] = events;
    // The title is the server's (`--chat-title`, else its first chat workflow).
    assert.deepEqual(ready?.detail, { workflow: "report", title: "shop" });
    assert.deepEqual(message?.detail, { id: "m1", text: "numbers please" });
    assert.equal(answer?.type, "answer");
    if (answer?.type === "answer") {
      assert.equal(answer.detail.text, "Here are the numbers.");
      assert.match(answer.detail.runId ?? "", /^run-/);
      assert.equal((answer.detail.blocks[0] as { $ui: string }).$ui, "stats");
    }
  });

  test("the page context reaches the workflow as sys.context", async () => {
    const context = { product_id: "sku-1", page: "https://shop.example/p/sku-1", name: undefined };
    const session = new ChatSession({ baseUrl: server.url, workflow: "shop", context });
    await session.start();
    await session.send("Is it in stock?");
    assert.equal(last(session.state).text, "Is it in stock? about sku-1");
    // A change of the caller's object after the call does not change what is sent.
    const next = { product_id: "sku-2" };
    session.setContext(next);
    next.product_id = "changed";
    await session.send("And this one?");
    assert.equal(last(session.state).text, "And this one? about sku-2");
    assert.deepEqual(session.context, { product_id: "sku-2" });
    session.setContext(undefined);
    await session.send("Nothing?");
    assert.equal(last(session.state).text, "Nothing? about ");
  });

  test("a context out of the limits is refused and the one before stays", () => {
    const session = new ChatSession({ baseUrl: server.url, context: { product_id: "sku-1" } });
    const refuse = (value: unknown, reason: RegExp): void => {
      assert.throws(
        () => session.setContext(value as never),
        (error: unknown) => {
          assert.ok(error instanceof TypeError);
          assert.match(error.message, reason);
          return true;
        },
      );
    };
    refuse("sku-1", /must be a JSON object/);
    refuse(["sku-1"], /must be a JSON object/);
    refuse(null, /must be a JSON object/);
    refuse({ when: new Date() }, /context\.when is not a plain object/);
    refuse({ total: Number.NaN }, /context\.total is not a finite number/);
    refuse({ handler: () => undefined }, /context\.handler is function, not JSON/);
    refuse({ ids: [1, undefined] }, /context\.ids\[1\] is undefined, not JSON/);
    refuse({ a: { b: { c: { d: { e: { f: "six levels" } } } } } }, /nested deeper than 5/);
    refuse({ notes: "x".repeat(MAX_CONTEXT_BYTES) }, /more than 8192/);
    assert.deepEqual(session.context, { product_id: "sku-1" });
    // The same limits as the server: five levels and 8192 bytes fit.
    assert.doesNotThrow(() => checkChatContext({ a: { b: { c: { d: { e: "five levels" } } } } }));
    assert.doesNotThrow(() => checkChatContext({ a: [[[["five levels"]]]] }));
    const fits = { text: "x".repeat(MAX_CONTEXT_BYTES - 11) };
    assert.equal(JSON.stringify(fits).length, MAX_CONTEXT_BYTES);
    assert.doesNotThrow(() => checkChatContext(fits));
    assert.throws(() => checkChatContext({ text: "é".repeat(MAX_CONTEXT_BYTES / 2) }), /more than 8192/);
    assert.throws(() => new ChatSession({ context: { bad: Symbol("x") } as never }), TypeError);
  });

  test("a question to the visitor is a handoff to them", async () => {
    const session = new ChatSession({ baseUrl: server.url, workflow: "confirm" });
    const events = record(session);
    await session.start();
    await session.send("send it");
    const handoff = events.at(-1);
    assert.equal(handoff?.type, "handoff");
    if (handoff?.type === "handoff") {
      assert.equal(handoff.detail.waitingFor, "visitor");
      assert.equal(handoff.detail.id, last(session.state).id);
    }
    await session.answer(last(session.state).id, "ask", "no", { note: "" });
    assert.deepEqual(
      events.map((event) => event.type),
      ["ready", "message", "handoff", "answer"],
    );
  });

  test("a failed answer and a chat that cannot connect are errors", async () => {
    let fail = false;
    const flaky: typeof fetch = (input, init) =>
      fail && String(input).includes("/runs")
        ? Promise.resolve(Response.json({ error: "down" }, { status: 503 }))
        : fetch(input, init);
    const session = new ChatSession({ baseUrl: server.url, workflow: "shop", fetch: flaky });
    const events = record(session);
    await session.start();
    fail = true;
    await session.send("hello");
    assert.deepEqual(
      events.map((event) => event.type),
      ["ready", "message", "error"],
    );
    assert.deepEqual(events.at(-1)?.detail, { during: "answer", message: "down", id: "m2" });

    const missing = new ChatSession({ baseUrl: server.url, workflow: "memo" });
    const missed = record(missing);
    await missing.start();
    assert.deepEqual(missed, [
      { type: "error", detail: { during: "connect", message: "This chat is not available." } },
    ]);
  });

  test("a listener that throws does not break the chat", async () => {
    const session = new ChatSession({ baseUrl: server.url, workflow: "shop" });
    const seen: string[] = [];
    const thrown: unknown[] = [];
    // What a browser has: errors of listeners are reported like uncaught ones.
    const holder = globalThis as { reportError?: (error: unknown) => void };
    holder.reportError = (error) => void thrown.push(error);
    try {
      session.on(() => {
        throw new Error("a bug of the page");
      });
      session.on((event) => seen.push(event.type));
      await session.start();
      await session.send("still works");
    } finally {
      delete holder.reportError;
    }
    assert.deepEqual(seen, ["ready", "message", "answer"]);
    assert.equal(thrown.length, 3, "each error is reported, not swallowed");
    assert.equal(last(session.state).state, "done");
  });
});
