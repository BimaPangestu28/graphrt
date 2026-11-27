/**
 * The widget script as a site gets it: `/chat/embed.js` as `graphrt serve`
 * serves it (the committed bundle, not the sources), run in a page (happy-dom)
 * with the queue stub, against a real server.
 *
 * Nothing of the sources is imported here, so the custom elements are the
 * bundle's own.
 */
import "../../../../test-support/dom.ts";
import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { runInThisContext } from "node:vm";
import { startServer, type TestServer } from "../../../../test-support/server.ts";
import type { ChatAnswerDetail, GraphrtChatApi, GraphrtChatStub } from "../../src/chat/index.ts";

let server: TestServer;

before(async () => {
  server = await startServer([
    "--chat",
    "shop",
    "--chat-access",
    "guests",
    "--cors-origin",
    "https://agent.example",
  ]);
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

test("the served embed.js runs the queue, chats with the workflow and reports its events", async () => {
  const response = await fetch(`${server.url}/chat/embed.js`);
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type") ?? "", /javascript/);
  const code = await response.text();

  // What the page has before the script: the stub and its calls.
  const events: string[] = [];
  const answers: ChatAnswerDetail[] = [];
  const page = globalThis as { GraphrtChat?: GraphrtChatApi | GraphrtChatStub };
  page.GraphrtChat = page.GraphrtChat ?? { q: [] };
  const stub = page.GraphrtChat as GraphrtChatStub;
  stub.q.push(
    ["init", { workflow: "shop", heading: "Shop help" }],
    (chat) => {
      for (const name of ["open", "ready", "message", "answer", "handoff", "error"] as const) {
        chat.on(name, () => events.push(name));
      }
      chat.on("answer", (answer) => answers.push(answer));
    },
    ["setContext", { product_id: "sku-42", page: "https://agent.example/p/sku-42" }],
  );

  // The script tag, as `document.currentScript` while it runs.
  const script = document.createElement("script");
  script.src = `${server.url}/chat/embed.js`;
  script.dataset.position = "left";
  document.head.append(script);
  Object.defineProperty(document, "currentScript", { value: script, configurable: true });
  try {
    runInThisContext(code);
  } finally {
    Object.defineProperty(document, "currentScript", { value: null, configurable: true });
  }

  const api = page.GraphrtChat as GraphrtChatApi;
  assert.equal(typeof api.send, "function", "the stub became the API");
  const widget = document.querySelector("graphrt-chat-widget");
  assert.ok(widget, "the bubble is in the page");
  assert.equal(widget.getAttribute("server"), server.url);
  assert.equal(widget.getAttribute("position"), "left");
  assert.equal(widget.shadowRoot?.querySelector("graphrt-chat"), null, "nothing connects before it opens");

  // After the load, the same queue runs a call at once.
  api.q.push(["send", "Do you have it in blue?"]);
  await until("the answer", () => answers.length === 1);
  assert.deepEqual(events, ["open", "ready", "message", "answer"]);
  assert.equal(answers[0]?.text, "Do you have it in blue? about sku-42");
  assert.match(answers[0]?.runId ?? "", /^run-/);
  const chat = widget.shadowRoot?.querySelector("graphrt-chat");
  assert.equal(chat?.shadowRoot?.querySelector("h1")?.textContent, "Shop help");
  widget.remove();
});
