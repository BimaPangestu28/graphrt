/** `stats` blocks. A port of `stats.rs` of the `graphrt-ui` crate. */
import { NUMBER_FORMATS } from "./chart.ts";
import {
  array,
  charCount,
  child,
  type Fields,
  index,
  MAX_LONG_TEXT,
  MAX_SHORT_TEXT,
  object,
  onlyKnown,
  optionalChoice,
  optionalCurrency,
  optionalNumber,
  optionalText,
  requiredName,
  UiError,
} from "./checks.ts";

export const MAX_ITEMS = 12;
const DIRECTIONS = ["up", "down"];
const TRENDS = ["up", "down", "flat"];
const ITEM_FIELDS = ["label", "value", "format", "currency", "delta", "delta_format", "trend", "good", "caption"];

/** @throws UiError - When `fields` (a block without its envelope) is not a stats block. */
export function validateStats(fields: Fields): void {
  onlyKnown(fields, "", ["title", "subtitle", "fallback_text", "items"]);
  optionalText(fields, "title", "", MAX_SHORT_TEXT);
  optionalText(fields, "subtitle", "", MAX_SHORT_TEXT);
  optionalText(fields, "fallback_text", "", MAX_LONG_TEXT);
  array(fields, "items", "", 1, MAX_ITEMS).forEach((value, i) => {
    const path = index("/items", i);
    const item = object(value, path);
    onlyKnown(item, path, ITEM_FIELDS);
    requiredName(item, "label", path);
    const shown = item.value;
    const ok = typeof shown === "number" || (typeof shown === "string" && charCount(shown) <= 60);
    if (!ok) {
      throw new UiError(child(path, "value"), "must be a number or a string of at most 60 characters");
    }
    optionalChoice(item, "format", path, NUMBER_FORMATS);
    optionalCurrency(item, "currency", path);
    optionalNumber(item, "delta", path);
    optionalChoice(item, "delta_format", path, ["number", "percent"]);
    optionalChoice(item, "trend", path, TRENDS);
    optionalChoice(item, "good", path, DIRECTIONS);
    optionalText(item, "caption", path, MAX_SHORT_TEXT);
  });
}
