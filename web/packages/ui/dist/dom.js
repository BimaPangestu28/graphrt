/**
 * DOM building without markup strings. Text reaches the page only through
 * `textContent` and attribute setters, so nothing in a block is ever parsed as
 * HTML: escaping is by construction, not by a sanitizer.
 */
const SVG_NS = "http://www.w3.org/2000/svg";
function append(parent, children) {
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
export function el(tag, attributes = {}, ...children) {
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
export function svg(tag, attributes = {}, ...children) {
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
export function px(value) {
    return String(Math.round(value * 100) / 100);
}
