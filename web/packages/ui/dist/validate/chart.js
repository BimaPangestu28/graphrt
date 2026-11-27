/** `chart` blocks. A port of `chart.rs` of the `graphrt-ui` crate. */
import { array, charCount, child, index, MAX_LONG_TEXT, MAX_SHORT_TEXT, object, onlyKnown, optionalBool, optionalChoice, optionalCurrency, optionalNumber, optionalText, requiredName, requiredText, UiError, } from "./checks.js";
export const CHART_TYPES = ["bar", "line", "area", "pie", "donut"];
export const NUMBER_FORMATS = ["number", "compact", "percent", "currency"];
export const MAX_CATEGORIES = 10_000;
export const MAX_SERIES = 20;
export const MAX_SLICES = 50;
export const MAX_VALUES = 50_000;
const FIELDS = ["title", "subtitle", "fallback_text", "type", "x", "y", "series", "stacked", "horizontal"];
/** @throws UiError - When `fields` (a block without its envelope) is not a chart. */
export function validateChart(fields) {
    onlyKnown(fields, "", FIELDS);
    optionalText(fields, "title", "", MAX_SHORT_TEXT);
    optionalText(fields, "subtitle", "", MAX_SHORT_TEXT);
    optionalText(fields, "fallback_text", "", MAX_LONG_TEXT);
    const chartType = requiredText(fields, "type", "", MAX_SHORT_TEXT);
    if (!CHART_TYPES.includes(chartType)) {
        throw new UiError("/type", `\`${chartType}\` is not one of: ${CHART_TYPES.join(", ")}`);
    }
    const isRound = chartType === "pie" || chartType === "donut";
    const categories = validateX(fields);
    validateY(fields);
    if (optionalBool(fields, "stacked", "") === true && chartType !== "bar" && chartType !== "area") {
        throw new UiError("/stacked", "only bar and area charts can be stacked");
    }
    if (optionalBool(fields, "horizontal", "") === true && chartType !== "bar") {
        throw new UiError("/horizontal", "only bar charts can be horizontal");
    }
    if (isRound && categories > MAX_SLICES) {
        throw new UiError("/x/values", `a ${chartType} chart has at most ${MAX_SLICES} slices`);
    }
    const series = array(fields, "series", "", 1, MAX_SERIES);
    if (isRound && series.length !== 1) {
        throw new UiError("/series", `a ${chartType} chart has exactly one series`);
    }
    if (series.length * categories > MAX_VALUES) {
        throw new UiError("/series", `at most ${MAX_VALUES} values in all`);
    }
    series.forEach((item, i) => {
        validateSeries(item, index("/series", i), categories, isRound);
    });
}
function validateX(fields) {
    if (fields.x === undefined) {
        throw new UiError("", "`x` is required");
    }
    const x = object(fields.x, "/x");
    onlyKnown(x, "/x", ["label", "values"]);
    optionalText(x, "label", "/x", MAX_SHORT_TEXT);
    const values = array(x, "values", "/x", 1, MAX_CATEGORIES);
    values.forEach((value, i) => {
        const ok = typeof value === "number" || (typeof value === "string" && charCount(value) <= MAX_SHORT_TEXT);
        if (!ok) {
            throw new UiError(index("/x/values", i), "must be a short string or a number");
        }
    });
    return values.length;
}
function validateY(fields) {
    if (fields.y === undefined) {
        return;
    }
    const y = object(fields.y, "/y");
    onlyKnown(y, "/y", ["label", "format", "currency", "min", "max"]);
    optionalText(y, "label", "/y", MAX_SHORT_TEXT);
    optionalChoice(y, "format", "/y", NUMBER_FORMATS);
    optionalCurrency(y, "currency", "/y");
    const min = optionalNumber(y, "min", "/y");
    const max = optionalNumber(y, "max", "/y");
    if (min !== undefined && max !== undefined && min >= max) {
        throw new UiError("/y", "`min` must be below `max`");
    }
}
function validateSeries(item, path, categories, isRound) {
    const series = object(item, path);
    onlyKnown(series, path, ["name", "values"]);
    requiredName(series, "name", path);
    const values = array(series, "values", path, 0, MAX_CATEGORIES);
    if (values.length !== categories) {
        throw new UiError(child(path, "values"), `has ${values.length} values but \`x.values\` has ${categories}: give one value (or null) per category`);
    }
    values.forEach((value, i) => {
        const ok = typeof value === "number"
            ? Number.isFinite(value) && (!isRound || value >= 0)
            : value === null && !isRound;
        if (!ok) {
            const expected = isRound ? "a number of 0 or more" : "a number or null";
            throw new UiError(index(child(path, "values"), i), `must be ${expected}`);
        }
    });
}
