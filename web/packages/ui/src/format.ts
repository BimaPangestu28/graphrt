/** Number formats of UI blocks: `number`, `compact`, `percent` and `currency`. */

export interface NumberStyle {
  format?: string | undefined;
  currency?: string | undefined;
}

const formatters = new Map<string, Intl.NumberFormat>();

function formatter(
  locale: string | undefined,
  key: string,
  options: Intl.NumberFormatOptions,
): Intl.NumberFormat {
  const cacheKey = `${locale ?? ""}|${key}`;
  let cached = formatters.get(cacheKey);
  if (cached === undefined) {
    try {
      cached = new Intl.NumberFormat(locale, options);
    } catch {
      // An unknown currency code or locale: plain numbers still read fine.
      cached = new Intl.NumberFormat(undefined, { maximumFractionDigits: 2 });
    }
    formatters.set(cacheKey, cached);
  }
  return cached;
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
export function formatNumber(value: number, style: NumberStyle = {}, locale?: string): string {
  const format = style.format?.toLowerCase() ?? "number";
  if (format === "compact") {
    return formatter(locale, "compact", { notation: "compact", maximumFractionDigits: 1 }).format(value);
  }
  if (format === "percent") {
    // Blocks carry percentages as written: 12.5 means 12.5%.
    return `${formatter(locale, "percent", { maximumFractionDigits: 2 }).format(value)}%`;
  }
  if (format === "currency" && style.currency !== undefined) {
    return formatter(locale, `currency:${style.currency}`, {
      style: "currency",
      currency: style.currency,
      maximumFractionDigits: Number.isInteger(value) ? 0 : 2,
    }).format(value);
  }
  return formatter(locale, "number", { maximumFractionDigits: 2 }).format(value);
}
