/**
 * The renderer against the conformance fixtures, and against blocks written
 * to break out of it.
 */
import "../../../test-support/dom.ts";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import {
  type BlockAction,
  defineGraphrtUi,
  formatNumber,
  GraphrtBlockElement,
  mermaidRenderer,
  renderBlock,
  setDiagramRenderer,
} from "../src/index.ts";
import { inlineMarkdown } from "../src/render/card.ts";
import { ticks } from "../src/render/chart.ts";
import { fixturesDir } from "./fixtures.ts";

function fixtures(kind: "valid" | "invalid"): [string, unknown][] {
  const dir = join(fixturesDir, kind);
  return readdirSync(dir)
    .filter((file) => file.endsWith(".json"))
    .sort()
    .map((file) => [file, JSON.parse(readFileSync(join(dir, file), "utf8"))]);
}

function fixture(name: string): unknown {
  return JSON.parse(readFileSync(join(fixturesDir, "valid", `${name}.json`), "utf8"));
}

function all(root: Element, selector: string): Element[] {
  return [...root.querySelectorAll(selector)];
}

/** Nothing in `root` can run code or load from a place the URL policy forbids. */
function assertInert(root: Element, label: string): void {
  assert.equal(all(root, "script, iframe, object, embed, link, style, base, form, meta").length, 0, label);
  for (const element of [root, ...all(root, "*")]) {
    for (const attribute of element.getAttributeNames()) {
      assert.ok(!attribute.startsWith("on"), `${label}: handler attribute ${attribute}`);
      assert.notEqual(attribute, "style", `${label}: inline style attribute`);
      assert.notEqual(attribute, "srcdoc", label);
    }
    const href = element.getAttribute("href");
    if (href !== null) {
      assert.match(href, /^(https:\/\/|mailto:)/, `${label}: href ${href}`);
      assert.equal(element.getAttribute("rel"), "noopener noreferrer", label);
    }
    const src = element.getAttribute("src");
    if (src !== null) {
      assert.match(
        src,
        /^(https:\/\/|data:image\/(png|jpeg|gif|webp|svg\+xml);)/,
        `${label}: src ${src.slice(0, 40)}`,
      );
    }
  }
}

const HOSTILE = '<img src=x onerror="globalThis.pwned=1"><script>globalThis.pwned=1</script>';

test("every valid fixture is drawn, and what is drawn is inert", () => {
  for (const [file, block] of fixtures("valid")) {
    const drawn = renderBlock(block);
    assert.ok(!drawn.classList.contains("error"), `${file} was refused`);
    assert.ok(drawn.textContent.trim().length > 0, `${file} drew nothing`);
    assertInert(drawn, file);
  }
});

test("every invalid fixture is refused: the notice and the reason are shown instead", () => {
  for (const [file, value] of fixtures("invalid")) {
    const { block, error } = value as { block: unknown; error: string };
    const reasons: string[] = [];
    const drawn = renderBlock(block, { onError: (reason) => reasons.push(reason.message) });
    assert.ok(drawn.classList.contains("error"), `${file} was drawn`);
    assert.ok(reasons[0]?.includes(error), `${file}: ${reasons[0]}`);
    assertInert(drawn, file);
    assert.equal(
      all(drawn, "img, a, button, input, svg").length,
      0,
      `${file}: parts of the block were drawn`,
    );
  }
});

