/**
 * `@graphrt/ui`: web components that draw graphrt UI blocks (Adaptive Cards,
 * charts, tables, stat tiles and diagrams), light and dark.
 *
 * Blocks come from models and tools, so they are untrusted: every block is
 * validated before it is drawn, all text reaches the page as text, and no code
 * or style is loaded from anywhere.
 */
export {
  adoptStyles,
  defineGraphrtUi,
  GraphrtBlockElement,
  type RenderOptions,
  renderBlock,
  setDiagramRenderer,
} from "./element.ts";
export { formatNumber, type NumberStyle } from "./format.ts";
export type { BlockAction, DiagramRenderer } from "./render/common.ts";
export { type MermaidLike, mermaidRenderer } from "./render/diagram.ts";
export { BLOCK_CSS, TOKENS_CSS } from "./styles.ts";
export {
  collectUiBlocks,
  MAX_BLOCK_BYTES,
  UI_KEY,
  UI_KINDS,
  type UiBlock,
  UiError,
  type UiKind,
  uiBlockKind,
  validate,
} from "./validate/index.ts";
