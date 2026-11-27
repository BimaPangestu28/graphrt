/** What every block renderer shares: the frame with title and subtitle. */
import { el } from "../dom.js";
export function text(fields, key) {
    const value = fields[key];
    return typeof value === "string" && value !== "" ? value : undefined;
}
/** The frame of a block: its title and subtitle above `content`. */
export function frame(kind, fields, ...content) {
    const title = text(fields, "title");
    const subtitle = text(fields, "subtitle");
    const head = title !== undefined || subtitle !== undefined
        ? el("div", { class: "head" }, title !== undefined && el("h3", { class: "title" }, title), subtitle !== undefined && el("p", { class: "subtitle" }, subtitle))
        : null;
    return el("section", { class: `block ${kind}` }, head, ...content);
}