test("markup in any text stays text", () => {
  const blocks: unknown[] = [
    {
      $ui: "stats",
      version: 1,
      title: HOSTILE,
      items: [{ label: HOSTILE, value: "<b>1</b>", caption: HOSTILE }],
    },
    {
      $ui: "table",
      version: 1,
      subtitle: HOSTILE,
      columns: [{ key: "a", label: HOSTILE, format: "status" }],
      rows: [{ a: HOSTILE }],
    },
    {
      $ui: "chart",
      version: 1,
      type: "bar",
      x: { values: [HOSTILE] },
      series: [{ name: HOSTILE, values: [1] }],
    },
    { $ui: "chart", version: 1, type: "pie", x: { values: [HOSTILE] }, series: [{ name: "s", values: [1] }] },
    {
      $ui: "adaptive_card",
      version: 1,
      card: {
        type: "AdaptiveCard",
        version: "1.5",
        body: [
          { type: "TextBlock", text: `${HOSTILE} [x](javascript:alert(1)) **${HOSTILE}**`, wrap: true },
          { type: "FactSet", facts: [{ title: HOSTILE, value: HOSTILE }] },
          { type: "Input.Text", id: "t", label: HOSTILE, placeholder: HOSTILE, value: HOSTILE },
          { type: "Input.ChoiceSet", id: "c", choices: [{ title: HOSTILE, value: HOSTILE }] },
        ],
        actions: [{ type: "Action.Submit", title: HOSTILE, tooltip: HOSTILE }],
      },
    },
  ];
  for (const block of blocks) {
    const drawn = renderBlock(block);
    assert.ok(!drawn.classList.contains("error"), JSON.stringify(block).slice(0, 60));
    assertInert(drawn, "hostile text");
    assert.equal(all(drawn, "img, script").length, 0);
    assert.ok(drawn.textContent.includes("onerror="), "the text is shown as it is");
  }
  assert.equal((globalThis as { pwned?: number }).pwned, undefined);
  // A fallback text of a refused block is text too.
  const refused = renderBlock({ $ui: "chart", version: 1, fallback_text: HOSTILE, type: "nope" });
  assert.ok(refused.classList.contains("error"));
  assertInert(refused, "fallback text");
  assert.ok(refused.textContent.includes("<script>"));
});

test("a block that fails validation is not drawn even when only one field is bad", () => {
  const card = {
    $ui: "adaptive_card",
    version: 1,
    card: {
      type: "AdaptiveCard",
      version: "1.5",
      body: [
        { type: "TextBlock", text: "fine" },
        { type: "Image", url: "http://tracker.example/pixel.gif" },
      ],
    },
  };
  const drawn = renderBlock(card);
  assert.ok(drawn.classList.contains("error"));
  assert.equal(all(drawn, "img").length, 0);
});

test("stats show formatted values and whether a change is good news", () => {
  const drawn = renderBlock(fixture("stats-weekly-kpis"), { locale: "id-ID" });
  const tiles = all(drawn, ".stat");
  assert.equal(tiles.length, 4);
  assert.match(tiles[0]?.querySelector(".stat-value")?.textContent ?? "", /Rp\s?125\.000\.000/);
  assert.ok(tiles[0]?.querySelector(".stat-delta")?.classList.contains("good"), "revenue up is good");
  assert.ok(tiles[1]?.querySelector(".stat-delta")?.classList.contains("bad"), "orders down is bad");
  // Refunds going down is good: `good: "down"`.
  assert.ok(tiles[2]?.querySelector(".stat-delta")?.classList.contains("good"));
  assert.equal(tiles[3]?.querySelector(".stat-value")?.textContent, "Sehat");
  assert.equal(drawn.querySelector(".title")?.textContent, "Minggu ini");
});

test("numbers follow their format", () => {
  assert.equal(formatNumber(1234.5, {}, "en-US"), "1,234.5");
  assert.equal(formatNumber(1840, { format: "compact" }, "en-US"), "1.8K");
  assert.equal(formatNumber(12.5, { format: "PERCENT" }, "en-US"), "12.5%");
  assert.equal(formatNumber(20, { format: "currency", currency: "USD" }, "en-US"), "$20");
  // An unknown currency code does not break the block.
  assert.doesNotThrow(() => formatNumber(20, { format: "currency", currency: "ZZZZ" }, "en-US"));
});

test("a table has a header per column and aligns numbers to the right", () => {
  const drawn = renderBlock(fixture("table-top-products"), { locale: "en-US" });
  const headers = all(drawn, "thead th");
  assert.deepEqual(
    headers.map((header) => header.textContent),
    ["Produk", "Terjual", "Pendapatan", "Status"],
  );
  assert.ok(headers[1]?.classList.contains("right"));
  assert.ok(!headers[0]?.classList.contains("right"));
  const rows = all(drawn, "tbody tr");
  assert.ok(rows.length > 0);
  assert.ok(rows[0]?.querySelector(".status"), "status cells are pills");
  assert.ok(drawn.querySelector(".scroll"), "wide tables scroll inside the block");
});

