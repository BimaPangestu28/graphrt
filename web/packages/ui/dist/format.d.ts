/** Number formats of UI blocks: `number`, `compact`, `percent` and `currency`. */
export interface NumberStyle {
    format?: string | undefined;
    currency?: string | undefined;
}
/**
 * A number as a block asks for it.
 *
 * @param style - `format` (compared without case) and, for `currency`, an ISO 4217 code.
 * @param locale - A BCP 47 tag. Default: the browser's.
 *
 * @example
 * formatNumber(125000000, { format: "currency", currency: "IDR" }, "id-ID"); // "Rp 125.000.000"
 * formatNumber(12.5, { format: "percent" }, "en"); // "12.5%"
 */
export declare function formatNumber(value: number, style?: NumberStyle, locale?: string): string;
