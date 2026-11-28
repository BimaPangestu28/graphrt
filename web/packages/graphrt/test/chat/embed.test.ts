/**
 * The embed API: events, methods and options of `<graphrt-chat>`,
 * `<graphrt-chat-widget>` and `window.GraphrtChat`, in a DOM (happy-dom)
 * against a real `graphrt serve`.
 */
import "../../../../test-support/dom.ts";
import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { startServer, type TestServer } from "../../../../test-support/server.ts";
import {
  type ChatEventName,
  defineGraphrtChatWidget,
  type GraphrtChatApi,
  type GraphrtChatElement,
  type GraphrtChatHolder,
  type GraphrtChatWidgetElement,
  mountEmbed,
} from "../../src/chat/index.ts";

let server: TestServer;

before(async () => {
  server = await startServer([
    "--chat",
    "shop",
    "--chat",
    "report",
    "--chat",
    "confirm",
    "--chat-access",
    "guests",
    "--user-resume",
  ]);
  defineGraphrtChatWidget();
});

after(() => server.stop());

async function until(what: string, condition: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (condition()) {
      return;
    }
    await new Promise((done) => setTimeout(done, 25));
  }
  assert.fail(`timed out waiting for ${what}`);
}

const NAMES: ChatEventName[] = ["open", "close", "ready", "message", "answer", "handoff", "error"];

/** Every chat event that reaches `target`, as `[name, detail]`. */
function recordEvents(target: EventTarget): [string, unknown][] {
  const seen: [string, unknown][] = [];
  for (const name of NAMES) {
    target.addEventListener(`graphrt-chat-${name}`, (event) => {
      seen.push([name, (event as CustomEvent).detail]);
    });
  }
  return seen;
}

const names = (seen: [string, unknown][]): string[] => seen.map(([name]) => name);

function newChat(workflow: string): GraphrtChatElement {
  const chat = document.createElement("graphrt-chat") as GraphrtChatElement;
  chat.setAttribute("server", server.url);
  chat.setAttribute("workflow", workflow);
  return chat;
}

/** A script tag as the page has it: `<script src=".../chat/embed.js" data-...>`. */
function scriptTag(data: Record<string, string> = {}): HTMLScriptElement {
  const script = document.createElement("script");
  script.src = `${server.url}/chat/embed.js`;
  for (const [name, value] of Object.entries(data)) {
    script.dataset[name] = value;
  }
  return script;
}

/** The text of the chat's message bubbles inside the widget. */
function bubbles(widget: GraphrtChatWidgetElement): string[] {
  const chat = widget.shadowRoot?.querySelector("graphrt-chat");
  return [...(chat?.shadowRoot?.querySelectorAll(".bubble") ?? [])].map((bubble) => bubble.textContent);
}

// ---- <graphrt-chat> -------------------------------------------------------------

test("the chat fires ready, message and answer as DOM events, and messages sent early wait their turn", async () => {
  const chat = newChat("shop");
  const seen = recordEvents(document.body);
  chat.setContext({ product_id: "sku-1" });
  // Before the chat is in the page: both wait, then go one after the other.
  const first = chat.send("Price?");
  const second = chat.send("Stock?");
  document.body.append(chat);
  await Promise.all([first, second]);
  assert.deepEqual(names(seen), ["ready", "message", "answer", "message", "answer"]);
  const answers = seen
    .filter(([name]) => name === "answer")
    .map(([, detail]) => (detail as { text: string }).text);
  assert.deepEqual(answers, ["Price? about sku-1", "Stock? about sku-1"]);
  assert.deepEqual(seen[1]?.[1], { id: "m1", text: "Price?" });
  // A new context counts from the next message; the one before stays when a new one is refused.
  chat.context = { product_id: "sku-9" };
  assert.throws(() => chat.setContext({ big: "x".repeat(9000) }), /more than 8192/);
  await chat.send("And now?");
  assert.equal(chat.session?.state.messages.at(-1)?.text, "And now? about sku-9");
  await chat.reset();
  assert.equal(chat.session?.state.messages.length, 0, "a new conversation");
  chat.remove();
});

test("a question of a paused run is a handoff to the visitor", async () => {
  const chat = newChat("confirm");
  const seen = recordEvents(chat);
  document.body.append(chat);
  await chat.send("send it");
  assert.deepEqual(names(seen), ["ready", "message", "handoff"]);
  assert.deepEqual(seen[2]?.[1], {
    id: "m2",
    runId: chat.session?.state.messages[1]?.runId,
    waitingFor: "visitor",
  });
  chat.remove();
});

test("a chat that cannot be used fires error, and send gives up instead of waiting", async () => {
  const chat = newChat("memo");
  const seen = recordEvents(chat);
  document.body.append(chat);
  await chat.send("anyone?");
  assert.deepEqual(seen, [["error", { during: "connect", message: "This chat is not available." }]]);
  chat.remove();
});

// ---- <graphrt-chat-widget> -------------------------------------------------------