test("a bar chart draws one bar per value, with its value in a tooltip", () => {
  const drawn = renderBlock(fixture("chart-stacked-horizontal-bar"), { locale: "en-US" });
  const bars = all(drawn, "svg rect");
  assert.equal(bars.length, 6);
  assert.equal(bars[0]?.querySelector("title")?.textContent, "New, Jakarta: 120");
  // A stack grows from the end of the segment before it.
  const [first, second] = [bars[0], bars[1]];
  const end = Number(first?.getAttribute("x")) + Number(first?.getAttribute("width"));
  assert.ok(Math.abs(Number(second?.getAttribute("x")) - end) <= 3, "segments touch, with a small gap");
  assert.deepEqual(
    all(drawn, ".legend li").map((item) => item.textContent),
    ["New", "Returning"],
  );
  assert.ok(drawn.querySelector("svg")?.getAttribute("aria-label"));
  // The numbers are also there as a table.
  assert.equal(all(drawn, "details tbody tr").length, 3);
});

test("a gap in a line chart is a gap in the line", () => {
  const drawn = renderBlock(fixture("chart-line-with-gaps"));
  assert.equal(all(drawn, "path.line").length, 2, "two runs around the missing value");
  assert.equal(all(drawn, "circle").length, 4);
  assert.equal(all(drawn, ".legend").length, 0, "one series needs no legend");
});

test("a donut has a slice per category and a legend of the categories", () => {
  const drawn = renderBlock(fixture("chart-donut"));
  const block = fixture("chart-donut") as { x: { values: unknown[] } };
  assert.equal(all(drawn, "svg path").length, block.x.values.length);
  assert.equal(all(drawn, ".legend li").length, block.x.values.length);
  // One slice of the whole is a full ring, which an arc cannot draw.
  const whole = renderBlock({
    $ui: "chart",
    version: 1,
    type: "pie",
    x: { values: ["all"] },
    series: [{ name: "s", values: [5] }],
  });
  assert.equal(all(whole, "svg circle").length, 1);
});

test("axis ticks are round numbers that cover the data", () => {
  assert.deepEqual(ticks(0, 100), [0, 20, 40, 60, 80, 100]);
  assert.deepEqual(ticks(0, 7), [0, 2, 4, 6, 8]);
  const negative = ticks(-30, 45);
  assert.ok((negative[0] ?? 0) <= -30 && (negative.at(-1) ?? 0) >= 45);
  assert.deepEqual(ticks(5, 5), [5]);
});

test("card text keeps a little Markdown and only safe links", () => {
  const nodes = inlineMarkdown(
    "a **bold** and _soft_ [docs](https://example.com/x) [bad](javascript:alert(1))",
  );
  const holder = document.createElement("p");
  for (const node of nodes) {
    if (node) {
      holder.append(node);
    }
  }
  assert.equal(holder.querySelector("strong")?.textContent, "bold");
  assert.equal(holder.querySelector("em")?.textContent, "soft");
  const links = [...holder.querySelectorAll("a")];
  assert.equal(links.length, 1);
  assert.equal(links[0]?.getAttribute("href"), "https://example.com/x");
  assert.ok(holder.textContent.includes("[bad](javascript:alert(1))"));
});

test("pressing a card action sends the inputs and the action's id", () => {
  const actions: BlockAction[] = [];
  const drawn = renderBlock(fixture("adaptive-card-refund-approval"), {
    onAction: (action) => actions.push(action),
  });
  document.body.append(drawn);
  const note = drawn.querySelector("textarea");
  assert.ok(note);
  note.value = "Sorry about that";
  const amount = drawn.querySelector('input[type="number"]') as HTMLInputElement;
  assert.equal(amount.value, "250000");
  assert.equal(amount.max, "480000");
  const buttons = all(drawn, "button") as HTMLButtonElement[];
  assert.deepEqual(
    buttons.map((button) => button.textContent),
    ["Approve refund", "Reject"],
  );
  buttons[0]?.click();
  assert.deepEqual(actions, [
    {
      type: "submit",
      id: "approve",
      verb: undefined,
      data: { reason: "damaged", amount: 250000, note: "Sorry about that", restock: "false" },
    },
  ]);
  // One answer per card: the buttons that send are off afterwards.
  assert.ok(buttons.every((button) => button.disabled));
  const link = drawn.querySelector("a.button");
  assert.equal(link?.getAttribute("href"), "https://shop.example.com/orders/1042");
  assert.equal(link?.getAttribute("target"), "_blank");
  drawn.remove();
});

