import type { BlockAction, DiagramRenderer } from "./render/common.ts";
import { UiError } from "./validate/index.ts";
/**
 * Draw `diagram` blocks with `renderer` on this page (see `mermaidRenderer`),
 * or show their source as text again with `undefined`. Blocks drawn before the
 * call keep how they look.
 */
export declare function setDiagramRenderer(renderer: DiagramRenderer | undefined): void;
export interface RenderOptions {
    /** A BCP 47 tag for numbers and dates. Default: the browser's. */
    locale?: string | undefined;
    /** Called when a person presses a card action that sends data. */
    onAction?: (action: BlockAction) => void;
    /** The theme diagrams are drawn for. Default: `light`. */
    theme?: "light" | "dark";
    /** Called with the reason when the block is not valid. */
    onError?: (error: UiError) => void;
}
/**
 * Validate `value` and draw it. An invalid block is never drawn: its
 * `fallback_text` (or a short notice) and the reason are shown instead.
 * The result needs the styles of this package ({@link BLOCK_CSS}), which
 * `<graphrt-block>` brings itself.
 */
export declare function renderBlock(value: unknown, options?: RenderOptions): HTMLElement;
/** Put `css` on a shadow root: a constructed sheet where the browser has them, else a style element. */
export declare function adoptStyles(root: ShadowRoot, css: string): void;
declare const BaseElement: typeof HTMLElement;
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
export declare class GraphrtBlockElement extends BaseElement {
    #private;
    static readonly observedAttributes: string[];
    /** The block to draw: any JSON value. It is validated before it is drawn. */
    get block(): unknown;
    set block(value: unknown);
    connectedCallback(): void;
    attributeChangedCallback(name: string, _old: string | null, value: string | null): void;
}
/**
 * Register `<graphrt-block>`. Safe to call more than once, and a no-op where
 * there are no custom elements (a server).
 */
export declare function defineGraphrtUi(): void;
export {};
