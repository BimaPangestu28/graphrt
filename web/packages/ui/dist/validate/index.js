/**
 * Validation of UI blocks: the gate before anything is drawn.
 *
 * A port of `graphrt_ui::validate` (the `graphrt-ui` crate), which is the
 * reference. Both implementations run against the same fixtures
 * (`crates/graphrt-ui/fixtures`), so a block one accepts the other accepts too.
 */
import { validateAdaptiveCard } from "./adaptive_card.js";
import { validateChart } from "./chart.js";
import { escapePointer, isObject, UiError } from "./checks.js";
import { validateDiagram } from "./diagram.js";
import { validateStats } from "./stats.js";
import { validateTable } from "./table.js";
export { UiError } from "./checks.js";
/** Key that marks an object as a UI block; its value is the block kind. */
export const UI_KEY = "$ui";
/** The block format version this package renders. */
export const VERSION = 1;
/** Largest block, in bytes of JSON. */
export const MAX_BLOCK_BYTES = 256 * 1024;
/** Most blocks {@link collectUiBlocks} returns for one value. */
export const MAX_COLLECTED_BLOCKS = 32;
const MAX_SCAN_DEPTH = 3;
export const UI_KINDS = ["adaptive_card", "chart", "table", "stats", "diagram"];
const VALIDATORS = {
    adaptive_card: validateAdaptiveCard,
    chart: validateChart,
    table: validateTable,
    stats: validateStats,
    diagram: validateDiagram,
};
function isUiKind(name) {
    return UI_KINDS.includes(name);
}
/**
 * Check a UI block: its envelope, its size and every field of its kind.
 *
 * @param value - Any JSON value, e.g. from a run's outputs.
 * @throws UiError - With the JSON pointer of the offending field and the reason.
 *
 * @example
 * const block = validate({ $ui: "stats", version: 1, items: [{ label: "Orders", value: 42 }] });
 * block.kind; // "stats"
 */
export function validate(value) {
    const fields = value;
    if (!isObject(fields)) {
        throw new UiError("", "a UI block must be a JSON object");
    }
    const kindName = fields[UI_KEY];
    if (typeof kindName !== "string") {
        throw new UiError("", `a UI block needs a string \`${UI_KEY}\` naming its kind`);
    }
    if (!isUiKind(kindName)) {
        throw new UiError(`/${escapePointer(UI_KEY)}`, `unknown kind \`${kindName}\` (known: ${UI_KINDS.join(", ")})`);
    }
    if (fields.version !== VERSION) {
        throw new UiError("/version", `\`version\` must be ${VERSION}`);
    }
    const size = new TextEncoder().encode(JSON.stringify(fields)).length;
    if (size > MAX_BLOCK_BYTES) {
        throw new UiError("", `the block is ${size} bytes; at most ${MAX_BLOCK_BYTES} are allowed`);
    }
    const body = {};
    for (const [key, field] of Object.entries(fields)) {
        if (key !== UI_KEY && key !== "version") {
            body[key] = field;
        }
    }
    VALIDATORS[kindName](body);
    return { kind: kindName, value: fields, body };
}
/** The kind a value claims to be (`"chart"`...), or `undefined` when it is not a block. */
export function uiBlockKind(value) {
    const fields = value;
    const kind = isObject(fields) ? fields[UI_KEY] : undefined;
    return typeof kind === "string" ? kind : undefined;
}
/**
 * The UI blocks in `value`, in document order: the value itself when it is a
 * block, otherwise the blocks in its array items and object fields, at most
 * three levels down. The inside of a block is not searched. The same rule as
 * `graphrt_core::ui::collect_ui_blocks`.
 *
 * @example
 * collectUiBlocks({ text: "done", data: { $ui: "stats", version: 1, items: [] } }).length; // 1
 */
export function collectUiBlocks(value) {
    const blocks = [];
    const collect = (candidate, depth) => {
        if (blocks.length >= MAX_COLLECTED_BLOCKS) {
            return;
        }
        if (uiBlockKind(candidate) !== undefined) {
            blocks.push(candidate);
            return;
        }
        if (depth >= MAX_SCAN_DEPTH || typeof candidate !== "object" || candidate === null) {
            return;
        }
        for (const item of Array.isArray(candidate) ? candidate : Object.values(candidate)) {
            collect(item, depth + 1);
        }
    };
    collect(value, 0);
    return blocks;
}