test("a required input stops the action and shows its message", () => {
  const actions: BlockAction[] = [];
  const card = {
    $ui: "adaptive_card",
    version: 1,
    card: {
      type: "AdaptiveCard",
      version: "1.5",
      body: [
        {
          type: "Input.Text",
          id: "name",
          label: "Name",
          isRequired: true,
          errorMessage: "Tell us your name",
        },
      ],
      actions: [
        { type: "Action.Submit", id: "go", title: "Go", data: { step: 2 } },
        { type: "Action.Submit", id: "skip", title: "Skip", associatedInputs: "none" },
      ],
    },
  };
  const drawn = renderBlock(card, { onAction: (action) => actions.push(action) });
  const [go, skip] = all(drawn, "button") as HTMLButtonElement[];
  go?.click();
  assert.equal(actions.length, 0);
  const problem = drawn.querySelector(".ac-invalid") as HTMLElement;
  assert.equal(problem.hidden, false);
  assert.equal(problem.textContent, "Tell us your name");
  (drawn.querySelector("input") as HTMLInputElement).value = "Ada";
  go?.click();
  assert.deepEqual(actions[0]?.data, { name: "Ada", step: 2 });
  assert.equal(problem.hidden, true);
  // An action that takes no inputs skips the checks.
  const again = renderBlock(card, { onAction: (action) => actions.push(action) });
  (all(again, "button")[1] as HTMLButtonElement).click();
  assert.deepEqual(actions[1], { type: "submit", id: "skip", verb: undefined, data: {} });
  assert.ok(skip);
});

test("show card, toggle visibility and fallbacks act inside the card", () => {
  const actions: BlockAction[] = [];
  const card = {
    $ui: "adaptive_card",
    version: 1,
    card: {
      type: "AdaptiveCard",
      version: "1.5",
      body: [
        { type: "TextBlock", id: "details", text: "The details", isVisible: false },
        { type: "Media", fallback: { type: "TextBlock", text: "A video" } },
        { type: "Carousel", fallback: "drop" },
      ],
      actions: [
        { type: "Action.ToggleVisibility", title: "Details", targetElements: ["details"] },
        {
          type: "Action.ShowCard",
          title: "Reply",
          card: {
            type: "AdaptiveCard",
            body: [{ type: "Input.Text", id: "reply" }],
            actions: [{ type: "Action.Execute", id: "send", verb: "reply", title: "Send" }],
          },
        },
        { type: "Action.Popover", title: "New kind", fallback: "drop" },
      ],
    },
  };
  const drawn = renderBlock(card, { onAction: (action) => actions.push(action) });
  assert.ok(drawn.textContent.includes("A video"), "the fallback element is drawn");
  const buttons = all(drawn, "button") as HTMLButtonElement[];
  assert.deepEqual(
    buttons.map((button) => button.textContent),
    ["Details", "Reply", "Send"],
  );
  const details = [...drawn.querySelectorAll("p")].find(
    (p) => p.textContent === "The details",
  ) as HTMLElement;
  assert.equal(details.hidden, true);
  buttons[0]?.click();
  assert.equal(details.hidden, false);
  const inner = drawn.querySelector(".ac-showcard") as HTMLElement;
  assert.equal(inner.hidden, true);
  buttons[1]?.click();
  assert.equal(inner.hidden, false);
  (inner.querySelector("input") as HTMLInputElement).value = "On it";
  buttons[2]?.click();
  assert.deepEqual(actions, [{ type: "execute", id: "send", verb: "reply", data: { reply: "On it" } }]);
});

