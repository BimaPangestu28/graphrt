/**
 * The chat components in a DOM (happy-dom), against a real `graphrt serve`.
 */
import "../../../../test-support/dom.ts";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";
import { runInThisContext } from "node:vm";
import { startServer, type TestServer } from "../../../../test-support/server.ts";
import {
  defineGraphrtChatWidget,
  type GraphrtChatElement,
  type GraphrtChatWidgetElement,
  loadDiagrams,
  richText,
  type ScriptLoader,
} from "../../src/chat/index.ts";

/** What stands in for Mermaid: the same global, a picture that tells the source's length. */
const FAKE_MERMAID = `globalThis.mermaid = {
  initialize() {},
  render(id, source) {
    return Promise.resolve({ svg: '<svg xmlns="http://www.w3.org/2000/svg"><text>' + source.length + "</text></svg>" });
  },
};`;

let server: TestServer;
let assets: string;

before(async () => {
  assets = await mkdtemp(join(tmpdir(), "graphrt-chat-assets-"));
  await writeFile(join(assets, "mermaid.min.js"), FAKE_MERMAID);
  server = await startServer([
    "--chat",
    "confirm",
    "--chat",
    "report",
    "--chat",
    "diagram",
    "--chat-mermaid",
    join(assets, "mermaid.min.js"),
    "--chat-access",
    "guests",
    "--user-resume",
  ]);
  defineGraphrtChatWidget();
});

after(async () => {
  await server.stop();
  await rm(assets, { recursive: true, force: true });
});

async function until(what: string, condition: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (condition()) {
      return;
    }
    await new Promise((done) => setTimeout(done, 25));
  }
  assert.fail(`timed out waiting for ${what}`);
}

function mountChat(workflow: string): { chat: GraphrtChatElement; root: ShadowRoot } {
  const chat = document.createElement("graphrt-chat") as GraphrtChatElement;
  chat.setAttribute("server", server.url);
  chat.setAttribute("workflow", workflow);
  document.body.append(chat);
  assert.ok(chat.shadowRoot);
  return { chat, root: chat.shadowRoot };
}

async function send(root: ShadowRoot, text: string): Promise<void> {
  const composer = root.querySelector("form.composer") as HTMLFormElement;
  await until("the composer", () => !composer.hidden);
  (root.querySelector("textarea") as HTMLTextAreaElement).value = text;
  composer.dispatchEvent(new Event("submit", { cancelable: true }));
}

test("a message is sent and its answer and UI blocks are drawn", async () => {
  const { chat, root } = mountChat("report");
  await send(root, "numbers <b>please</b>");
  await until("the answer", () => chat.session?.state.messages.at(-1)?.state === "done");
  const bubbles = [...root.querySelectorAll(".bubble")];
  assert.equal(bubbles[0]?.textContent, "numbers <b>please</b>");
  assert.equal(bubbles[0]?.querySelector("b"), null, "what the person typed is text");
  assert.equal(bubbles[1]?.textContent, "Here are the numbers.");
  const block = root.querySelector("graphrt-block");
  assert.ok(block?.shadowRoot?.textContent.includes("Orders"), "the stats block of the outputs is drawn");
  assert.equal((root.querySelector("textarea") as HTMLTextAreaElement).value, "", "the composer is cleared");
  assert.equal(root.querySelectorAll('.tools button[aria-label="Helpful"]').length, 1);
  assert.equal(root.querySelector("h1")?.textContent, "confirm", "the title the server configured");
  // A theme set later reaches the blocks already drawn.
  chat.setAttribute("theme", "dark");
  assert.equal(block?.getAttribute("theme"), "dark");
  chat.removeAttribute("theme");
  assert.equal(block?.hasAttribute("theme"), false);
  chat.remove();
});

test("a question of a paused run is answered in the chat", async () => {
  const { chat, root } = mountChat("confirm");
  await send(root, "send it");
  await until("the question", () => chat.session?.state.messages.at(-1)?.state === "waiting");
  const card = root.querySelector("graphrt-block")?.shadowRoot;
  assert.ok(card);
  assert.ok(card.textContent.includes("Send it?"));
  (card.querySelector("input") as HTMLInputElement).value = "now";
  const yes = [...card.querySelectorAll("button")].find((button) => button.textContent === "Yes");
  assert.ok(yes);
  yes.click();
  await until("the rest of the answer", () => chat.session?.state.messages.at(-1)?.state === "done");
  assert.equal([...root.querySelectorAll(".bubble")].at(-1)?.textContent, "sent (now)");
  assert.equal(root.querySelector("graphrt-block"), null, "the answered question is gone");
  chat.remove();
});

