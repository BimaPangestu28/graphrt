/**
 * DOM building without markup strings. Text reaches the page only through
 * `textContent` and attribute setters, so nothing in a block is ever parsed as
 * HTML: escaping is by construction, not by a sanitizer.
 */
export type Child = Node | string | null | undefined | false;
/**
 * An HTML element with attributes and children.
 *
 * @example
 * el("p", { class: "caption" }, userText); // userText is text, whatever it holds
 */
export declare function el<K extends keyof HTMLElementTagNameMap>(tag: K, attributes?: Record<string, string | undefined>, ...children: Child[]): HTMLElementTagNameMap[K];
/** An SVG element with attributes and children. */
export declare function svg(tag: string, attributes?: Record<string, string | number | undefined>, ...children: Child[]): SVGElement;
/** A number for an SVG attribute: two decimals are plenty for a chart. */
export declare function px(value: number): string;
