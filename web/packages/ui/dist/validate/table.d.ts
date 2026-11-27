import { type Fields } from "./checks.ts";
export declare const MAX_COLUMNS = 20;
export declare const MAX_ROWS = 1000;
export declare const COLUMN_FORMATS: string[];
/** @throws UiError - When `fields` (a block without its envelope) is not a table. */
export declare function validateTable(fields: Fields): void;
