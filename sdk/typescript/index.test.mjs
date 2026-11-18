// Tests for tool result images: node --test
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import { test } from "node:test";

import { Plugin, PluginError, toolResult } from "./index.mjs";

const PNG = Buffer.from("hello");

/** Send `messages` to a plugin and collect its replies until every request with an id is answered. */
async function exchange(plugin, messages) {
  const input = new PassThrough();
  const output = new PassThrough();
  const replies = [];
  let buffered = "";
  const expected = messages.filter((m) => m.id !== undefined).length;
  const done = new Promise((resolve) => {
    output.on("data", (chunk) => {
      buffered += chunk.toString();
      let newline;
      while ((newline = buffered.indexOf("\n")) >= 0) {
        replies.push(JSON.parse(buffered.slice(0, newline)));
        buffered = buffered.slice(newline + 1);
      }
      if (replies.length >= expected) resolve();
    });
  });
  const running = plugin.run({ input, output });
  for (const m of messages) input.write(JSON.stringify({ jsonrpc: "2.0", ...m }) + "\n");
  await done;
  input.end();
  await running;
  return replies;
}

function screenPlugin(images) {
  return new Plugin("screen").tool({ name: "shot" }, async () => ({ text: "800x600", images }));
}

const call = { id: "t", method: "tool.call", params: { name: "shot", arguments: {} } };

test("images are sent to hosts that announce tool_images", async () => {
  const replies = await exchange(screenPlugin([{ media_type: "image/png", data: PNG }, { url: "https://x/a.png" }]), [
    { id: "i", method: "initialize", params: { features: ["tool_images"] } },
    call,
  ]);
  const reply = replies.find((r) => r.id === "t");
  assert.deepEqual(reply.result, {
    text: "800x600",
    is_error: false,
    images: [{ media_type: "image/png", data_base64: PNG.toString("base64") }, { url: "https://x/a.png" }],
  });
});

test("images are dropped with a note for older hosts", async () => {
  const replies = await exchange(screenPlugin([{ media_type: "image/png", data_base64: "aGVsbG8=" }]), [
    { id: "i", method: "initialize", params: {} },
    call,
  ]);
  const reply = replies.find((r) => r.id === "t");
  assert.equal(reply.result.images, undefined);
  assert.equal(reply.result.text, "800x600\n[1 image(s) omitted: the graphrt host does not support image tool results]");
});

test("invalid images fail the call", async () => {
  const replies = await exchange(screenPlugin([{ media_type: "image/bmp", data_base64: "aGVs" }]), [
    { id: "i", method: "initialize", params: { features: ["tool_images"] } },
    call,
  ]);
  const reply = replies.find((r) => r.id === "t");
  assert.equal(reply.error.data.kind, "invalid_output");
  assert.match(reply.error.message, /image\/bmp/);
});

test("results above the host's line limit are reported, not sent", async () => {
  const big = Buffer.alloc(4096, 1);
  const replies = await exchange(screenPlugin([{ media_type: "image/png", data: big }]), [
    { id: "i", method: "initialize", params: { features: ["tool_images"], max_message_bytes: 2048 } },
    call,
  ]);
  const reply = replies.find((r) => r.id === "t");
  assert.equal(reply.error.data.kind, "message_too_large");
});

test("toolResult validates limits", () => {
  assert.deepEqual(toolResult("hi"), { text: "hi", is_error: false });
  const five = Array.from({ length: 5 }, () => ({ url: "https://x/a.png" }));
  assert.throws(() => toolResult({ text: "", images: five }), PluginError);
  assert.throws(() => toolResult({ text: "", images: [{ url: "http://x/a.png" }] }), /https/);
  assert.throws(() => toolResult({ text: "", images: [{ media_type: "image/png", data_base64: "%%%" }] }), /base64/);
  const huge = "A".repeat(((5 * 1024 * 1024) / 3 + 1) * 4);
  assert.throws(() => toolResult({ text: "", images: [{ media_type: "image/png", data_base64: huge }] }), /limit/);
});
