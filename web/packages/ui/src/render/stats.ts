/** `stats` blocks: tiles with a value, its change and a caption. */
import { el } from "../dom.ts";
import { formatNumber } from "../format.ts";
import { type Fields, isObject } from "../validate/checks.ts";
import { frame, type RenderContext, text } from "./common.ts";

/** The arrow and whether the change is good news, for a tile's delta. */
function deltaOf(item: Fields, locale: string | undefined): { label: string; tone: string } | undefined {
  const delta = item.delta;
  if (typeof delta !== "number") {
    return undefined;
  }
  const trend = text(item, "trend")?.toLowerCase() ?? (delta > 0 ? "up" : delta < 0 ? "down" : "flat");
  const good = text(item, "good")?.toLowerCase() ?? "up";
  const tone = trend === "flat" ? "" : trend === good ? "good" : "bad";
  const arrow = trend === "up" ? "▲" : trend === "down" ? "▼" : "•";
  const amount = formatNumber(Math.abs(delta), { format: text(item, "delta_format") }, locale);
  const sign = delta > 0 ? "+" : delta < 0 ? "-" : "";
  return { label: `${arrow} ${sign}${amount}`, tone };
}

export function renderStats(fields: Fields, context: RenderContext): HTMLElement {
  const items = Array.isArray(fields.items) ? fields.items.filter(isObject) : [];
  const tiles = items.map((item) => {
    const value =
      typeof item.value === "number"
        ? formatNumber(item.value, { format: text(item, "format"), currency: text(item, "currency") }, context.locale)
        : String(item.value ?? "");
    const delta = deltaOf(item, context.locale);
    const caption = text(item, "caption");
    return el(
      "div",
      { class: "stat" },
      el("div", { class: "stat-label" }, text(item, "label") ?? ""),
      el("div", { class: "stat-value" }, value),
      delta !== undefined && el("span", { class: `stat-delta ${delta.tone}` }, delta.label),
      delta !== undefined && caption !== undefined && " ",
      caption !== undefined && el("span", { class: "stat-caption" }, caption),
    );
  });
  return frame("stats-block", fields, el("div", { class: "stats" }, ...tiles));
}