test("the widget opens a chat on demand and closes again", async () => {
  const widget = document.createElement("graphrt-chat-widget") as GraphrtChatWidgetElement;
  widget.setAttribute("server", server.url);
  widget.setAttribute("workflow", "report");
  widget.setAttribute("heading", "Ask us");
  document.body.append(widget);
  const root = widget.shadowRoot;
  assert.ok(root);
  const launcher = root.querySelector("button.launcher") as HTMLButtonElement;
  const panel = root.querySelector(".panel") as HTMLElement;
  assert.equal(panel.hidden, true);
  assert.equal(root.querySelector("graphrt-chat"), null, "no chat, and no request, before it is opened");
  launcher.click();
  assert.equal(panel.hidden, false);
  assert.equal(launcher.getAttribute("aria-expanded"), "true");
  const chat = root.querySelector("graphrt-chat") as GraphrtChatElement;
  assert.equal(chat.getAttribute("server"), server.url);
  assert.equal(chat.shadowRoot?.querySelector("h1")?.textContent, "Ask us");
  // The chat's own close button closes the panel and keeps the chat for next time.
  const close = [...(chat.shadowRoot?.querySelectorAll("header button") ?? [])].find(
    (button) => button.getAttribute("aria-label") === "Close",
  ) as HTMLButtonElement;
  assert.equal(close.hidden, false);
  close.click();
  assert.equal(panel.hidden, true);
  assert.equal(widget.hasAttribute("open"), false);
  widget.toggle();
  assert.equal(root.querySelectorAll("graphrt-chat").length, 1);
  widget.remove();
});

test("answer text keeps code and safe links, and nothing else", () => {
  const holder = document.createElement("div");
  holder.append(
    ...richText(
      "Run `graphrt serve` **now**.\n```sh\ncurl <b>x</b>\n```\nSee https://example.com/docs, [more](https://example.com/more) " +
        "or [bad](javascript:alert(1)) <img src=x onerror=alert(1)>",
    ),
  );
  assert.equal(holder.querySelector("code")?.textContent, "graphrt serve");
  assert.equal(holder.querySelector("strong")?.textContent, "now");
  assert.equal(holder.querySelector("pre")?.textContent, "curl <b>x</b>");
  const links = [...holder.querySelectorAll("a")];
  assert.deepEqual(
    links.map((link) => link.getAttribute("href")),
    ["https://example.com/docs", "https://example.com/more"],
  );
  assert.ok(links.every((link) => link.rel === "noopener noreferrer" && link.target === "_blank"));
  assert.equal(holder.querySelectorAll("img, b, script").length, 0);
  assert.ok(holder.textContent.includes("<img src=x onerror=alert(1)>"));
});

test("a diagram is text while the server's Mermaid loads, then a picture", async () => {
  let release: () => void = () => undefined;
  const held = new Promise<void>((done) => {
    release = done;
  });
  // What a script element does in a browser: fetch the server's file and run it.
  const loader: ScriptLoader = async (url) => {
    await held;
    const response = await fetch(url);
    assert.equal(response.status, 200);
    runInThisContext(await response.text());
  };
  const loaded = loadDiagrams(server.url, loader);
  const { chat, root } = mountChat("diagram");
  await send(root, "draw it");
  await until("the answer", () => chat.session?.state.messages.at(-1)?.state === "done");
  const block = root.querySelector("graphrt-block")?.shadowRoot;
  assert.ok(block);
  assert.ok(block.querySelector(".diagram pre")?.textContent.includes("flowchart TD"));
  assert.equal(block.querySelector("img"), null);
  release();
  assert.equal(await loaded, true);
  await until("the picture", () => block.querySelector(".diagram img") !== null);
  const source = block.querySelector("img")?.getAttribute("src") ?? "";
  assert.match(source, /^data:image\/svg\+xml;charset=utf-8,/);
  assert.equal(block.querySelector("svg"), null, "the picture is an image, never markup of the page");
  chat.remove();
});
