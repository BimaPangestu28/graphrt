/** `table` blocks: columns and rows, scrolling sideways on a narrow screen. */
import { el } from "../dom.js";
import { formatNumber } from "../format.js";
import { isObject } from "../validate/checks.js";
import { frame, text } from "./common.js";
const NUMERIC = ["number", "compact", "percent", "currency"];
function cellText(value, column, locale) {
    if (value === null || value === undefined) {
        return "";
    }
    const format = text(column, "format")?.toLowerCase();
    if (typeof value === "number") {
        return formatNumber(value, { format: format ?? "number", currency: text(column, "currency") }, locale);
    }
    if (typeof value === "boolean") {
        return value ? "✓" : "–";
    }
    if (format === "date" && typeof value === "string") {
        const time = Date.parse(value);
        // A date without a time is that calendar day everywhere.
        const dateOnly = /^\d{4}-\d{2}-\d{2}$/.test(value);
        if (!Number.isNaN(time)) {
            return new Intl.DateTimeFormat(locale, {
                dateStyle: "medium",
                ...(dateOnly ? { timeZone: "UTC" } : { timeStyle: "short" }),
            }).format(time);
        }
    }
    return String(value);
}
export function renderTable(fields, context) {
    const columns = Array.isArray(fields.columns) ? fields.columns.filter(isObject) : [];
    const rows = Array.isArray(fields.rows) ? fields.rows.filter(isObject) : [];
    const alignOf = (column) => {
        const align = text(column, "align")?.toLowerCase();
        const numeric = NUMERIC.includes(text(column, "format")?.toLowerCase() ?? "");
        return align ?? (numeric ? "right" : "left");
    };
    const head = el("tr", {}, ...columns.map((column) => el("th", { scope: "col", class: alignOf(column) }, text(column, "label") ?? "")));
    const body = rows.map((row) => el("tr", {}, ...columns.map((column) => {
        const shown = cellText(row[text(column, "key") ?? ""], column, context.locale);
        const isStatus = text(column, "format")?.toLowerCase() === "status" && shown !== "";
        return el("td", { class: alignOf(column) }, isStatus ? el("span", { class: "status" }, shown) : shown);
    })));
    const table = el("table", {}, el("thead", {}, head), el("tbody", {}, ...body));
    return frame("table-block", fields, el("div", { class: "scroll" }, table));
}
