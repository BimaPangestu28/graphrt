/**
 * DOM building without markup strings. Text reaches the page only through
 * `textContent` and attribute setters, so nothing in a block is ever parsed as
 * HTML: escaping is by construction, not by a sanitizer.
 */

export type Child = Node | string | null | undefined | false;

const SVG_NS = "http://www.w3.org/2000/svg";

function append(parent: Node, children: readonly Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) {
      continue;
    }
    parent.appendChild(typeof child === "string" ? document.createTextNode(child) : child);
  }
}

/**
 * An HTML element with attributes and children.
 *
 * @example
 * el("p", { class: "caption" }, userText); // userText is text, whatever it holds
 */
export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attributes: Record<string, string | undefined> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (value !== undefined) {
      element.setAttribute(name, value);
    }
  }
  append(element, children);
  return element;
}

/** An SVG element with attributes and children. */
export function svg(
  tag: string,
  attributes: Record<string, string | number | undefined> = {},
  ...children: Child[]
): SVGElement {
  const element = document.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (value !== undefined) {
      element.setAttribute(name, String(value));
    }
  }
  append(element, children);
  return element;
}

/** A number for an SVG attribute: two decimals are plenty for a chart. */
export function px(value: number): string {
  return String(Math.round(value * 100) / 100);
}
