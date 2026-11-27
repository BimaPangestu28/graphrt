/**
 * `@graphrt/ui`: web components that draw graphrt UI blocks (Adaptive Cards,
 * charts, tables, stat tiles and diagrams), light and dark.
 *
 * Blocks come from models and tools, so they are untrusted: every block is
 * validated before it is drawn, all text reaches the page as text, and no code
 * or style is loaded from anywhere.
 */
export { adoptStyles, defineGraphrtUi, GraphrtBlockElement, renderBlock, setDiagramRenderer, } from "./element.js";
export { formatNumber } from "./format.js";
export { mermaidRenderer } from "./render/diagram.js";
export { BLOCK_CSS, TOKENS_CSS } from "./styles.js";
export { collectUiBlocks, MAX_BLOCK_BYTES, UI_KEY, UI_KINDS, UiError, uiBlockKind, validate, } from "./validate/index.js";
