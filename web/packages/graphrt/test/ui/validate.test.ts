/**
 * The validator against the conformance fixtures of the `graphrt-ui` crate:
 * what the Rust reference accepts and refuses, this port accepts and refuses,
 * with the same reason.
 */
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { collectUiBlocks, MAX_COLLECTED_BLOCKS, UiError, validate } from "../../src/ui/validate/index.ts";
import { fixturesDir } from "./fixtures.ts";

function fixtures(kind: "valid" | "invalid"): [string, unknown][] {
  const dir = join(fixturesDir, kind);
  const files = readdirSync(dir)
    .filter((file) => file.endsWith(".json"))
    .sort();
  assert.ok(files.length > 0, `no fixtures in ${dir}`);
  return files.map((file) => [file, JSON.parse(readFileSync(join(dir, file), "utf8"))]);
}

test("every valid fixture is accepted", () => {
  for (const [file, block] of fixtures("valid")) {
    assert.doesNotThrow(() => validate(block), file);
  }
});

test("every invalid fixture is refused for the expected reason", () => {
  for (const [file, fixture] of fixtures("invalid")) {
    const { block, error } = fixture as { block: unknown; error: string };
    assert.throws(
      () => validate(block),
      (thrown: unknown) => {
        assert.ok(thrown instanceof UiError, `${file}: ${String(thrown)}`);
        assert.ok(thrown.message.includes(error), `${file}: \`${thrown.message}\` lacks \`${error}\``);
        return true;
      },
      file,
    );
  }
});

test("the envelope is checked first", () => {
  assert.throws(() => validate("chart"), /must be a JSON object/);
  assert.throws(() => validate([{ $ui: "chart" }]), /must be a JSON object/);
  assert.throws(() => validate({ ui: "chart" }), /needs a string `\$ui`/);
  assert.throws(() => validate({ $ui: "stats", version: 2, items: [] }), /`version` must be 1/);
  assert.throws(() => validate({ $ui: "stats", version: "1", items: [] }), /`version` must be 1/);
});

test("a block over the size limit is refused", () => {
  const rows = Array.from({ length: 1000 }, () => ({ a: "x".repeat(300) }));
  const block = { $ui: "table", version: 1, columns: [{ key: "a", label: "A" }], rows };
  assert.throws(() => validate(block), /at most 262144 are allowed/);
});

test("errors name the field by its JSON pointer", () => {
  try {
    validate({
      $ui: "stats",
      version: 1,
      items: [
        { label: "Orders", value: 1 },
        { label: "", value: 2 },
      ],
    });
    assert.fail("accepted");
  } catch (error) {
    assert.ok(error instanceof UiError);
    assert.equal(error.path, "/items/1/label");
    assert.equal(error.message, "at /items/1/label: must not be empty");
  }
});

test("URLs that could run code or leave https are refused", () => {
  const card = (url: string, kind: "image" | "link"): unknown => ({
    $ui: "adaptive_card",
    version: 1,
    card: {
      type: "AdaptiveCard",
      version: "1.5",
      body: kind === "image" ? [{ type: "Image", url }] : [],
      actions: kind === "link" ? [{ type: "Action.OpenUrl", url }] : [],
    },
  });
  for (const url of [
    "javascript:alert(1)",
    "JaVaScRiPt:alert(1)",
    "http://example.com/a.png",
    "data:image/svg+xml;base64,PHN2Zz4=",
    "data:text/html;base64,PGI+",
    "https://user@evil.example/a.png",
    "https://exa mple.com/a.png",
    "https://",
    "//example.com/a.png",
    "/relative.png",
  ]) {
    assert.throws(() => validate(card(url, "image")), UiError, `image ${url}`);
    assert.throws(() => validate(card(url, "link")), UiError, `link ${url}`);
  }
  assert.doesNotThrow(() => validate(card("https://example.com/a.png", "image")));
  assert.doesNotThrow(() => validate(card("data:image/png;base64,iVBORw0KGgo=", "image")));
  assert.doesNotThrow(() => validate(card("mailto:help@example.com", "link")));
  // A data URL is an image, never a link.
  assert.throws(() => validate(card("data:image/png;base64,iVBORw0KGgo=", "link")), UiError);
});

test("blocks are found in outputs like the engine finds them", () => {
  const found = collectUiBlocks({
    first: { $ui: "chart", nested: { $ui: "table" } },
    list: [{ $ui: "stats" }, "text", { wrapper: { $ui: "adaptive_card" } }],
  });
  assert.deepEqual(
    found.map((block) => (block as { $ui: string }).$ui),
    ["chart", "stats", "adaptive_card"],
  );
  assert.equal(collectUiBlocks({ a: { b: { c: { $ui: "chart" } } } }).length, 1);
  assert.equal(collectUiBlocks({ a: { b: { c: { d: { $ui: "chart" } } } } }).length, 0);
  assert.equal(
    collectUiBlocks(Array.from({ length: 40 }, () => ({ $ui: "chart" }))).length,
    MAX_COLLECTED_BLOCKS,
  );
  assert.equal(collectUiBlocks("chart").length, 0);
  assert.equal(collectUiBlocks({ $ui: 3 }).length, 0);
});
