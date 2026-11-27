/**
 * Field checks shared by the block kinds. A port of `checks.rs` of the
 * `graphrt-ui` crate, which is the reference: the messages are the same, and
 * both run against the fixtures in `crates/graphrt-ui/fixtures`.
 */
/** JSON as a block holds it. */
export type Json = null | boolean | number | string | Json[] | {
    [key: string]: Json;
};
export type Fields = {
    [key: string]: Json;
};
/** A block that failed validation: where (a JSON pointer) and why. */
export declare class UiError extends Error {
    readonly path: string;
    readonly reason: string;
    constructor(path: string, reason: string);
}
/** Titles, labels, names. */
export declare const MAX_SHORT_TEXT = 300;
/** Body text and fallbacks. */
export declare const MAX_LONG_TEXT = 10000;
/** A URL; data URLs of small images fit. */
export declare const MAX_URL: number;
/** Characters as Rust counts them (`chars().count()`): code points, not UTF-16 units. */
export declare function charCount(text: string): number;
export declare function escapePointer(segment: string): string;
export declare function child(path: string, segment: string): string;
export declare function index(path: string, i: number): string;
export declare function isObject(value: Json | undefined): value is Fields;
/** @throws UiError - When `value` is not an object. */
export declare function object(value: Json | undefined, path: string): Fields;
/** @throws UiError - For a field that is not in `allowed`. */
export declare function onlyKnown(fields: Fields, path: string, allowed: readonly string[]): void;
/** @throws UiError - When the field is present and not a string of at most `maxChars`. */
export declare function optionalText(fields: Fields, key: string, path: string, maxChars: number): string | undefined;
/** @throws UiError - When the field is missing or not a string of at most `maxChars`. */
export declare function requiredText(fields: Fields, key: string, path: string, maxChars: number): string;
/** A required short text that is not blank. */
export declare function requiredName(fields: Fields, key: string, path: string): string;
export declare function optionalBool(fields: Fields, key: string, path: string): boolean | undefined;
export declare function optionalNumber(fields: Fields, key: string, path: string): number | undefined;
/** An optional text that is one of `choices`, compared without case. */
export declare function optionalChoice(fields: Fields, key: string, path: string, choices: readonly string[]): string | undefined;
/** An array field with between `min` and `max` items; missing counts as empty when `min` is 0. */
export declare function array(fields: Fields, key: string, path: string, min: number, max: number): Json[];
export declare function optionalCurrency(fields: Fields, key: string, path: string): void;
/** Whether `url` is `https://host...` without user info, spaces or control characters. */
export declare function isHttps(url: string): boolean;
/** Whether an image URL may be shown: `https`, or a `data:` URL of a raster image. */
export declare function isImageUrl(url: string): boolean;
/** Whether a link may be opened: `https` or `mailto`. */
export declare function isLinkUrl(url: string): boolean;
/** @throws UiError - For an image URL that is not allowed. */
export declare function checkImageUrl(url: string, path: string): void;
/** @throws UiError - For a link that is not allowed. */
export declare function checkLinkUrl(url: string, path: string): void;
