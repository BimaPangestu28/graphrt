/** `table` blocks. A port of `table.rs` of the `graphrt-ui` crate. */
import { NUMBER_FORMATS } from "./chart.js";
import { array, charCount, child, index, MAX_LONG_TEXT, MAX_SHORT_TEXT, object, onlyKnown, optionalChoice, optionalCurrency, optionalText, requiredName, UiError, } from "./checks.js";
export const MAX_COLUMNS = 20;
export const MAX_ROWS = 1000;
const ALIGNMENTS = ["left", "center", "right"];
const FIELDS = ["title", "subtitle", "fallback_text", "columns", "rows"];
export const COLUMN_FORMATS = ["text", "date", "status", ...NUMBER_FORMATS];
/** @throws UiError - When `fields` (a block without its envelope) is not a table. */
export function validateTable(fields) {
    onlyKnown(fields, "", FIELDS);
    optionalText(fields, "title", "", MAX_SHORT_TEXT);
    optionalText(fields, "subtitle", "", MAX_SHORT_TEXT);
    optionalText(fields, "fallback_text", "", MAX_LONG_TEXT);
    const keys = [];
    array(fields, "columns", "", 1, MAX_COLUMNS).forEach((value, i) => {
        const path = index("/columns", i);
        const column = object(value, path);
        onlyKnown(column, path, ["key", "label", "align", "format", "currency"]);
        const key = requiredName(column, "key", path);
        if (keys.includes(key)) {
            throw new UiError(child(path, "key"), `\`${key}\` is used by another column`);
        }
        keys.push(key);
        requiredName(column, "label", path);
        optionalChoice(column, "align", path, ALIGNMENTS);
        optionalChoice(column, "format", path, COLUMN_FORMATS);
        optionalCurrency(column, "currency", path);
    });
    array(fields, "rows", "", 0, MAX_ROWS).forEach((value, i) => {
        const path = index("/rows", i);
        for (const [key, cell] of Object.entries(object(value, path))) {
            const ok = typeof cell === "string"
                ? charCount(cell) <= MAX_LONG_TEXT
                : cell === null || typeof cell === "number" || typeof cell === "boolean";
            if (!ok) {
                throw new UiError(child(path, key), "must be a string, number, boolean or null");
            }
        }
    });
}
