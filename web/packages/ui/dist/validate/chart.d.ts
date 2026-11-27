/** `chart` blocks. A port of `chart.rs` of the `graphrt-ui` crate. */
import { type Fields } from "./checks.ts";
export declare const CHART_TYPES: readonly ["bar", "line", "area", "pie", "donut"];
export declare const NUMBER_FORMATS: readonly ["number", "compact", "percent", "currency"];
export declare const MAX_CATEGORIES = 10000;
export declare const MAX_SERIES = 20;
export declare const MAX_SLICES = 50;
export declare const MAX_VALUES = 50000;
/** @throws UiError - When `fields` (a block without its envelope) is not a chart. */
export declare function validateChart(fields: Fields): void;
