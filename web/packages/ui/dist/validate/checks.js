/**
 * Field checks shared by the block kinds. A port of `checks.rs` of the
 * `graphrt-ui` crate, which is the reference: the messages are the same, and
 * both run against the fixtures in `crates/graphrt-ui/fixtures`.
 */
/** A block that failed validation: where (a JSON pointer) and why. */
export class UiError extends Error {
    path;
    reason;
    constructor(path, reason) {
        super(path === "" ? reason : `at ${path}: ${reason}`);
        this.name = "UiError";
        this.path = path;
        this.reason = reason;
    }
}
/** Titles, labels, names. */
export const MAX_SHORT_TEXT = 300;
/** Body text and fallbacks. */
export const MAX_LONG_TEXT = 10_000;
/** A URL; data URLs of small images fit. */
export const MAX_URL = 200 * 1024;
const DATA_IMAGE_PREFIXES = [
    "data:image/png;base64,",
    "data:image/jpeg;base64,",
    "data:image/gif;base64,",
    "data:image/webp;base64,",
];
/** Characters as Rust counts them (`chars().count()`): code points, not UTF-16 units. */
export function charCount(text) {
    let count = 0;
    for (const _ of text) {
        count += 1;
    }
    return count;
}
export function escapePointer(segment) {
    return segment.replaceAll("~", "~0").replaceAll("/", "~1");
}
export function child(path, segment) {
    return `${path}/${escapePointer(segment)}`;
}
export function index(path, i) {
    return `${path}/${i}`;
}
export function isObject(value) {
    return typeof value === "object" && value !== null && !Array.isArray(value);
}
/** @throws UiError - When `value` is not an object. */
export function object(value, path) {
    if (!isObject(value)) {
        throw new UiError(path, "must be an object");
    }
    return value;
}
/** @throws UiError - For a field that is not in `allowed`. */
export function onlyKnown(fields, path, allowed) {
    const unknown = Object.keys(fields).find((key) => !allowed.includes(key));
    if (unknown !== undefined) {
        throw new UiError(child(path, unknown), `unknown field \`${unknown}\` (allowed: ${allowed.join(", ")})`);
    }
}
function textOf(value, path, maxChars) {
    if (typeof value !== "string") {
        throw new UiError(path, "must be a string");
    }
    const chars = charCount(value);
    if (chars > maxChars) {
        throw new UiError(path, `is ${chars} characters long; at most ${maxChars} are allowed`);
    }
    return value;
}
/** @throws UiError - When the field is present and not a string of at most `maxChars`. */
export function optionalText(fields, key, path, maxChars) {
    const value = fields[key];
    return value === undefined || value === null ? undefined : textOf(value, child(path, key), maxChars);
}
/** @throws UiError - When the field is missing or not a string of at most `maxChars`. */
export function requiredText(fields, key, path, maxChars) {
    const value = fields[key];
    if (value === undefined) {
        throw new UiError(path, `\`${key}\` is required`);
    }
    return textOf(value, child(path, key), maxChars);
}
/** A required short text that is not blank. */
export function requiredName(fields, key, path) {
    const name = requiredText(fields, key, path, MAX_SHORT_TEXT);
    if (name.trim() === "") {
        throw new UiError(child(path, key), "must not be empty");
    }
    return name;
}
export function optionalBool(fields, key, path) {
    const value = fields[key];
    if (value === undefined || value === null) {
        return undefined;
    }
    if (typeof value !== "boolean") {
        throw new UiError(child(path, key), "must be true or false");
    }
    return value;
}
export function optionalNumber(fields, key, path) {
    const value = fields[key];
    if (value === undefined || value === null) {
        return undefined;
    }
    if (typeof value !== "number") {
        throw new UiError(child(path, key), "must be a number");
    }
    return value;
}
/** An optional text that is one of `choices`, compared without case. */
export function optionalChoice(fields, key, path, choices) {
    const value = optionalText(fields, key, path, MAX_SHORT_TEXT);
    if (value === undefined) {
        return undefined;
    }
    if (!choices.some((choice) => choice.toLowerCase() === value.toLowerCase())) {
        throw new UiError(child(path, key), `\`${value}\` is not one of: ${choices.join(", ")}`);
    }
    return value;
}
/** An array field with between `min` and `max` items; missing counts as empty when `min` is 0. */
export function array(fields, key, path, min, max) {
    const itemsPath = child(path, key);
    const value = fields[key];
    if ((value === undefined || value === null) && min === 0) {
        return [];
    }
    if (value === undefined) {
        throw new UiError(path, `\`${key}\` is required`);
    }
    if (!Array.isArray(value)) {
        throw new UiError(itemsPath, "must be an array");
    }
    if (value.length < min) {
        throw new UiError(itemsPath, `needs at least ${min} item(s)`);
    }
    if (value.length > max) {
        throw new UiError(itemsPath, `has ${value.length} items; at most ${max} are allowed`);
    }
    return value;
}
export function optionalCurrency(fields, key, path) {
    const code = optionalText(fields, key, path, MAX_SHORT_TEXT);
    if (code !== undefined && !/^[A-Z]{3}$/.test(code)) {
        throw new UiError(child(path, key), "must be a 3-letter ISO 4217 code such as IDR or USD");
    }
}
function hasWhitespaceOrControl(text) {
    // Rust's `char::is_whitespace` and `char::is_control`.
    return /[\s\u0085\p{Cc}]/u.test(text);
}
/** Whether `url` is `https://host...` without user info, spaces or control characters. */
export function isHttps(url) {
    if (url.slice(0, 8).toLowerCase() !== "https://") {
        return false;
    }
    const host = url.slice(8).split(/[/?#]/)[0] ?? "";
    return host !== "" && !host.includes("@") && !hasWhitespaceOrControl(url);
}
/** Whether an image URL may be shown: `https`, or a `data:` URL of a raster image. */
export function isImageUrl(url) {
    const lower = url.slice(0, 32).toLowerCase();
    return DATA_IMAGE_PREFIXES.some((prefix) => lower.startsWith(prefix)) || isHttps(url);
}
/** Whether a link may be opened: `https` or `mailto`. */
export function isLinkUrl(url) {
    const lower = url.slice(0, 8).toLowerCase();
    return charCount(url) <= 2048 && (isHttps(url) || (lower.startsWith("mailto:") && url.length > 7));
}
/** @throws UiError - For an image URL that is not allowed. */
export function checkImageUrl(url, path) {
    if (charCount(url) > MAX_URL) {
        throw new UiError(path, "the image URL is too long; use a smaller image");
    }
    if (!isImageUrl(url)) {
        throw new UiError(path, "images must be https URLs or data:image/(png|jpeg|gif|webp);base64 URLs");
    }
}
/** @throws UiError - For a link that is not allowed. */
export function checkLinkUrl(url, path) {
    if (!isLinkUrl(url)) {
        throw new UiError(path, "links must be https or mailto URLs");
    }
}