test("the widget fires open before ready, and one close however it is closed", async () => {
  const widget = document.createElement("graphrt-chat-widget") as GraphrtChatWidgetElement;
  widget.setAttribute("server", server.url);
  widget.setAttribute("workflow", "shop");
  const seen = recordEvents(widget);
  // Sent before the widget is in the page: it opens and sends once the chat is ready.
  const sent = widget.send("Hello?");
  document.body.append(widget);
  await sent;
  assert.deepEqual(names(seen), ["open", "ready", "message", "answer"]);
  assert.deepEqual(seen[0]?.[1], { workflow: "shop" });
  // The chat's own close button: one `close`, from the widget.
  const chat = widget.shadowRoot?.querySelector("graphrt-chat") as GraphrtChatElement;
  const close = [...(chat.shadowRoot?.querySelectorAll("header button") ?? [])].find(
    (button) => button.getAttribute("aria-label") === "Close",
  ) as HTMLButtonElement;
  close.click();
  assert.deepEqual(names(seen).slice(4), ["close"]);
  widget.open();
  widget.close();
  widget.close();
  assert.deepEqual(names(seen).slice(4), ["close", "open", "close"]);
  widget.remove();
});

// ---- window.GraphrtChat ------------------------------------------------------------

test("calls queued before the script loaded run in order, then the bubble shows", async () => {
  const order: string[] = [];
  const holder: GraphrtChatHolder = {
    GraphrtChat: {
      q: [
        ["init", { workflow: "shop", heading: "Ask the shop", context: { product_id: "sku-7" } }],
        (chat) => {
          for (const name of NAMES) {
            chat.on(name, () => order.push(name));
          }
        },
        ["setTheme", "dark"],
        ["send", "Fits me?"],
      ],
    },
  };
  // `data-workflow` loses to `init`; the other attributes still count.
  const api = mountEmbed(scriptTag({ workflow: "report", position: "left" }), holder);
  assert.ok(api);
  assert.equal(holder.GraphrtChat, api);
  const widget = api.element;
  assert.equal(widget.getAttribute("server"), server.url);
  assert.equal(widget.getAttribute("workflow"), "shop");
  assert.equal(widget.getAttribute("position"), "left");
  assert.equal(widget.getAttribute("theme"), "dark");
  assert.equal(widget.isConnected, true);
  await until("the answer", () => order.includes("answer"));
  assert.deepEqual(order, ["open", "ready", "message", "answer"]);
  assert.deepEqual(bubbles(widget), ["Fits me?", "Fits me? about sku-7"]);
  const chat = widget.shadowRoot?.querySelector("graphrt-chat");
  assert.equal(chat?.shadowRoot?.querySelector("h1")?.textContent, "Ask the shop");
  widget.remove();
});

test("after the script loaded, q.push runs at once and on/off add and remove listeners", async () => {
  const holder: GraphrtChatHolder = {};
  const api = mountEmbed(scriptTag({ workflow: "shop" }), holder) as GraphrtChatApi;
  let ran: GraphrtChatApi | undefined;
  api.q.push((chat) => {
    ran = chat;
  });
  assert.equal(ran, api);
  const answers: string[] = [];
  const onAnswer = (detail: { text: string }): void => void answers.push(detail.text);
  const stop = api.on("answer", onAnswer);
  api.on("answer", onAnswer);
  api.setContext({ product_id: "sku-1" });
  await api.send("One");
  assert.deepEqual(answers, ["One about sku-1"], "a listener added twice is called once");
  stop();
  await api.send("Two");
  assert.deepEqual(answers, ["One about sku-1"]);
  api.on("answer", onAnswer);
  api.off("answer", onAnswer);
  api.setContext(null);
  await api.send("Three");
  assert.deepEqual(answers, ["One about sku-1"]);
  assert.deepEqual(bubbles(api.element).at(-1), "Three about ");
  // A typo is an error, not a listener that never runs.
  assert.throws(() => api.on("answered" as ChatEventName, () => undefined), /Unknown chat event `answered`/);
  assert.throws(() => api.setContext("sku-1" as never), /must be a JSON object/);
  // `reset` starts a new conversation.
  await api.reset();
  assert.deepEqual(bubbles(api.element), []);
  api.element.remove();
});

test("data-open, setTheme auto and a second script tag", () => {
  const holder: GraphrtChatHolder = {};
  const api = mountEmbed(scriptTag({ workflow: "shop", theme: "dark", open: "" }), holder);
  assert.ok(api);
  assert.equal(api.element.hasAttribute("open"), true);
  assert.equal(api.element.getAttribute("theme"), "dark");
  api.setTheme("auto");
  assert.equal(api.element.hasAttribute("theme"), false);
  api.setTheme("light");
  assert.equal(api.element.getAttribute("theme"), "light");
  api.close();
  assert.equal(api.element.hasAttribute("open"), false);
  // Included twice: the page keeps one bubble and one API.
  assert.equal(mountEmbed(scriptTag(), holder), undefined);
  assert.equal(holder.GraphrtChat, api);
  assert.equal(document.querySelectorAll("graphrt-chat-widget").length, 1);
  api.element.remove();
});

test("init can keep data-open closed, and a failing queued call does not stop the others", () => {
  const reported: unknown[] = [];
  const global = globalThis as { reportError?: (error: unknown) => void };
  global.reportError = (error) => void reported.push(error);
  try {
    const holder: GraphrtChatHolder = {
      GraphrtChat: {
        q: [
          ["setContext", { when: new Date() } as never],
          () => {
            throw new Error("a bug of the page");
          },
          ["init", { open: false, greeting: "Hi there" }],
        ],
      },
    };
    const api = mountEmbed(scriptTag({ workflow: "shop", open: "" }), holder);
    assert.ok(api);
    assert.equal(api.element.hasAttribute("open"), false);
    assert.equal(api.element.getAttribute("greeting"), "Hi there");
    assert.equal(reported.length, 2);
    assert.match(String(reported[0]), /context\.when is not a plain object/);
    api.element.remove();
  } finally {
    delete global.reportError;
  }
});
