import { type Fields } from "./checks.ts";
export { type Fields, type Json, UiError } from "./checks.ts";
/** Key that marks an object as a UI block; its value is the block kind. */
export declare const UI_KEY = "$ui";
/** The block format version this package renders. */
export declare const VERSION = 1;
/** Largest block, in bytes of JSON. */
export declare const MAX_BLOCK_BYTES: number;
/** Most blocks {@link collectUiBlocks} returns for one value. */
export declare const MAX_COLLECTED_BLOCKS = 32;
export declare const UI_KINDS: readonly ["adaptive_card", "chart", "table", "stats", "diagram"];
export type UiKind = (typeof UI_KINDS)[number];
/** A block that passed {@link validate}. */
export interface UiBlock {
    readonly kind: UiKind;
    /** The block as it was given, envelope included. */
    readonly value: Fields;
    /** The block's fields without `$ui` and `version`. */
    readonly body: Fields;
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
export declare function validate(value: unknown): UiBlock;
/** The kind a value claims to be (`"chart"`...), or `undefined` when it is not a block. */
export declare function uiBlockKind(value: unknown): string | undefined;
/**
 * The UI blocks in `value`, in document order: the value itself when it is a
 * block, otherwise the blocks in its array items and object fields, at most
 * three levels down. The inside of a block is not searched. The same rule as
 * `graphrt_core::ui::collect_ui_blocks`.
 *
 * @example
 * collectUiBlocks({ text: "done", data: { $ui: "stats", version: 1, items: [] } }).length; // 1
 */
export declare function collectUiBlocks(value: unknown): unknown[];
