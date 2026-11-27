import { type Fields } from "./checks.ts";
export declare const MAX_ITEMS = 12;
/** @throws UiError - When `fields` (a block without its envelope) is not a stats block. */
export declare function validateStats(fields: Fields): void;
