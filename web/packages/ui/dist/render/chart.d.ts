import { type Fields } from "../validate/checks.ts";
import { type RenderContext } from "./common.ts";
/** Round tick values that cover `[min, max]`. */
export declare function ticks(min: number, max: number, count?: number): number[];
export declare function renderChart(fields: Fields, context: RenderContext): HTMLElement;
