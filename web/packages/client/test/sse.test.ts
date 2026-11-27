import assert from "node:assert/strict";
import { test } from "node:test";
import { readSse, type SseMessage, SseParser } from "../src/sse.ts";

function parseAll(chunks: string[]): SseMessage[] {
  const parser = new SseParser();
  return chunks.flatMap((chunk) => parser.push(chunk));
}

test("a message has its event name, id and data", () => {
  const messages = parseAll(['event: response\nid: 4.0\ndata: {"text":"Hi"}\n\n']);
  assert.deepEqual(messages, [{ event: "response", id: "4.0", data: '{"text":"Hi"}' }]);
});

test("a message split anywhere across chunks is the same message", () => {
  const text = 'event: run_started\r\nid: 0\r\ndata: {"a":1}\r\n\r\nevent: x\ndata: 1\n\n';
  const whole = parseAll([text]);
  assert.equal(whole.length, 2);
  for (let cut = 1; cut < text.length; cut += 1) {
    assert.deepEqual(parseAll([text.slice(0, cut), text.slice(cut)]), whole, `cut at ${cut}`);
  }
  // One character at a time, too.
  assert.deepEqual(parseAll([...text]), whole);
});

test("comments keep the connection alive and are not messages", () => {
  assert.deepEqual(parseAll([": keep-alive\n\n", ":\n\n", "data: x\n\n"]), [{ event: "message", data: "x" }]);
});

test("several data lines are joined, and only one leading space is dropped", () => {
  const messages = parseAll(["data: first\ndata:  second\ndata:third\n\n"]);
  assert.deepEqual(messages, [{ event: "message", data: "first\n second\nthird" }]);
});

test("a block without data is not a message, and an id does not carry over", () => {
  const messages = parseAll(["id: 7\nevent: nothing\n\n", "data: x\n\n"]);
  assert.deepEqual(messages, [{ event: "message", data: "x" }]);
});

test("unknown fields and a line without a colon are ignored", () => {
  assert.deepEqual(parseAll(["retry: 100\nfoo\ndata: x\n\n"]), [{ event: "message", data: "x" }]);
});

test("an unfinished message at the end of the stream is dropped", () => {
  assert.deepEqual(parseAll(["data: complete\n\ndata: cut off"]), [{ event: "message", data: "complete" }]);
});

test("a stream is decoded as UTF-8 even when a character is split", async () => {
  const bytes = new TextEncoder().encode("data: héllo ✓\n\n");
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const byte of bytes) {
        controller.enqueue(Uint8Array.of(byte));
      }
      controller.close();
    },
  });
  const messages: SseMessage[] = [];
  for await (const message of readSse(body)) {
    messages.push(message);
  }
  assert.deepEqual(messages, [{ event: "message", data: "héllo ✓" }]);
});

test("leaving the stream early cancels the body", async () => {
  let cancelled = false;
  const body = new ReadableStream<Uint8Array>({
    pull(controller) {
      controller.enqueue(new TextEncoder().encode("data: again\n\n"));
    },
    cancel() {
      cancelled = true;
    },
  });
  for await (const message of readSse(body)) {
    assert.equal(message.data, "again");
    break;
  }
  assert.equal(cancelled, true);
});