test("a diagram is text until the page brings a renderer, then an image", async () => {
  const block = fixture("diagram-order-flow") as { source: string };
  const asText = renderBlock(block);
  assert.equal(asText.querySelector("pre")?.textContent, block.source);
  assert.equal(all(asText, "img").length, 0);

  const configs: Record<string, unknown>[] = [];
  const mermaid = {
    initialize: (config: Record<string, unknown>) => configs.push(config),
    render: (_id: string, source: string) =>
      Promise.resolve({
        svg: `<svg xmlns="http://www.w3.org/2000/svg"><script>x</script><text>${source.length}</text></svg>`,
      }),
  };
  setDiagramRenderer(mermaidRenderer(mermaid));
  try {
    const drawn = renderBlock(block, { theme: "dark" });
    await new Promise((done) => setTimeout(done, 0));
    const image = drawn.querySelector("img");
    assert.ok(image, "the picture replaced the text");
    // The SVG is an image source: whatever it holds, it is never part of the page.
    assert.match(image.getAttribute("src") ?? "", /^data:image\/svg\+xml;charset=utf-8,/);
    assert.equal(all(drawn, "svg, script").length, 0);
    assert.equal(configs[0]?.securityLevel, "strict");
    assert.equal(configs[0]?.htmlLabels, false);
    assert.equal(configs[0]?.theme, "dark");
    // A renderer that fails leaves the text.
    setDiagramRenderer(() => Promise.reject(new Error("parse error")));
    const kept = renderBlock(block);
    await new Promise((done) => setTimeout(done, 0));
    assert.ok(kept.querySelector("pre"));
  } finally {
    setDiagramRenderer(undefined);
  }
});

test("the element draws its block in a shadow tree and reports actions and errors", () => {
  defineGraphrtUi();
  defineGraphrtUi();
  const element = document.createElement("graphrt-block") as GraphrtBlockElement;
  assert.ok(element instanceof GraphrtBlockElement);
  const actions: BlockAction[] = [];
  const errors: string[] = [];
  document.body.addEventListener("graphrt-action", (event) =>
    actions.push((event as CustomEvent<BlockAction>).detail),
  );
  document.body.addEventListener("graphrt-error", (event) =>
    errors.push((event as CustomEvent<{ message: string }>).detail.message),
  );
  document.body.append(element);
  element.block = fixture("adaptive-card-refund-approval");
  const root = element.shadowRoot;
  assert.ok(root);
  assert.equal(document.body.querySelector("button"), null, "the block's parts stay inside the shadow tree");
  (root.querySelector("button") as HTMLButtonElement).click();
  assert.equal(actions[0]?.id, "approve");
  // A new block replaces the old one.
  element.block = { $ui: "stats", version: 1, items: [{ label: "Orders", value: 42 }] };
  assert.equal(root.querySelectorAll(".block").length, 1);
  assert.ok(root.textContent.includes("Orders"));
  element.block = { $ui: "nope", version: 1 };
  assert.match(errors[0] ?? "", /unknown kind `nope`/);
  assert.ok(root.querySelector(".error"));
  // The block can also be given as an attribute.
  element.setAttribute(
    "data",
    JSON.stringify({ $ui: "stats", version: 1, items: [{ label: "Visits", value: 7 }] }),
  );
  assert.ok(root.textContent.includes("Visits"));
  element.remove();
});

test("light and dark are tokens on the element, and dark can be forced either way", async () => {
  const { BLOCK_CSS } = await import("../src/index.ts");
  assert.match(BLOCK_CSS, /:host \{[^}]*color-scheme: light/);
  assert.match(
    BLOCK_CSS,
    /@media \(prefers-color-scheme: dark\) \{ :host\(:not\(\[theme="light"\]\)\) \{[^}]*color-scheme: dark/,
  );
  assert.match(BLOCK_CSS, /:host\(\[theme="dark"\]\) \{[^}]*color-scheme: dark/);
  // Nothing is loaded from anywhere.
  assert.doesNotMatch(BLOCK_CSS, /url\(|@import|https?:/);
});
