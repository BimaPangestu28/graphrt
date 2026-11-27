/**
 * `<graphrt-block>`: a custom element that draws one UI block.
 *
 * @example
 * import { defineGraphrtUi } from "@graphrt/ui";
 * defineGraphrtUi();
 * const element = document.createElement("graphrt-block");
 * element.block = run.outputs.report;
 * element.addEventListener("graphrt-action", (event) => resume(event.detail));
 */
import { el } from "./dom.js";
import { renderCard } from "./render/card.js";
import { renderChart } from "./render/chart.js";
import { renderDiagram } from "./render/diagram.js";
import { renderStats } from "./render/stats.js";
import { renderTable } from "./render/table.js";
import { BLOCK_CSS } from "./styles.js";
import { isObject } from "./validate/checks.js";
import { UiError, validate } from "./validate/index.js";
let pageDiagramRenderer;
/**
 * Draw `diagram` blocks with `renderer` on this page (see `mermaidRenderer`),
 * or show their source as text again with `undefined`. Blocks drawn before the
 * call keep how they look.
 */
export function setDiagramRenderer(renderer) {
    pageDiagramRenderer = renderer;
}
/** What is shown in place of a block that cannot be drawn. */
function fallback(value, error) {
    const fields = value;
    const fallbackText = isObject(fields) && typeof fields.fallback_text === "string" ? fields.fallback_text : undefined;
    return el("section", { class: "block error" }, fallbackText !== undefined
        ? el("pre", {}, fallbackText)
        : el("p", { class: "muted" }, "This content cannot be shown."), el("small", {}, error.message));
}
function draw(block, context, theme) {
    switch (block.kind) {
        case "stats":
            return renderStats(block.body, context);
        case "table":
            return renderTable(block.body, context);
        case "chart":
            return renderChart(block.body, context);
        case "diagram":
            return renderDiagram(block.body, context, theme);
        case "adaptive_card":
            return renderCard(block.body, context);
    }
}
/**
 * Validate `value` and draw it. An invalid block is never drawn: its
 * `fallback_text` (or a short notice) and the reason are shown instead.
 * The result needs the styles of this package ({@link BLOCK_CSS}), which
 * `<graphrt-block>` brings itself.
 */
export function renderBlock(value, options = {}) {
    let block;
    try {
        block = validate(value);
    }
    catch (error) {
        const reason = error instanceof UiError ? error : new UiError("", String(error));
        options.onError?.(reason);
        return fallback(value, reason);
    }
    const context = {
        locale: options.locale,
        onAction: options.onAction ?? (() => undefined),
        diagram: pageDiagramRenderer,
    };
    return draw(block, context, options.theme ?? "light");
}
/** Put `css` on a shadow root: a constructed sheet where the browser has them, else a style element. */
export function adoptStyles(root, css) {
    try {
        const sheet = new CSSStyleSheet();
        sheet.replaceSync(css);
        root.adoptedStyleSheets = [...root.adoptedStyleSheets, sheet];
    }
    catch {
        root.append(el("style", {}, css));
    }
}
const BaseElement = globalThis.HTMLElement ?? class {
};
/**
 * Draws the block in its `block` property (or the JSON in its `data`
 * attribute). Attribute `theme`: `light`, `dark`, or absent to follow the
 * page. Attribute `locale`: a BCP 47 tag.
 *
 * Events, both bubbling out of shadow trees:
 * - `graphrt-action` (`detail`: {@link BlockAction}) when a person presses a
 *   card action that sends data;
 * - `graphrt-error` (`detail`: `{ path, message }`) for a block that is not valid.
 */
export class GraphrtBlockElement extends BaseElement {
    static observedAttributes = ["data", "theme", "locale"];
    #block;
    #root;
    #content;
    /** The block to draw: any JSON value. It is validated before it is drawn. */
    get block() {
        return this.#block;
    }
    set block(value) {
        this.#block = value;
        this.#render();
    }
    connectedCallback() {
        if (this.#root === undefined) {
            this.#root = this.attachShadow({ mode: "open" });
            adoptStyles(this.#root, BLOCK_CSS);
        }
        // Moved within the page, it keeps what it shows: a form half filled in stays so.
        if (this.#content === undefined) {
            this.#render();
        }
    }
    attributeChangedCallback(name, _old, value) {
        if (name === "data" && value !== null) {
            try {
                this.#block = JSON.parse(value);
            }
            catch {
                this.#block = value;
            }
        }
        this.#render();
    }
    #theme() {
        const theme = this.getAttribute("theme");
        if (theme === "light" || theme === "dark") {
            return theme;
        }
        const prefersDark = typeof matchMedia === "function" && matchMedia("(prefers-color-scheme: dark)").matches;
        return prefersDark ? "dark" : "light";
    }
    #render() {
        if (this.#root === undefined || this.#block === undefined) {
            return;
        }
        const content = renderBlock(this.#block, {
            locale: this.getAttribute("locale") ?? undefined,
            theme: this.#theme(),
            onAction: (detail) => {
                this.dispatchEvent(new CustomEvent("graphrt-action", { detail, bubbles: true, composed: true }));
            },
            onError: (error) => {
                const detail = { path: error.path, message: error.message };
                this.dispatchEvent(new CustomEvent("graphrt-error", { detail, bubbles: true, composed: true }));
            },
        });
        if (this.#content === undefined) {
            this.#root.append(content);
        }
        else {
            this.#content.replaceWith(content);
        }
        this.#content = content;
    }
}
/**
 * Register `<graphrt-block>`. Safe to call more than once, and a no-op where
 * there are no custom elements (a server).
 */
export function defineGraphrtUi() {
    if (typeof customElements !== "undefined" && customElements.get("graphrt-block") === undefined) {
        customElements.define("graphrt-block", GraphrtBlockElement);
    }
}
