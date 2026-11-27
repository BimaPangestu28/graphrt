/**
 * `chart` blocks as inline SVG: bar (grouped, stacked, horizontal), line, area,
 * pie and donut. No chart library: the marks are few and simple.
 *
 * Reading aids that do not depend on colour: a legend for two or more series,
 * the value of every mark in its tooltip, and the data as a table below the
 * picture.
 */
import { el, px, svg } from "../dom.ts";
import { formatNumber, type NumberStyle } from "../format.ts";
import { SERIES_COLOURS } from "../styles.ts";
import { type Fields, isObject } from "../validate/checks.ts";
import { frame, type RenderContext, text } from "./common.ts";

interface Series {
  name: string;
  values: (number | null)[];
}

interface ChartData {
  type: string;
  categories: string[];
  series: Series[];
  stacked: boolean;
  horizontal: boolean;
  style: NumberStyle;
  min: number | undefined;
  max: number | undefined;
  xLabel: string | undefined;
  yLabel: string | undefined;
}

const WIDTH = 640;
const HEIGHT = 300;
/** Thickest bar: beyond it the band's leftover is air. */
const MAX_BAR = 24;

function seriesClass(i: number): string {
  return `s${(i % SERIES_COLOURS) + 1}`;
}

function read(fields: Fields): ChartData {
  const x = isObject(fields.x) ? fields.x : {};
  const y = isObject(fields.y) ? fields.y : {};
  const categories = (Array.isArray(x.values) ? x.values : []).map((value) => String(value));
  const series = (Array.isArray(fields.series) ? fields.series : []).filter(isObject).map((item) => ({
    name: text(item, "name") ?? "",
    values: (Array.isArray(item.values) ? item.values : []).map((value) => (typeof value === "number" ? value : null)),
  }));
  return {
    type: text(fields, "type") ?? "bar",
    categories,
    series,
    stacked: fields.stacked === true,
    horizontal: fields.horizontal === true,
    style: { format: text(y, "format"), currency: text(y, "currency") },
    min: typeof y.min === "number" ? y.min : undefined,
    max: typeof y.max === "number" ? y.max : undefined,
    xLabel: text(x, "label"),
    yLabel: text(y, "label"),
  };
}

/** Round tick values that cover `[min, max]`. */
export function ticks(min: number, max: number, count = 5): number[] {
  if (!(max > min)) {
    return [min];
  }
  const rough = (max - min) / count;
  const power = 10 ** Math.floor(Math.log10(rough));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * power).find((candidate) => candidate >= rough) ?? rough;
  const first = Math.floor(min / step) * step;
  // As many steps as it takes to reach `max`, counted so rounding cannot drop the last one.
  const steps = Math.ceil((max - first) / step - 1e-9);
  return Array.from({ length: steps + 1 }, (_, i) => Math.round((first + i * step) / step) * step);
}

/** The value range the marks need: stacks add up, and bars and areas start at zero. */
function valueRange(data: ChartData): [number, number] {
  let low = 0;
  let high = 0;
  const anchored = data.type !== "line";
  if (data.stacked) {
    for (let c = 0; c < data.categories.length; c += 1) {
      let positive = 0;
      let negative = 0;
      for (const item of data.series) {
        const value = item.values[c] ?? 0;
        if (value >= 0) {
          positive += value;
        } else {
          negative += value;
        }
      }
      high = Math.max(high, positive);
      low = Math.min(low, negative);
    }
  } else {
    const all = data.series.flatMap((item) => item.values).filter((value): value is number => value !== null);
    high = all.length > 0 ? Math.max(...all) : 1;
    low = all.length > 0 ? Math.min(...all) : 0;
    if (anchored) {
      low = Math.min(low, 0);
      high = Math.max(high, 0);
    }
  }
  if (low === high) {
    high = low + 1;
  }
  return [data.min ?? low, data.max ?? high];
}

function shortLabel(label: string, max: number): string {
  return label.length > max ? `${label.slice(0, max - 1)}…` : label;
}

/** Bar, line and area charts on one value axis. */
function cartesian(data: ChartData, locale: string | undefined): SVGElement {
  const [low, high] = valueRange(data);
  const marks = ticks(low, high);
  const lo = Math.min(low, marks[0] ?? low);
  const hi = Math.max(high, marks.at(-1) ?? high);
  const format = (value: number): string => formatNumber(value, data.style, locale);
  const horizontal = data.horizontal && data.type === "bar";
  const longestTick = Math.max(...marks.map((value) => format(value).length), 1);
  const longestCategory = Math.max(...data.categories.map((category) => category.length), 1);
  const left = horizontal ? Math.min(160, 12 + longestCategory * 6.2) : 14 + longestTick * 6.2;
  const plot = { left, top: 10, right: WIDTH - 12, bottom: HEIGHT - (data.xLabel ? 44 : 28) };
  const span = hi - lo;
  // Value to pixel along the value axis, and band start along the category axis.
  const valueAt = (value: number): number =>
    horizontal
      ? plot.left + ((value - lo) / span) * (plot.right - plot.left)
      : plot.bottom - ((value - lo) / span) * (plot.bottom - plot.top);
  const bandSize = (horizontal ? plot.bottom - plot.top : plot.right - plot.left) / data.categories.length;
  const bandAt = (c: number): number => (horizontal ? plot.top : plot.left) + c * bandSize;
  const root = svg("svg", { viewBox: `0 0 ${WIDTH} ${HEIGHT}`, role: "img" });

  for (const value of marks) {
    const at = px(valueAt(value));
    if (horizontal) {
      root.append(
        svg("line", { class: "grid", x1: at, x2: at, y1: plot.top, y2: plot.bottom }),
        svg("text", { x: at, y: plot.bottom + 14, "text-anchor": "middle" }, format(value)),
      );
    } else {
      root.append(
        svg("line", { class: "grid", x1: plot.left, x2: plot.right, y1: at, y2: at }),
        svg("text", { x: plot.left - 6, y: at, "text-anchor": "end", "dominant-baseline": "middle" }, format(value)),
      );
    }
  }
  // Category labels: every one when they fit, else every n-th.
  const every = Math.max(1, Math.ceil(data.categories.length / (horizontal ? 14 : Math.max(2, Math.floor(60 / Math.min(longestCategory, 12))))));
  data.categories.forEach((category, c) => {
    if (c % every !== 0) {
      return;
    }
    const middle = px(bandAt(c) + bandSize / 2);
    root.append(
      horizontal
        ? svg("text", { x: plot.left - 6, y: middle, "text-anchor": "end", "dominant-baseline": "middle" }, shortLabel(category, 24))
        : svg("text", { x: middle, y: plot.bottom + 14, "text-anchor": "middle" }, shortLabel(category, 12)),
    );
  });
  if (data.xLabel && !horizontal) {
    root.append(svg("text", { x: (plot.left + plot.right) / 2, y: HEIGHT - 6, "text-anchor": "middle" }, data.xLabel));
  }

  const tip = (item: Series, c: number, value: number): SVGElement =>
    svg("title", {}, `${item.name}, ${data.categories[c] ?? ""}: ${format(value)}`);

  if (data.type === "bar") {
    const groups = data.stacked ? 1 : data.series.length;
    const thickness = Math.min(MAX_BAR, (bandSize * 0.72) / groups);
    data.categories.forEach((_, c) => {
      let positive = 0;
      let negative = 0;
      data.series.forEach((item, s) => {
        const value = item.values[c];
        if (value === null || value === undefined) {
          return;
        }
        const base = data.stacked ? (value >= 0 ? positive : negative) : Math.max(lo, Math.min(hi, 0));
        const top = base + value;
        if (data.stacked) {
          if (value >= 0) {
            positive = top;
          } else {
            negative = top;
          }
        }
        const offset = data.stacked ? 0 : (s - (groups - 1) / 2) * thickness;
        const across = bandAt(c) + bandSize / 2 + offset - thickness / 2;
        const [from, to] = [valueAt(base), valueAt(top)].sort((a, b) => a - b) as [number, number];
        // A 2px gap in the surface colour separates touching bars and stack segments.
        const length = Math.max(to - from - (data.stacked ? 2 : 0), 1);
        const rect = horizontal
          ? svg("rect", { class: seriesClass(s), x: px(from), y: px(across + 1), width: px(length), height: px(thickness - 2), rx: 3 })
          : svg("rect", { class: seriesClass(s), x: px(across + 1), y: px(from), width: px(thickness - 2), height: px(length), rx: 3 });
        rect.setAttribute("stroke", "none");
        rect.append(tip(item, c, value));
        root.append(rect);
      });
    });
    return root;
  }

  // Line and area: a gap in the data is a gap in the line, never a zero.
  const stackBase = new Array<number>(data.categories.length).fill(0);
  data.series.forEach((item, s) => {
    const points = item.values.map((value, c) => {
      if (value === null) {
        return null;
      }
      const base = data.stacked ? (stackBase[c] ?? 0) : 0;
      if (data.stacked) {
        stackBase[c] = base + value;
      }
      return { x: bandAt(c) + bandSize / 2, y: valueAt(base + value), y0: valueAt(Math.max(lo, base)), value, c };
    });
    let run: NonNullable<(typeof points)[number]>[] = [];
    const flush = (): void => {
      if (run.length === 0) {
        return;
      }
      const path = run.map((p, i) => `${i === 0 ? "M" : "L"}${px(p.x)} ${px(p.y)}`).join(" ");
      if (data.type === "area") {
        const back = [...run].reverse().map((p) => `L${px(p.x)} ${px(p.y0)}`).join(" ");
        root.append(svg("path", { class: `area ${seriesClass(s)}`, d: `${path} ${back} Z` }));
      }
      root.append(svg("path", { class: `line ${seriesClass(s)}`, d: path }));
      run = [];
    };
    for (const point of points) {
      if (point === null) {
        flush();
      } else {
        run.push(point);
      }
    }
    flush();
    // Markers when there are few enough points to tell them apart.
    if (data.categories.length <= 40) {
      for (const point of points) {
        if (point !== null) {
          const dot = svg("circle", { class: `dot ${seriesClass(s)}`, cx: px(point.x), cy: px(point.y), r: 4 });
          dot.setAttribute("stroke", "var(--grt-surface)");
          dot.append(tip(item, point.c, point.value));
          root.append(dot);
        }
      }
    }
  });
  return root;
}

/** Pie and donut: one series, slices from twelve o'clock, clockwise. */
function round(data: ChartData, locale: string | undefined): SVGElement {
  const size = 240;
  const root = svg("svg", { viewBox: `0 0 ${size} ${size}`, role: "img" });
  const values = (data.series[0]?.values ?? []).map((value) => value ?? 0);
  const total = values.reduce((sum, value) => sum + value, 0);
  const [cx, cy, radius] = [size / 2, size / 2, size / 2 - 6];
  const inner = data.type === "donut" ? radius * 0.58 : 0;
  const pointAt = (angle: number, r: number): string =>
    `${px(cx + r * Math.sin(angle))} ${px(cy - r * Math.cos(angle))}`;
  let start = 0;
  values.forEach((value, i) => {
    if (total <= 0 || value <= 0) {
      return;
    }
    const sweep = (value / total) * Math.PI * 2;
    const end = start + sweep;
    const share = formatNumber((value / total) * 100, { format: "percent" }, locale);
    const label = `${data.categories[i] ?? ""}: ${formatNumber(value, data.style, locale)} (${share})`;
    let slice: SVGElement;
    if (sweep >= Math.PI * 2 - 1e-9) {
      // One slice is the whole circle, which an arc cannot draw.
      slice = svg("circle", { class: `gap ${seriesClass(i)}`, cx, cy, r: px((radius + inner) / 2) });
      slice.setAttribute("fill", "none");
      slice.setAttribute("stroke-width", px(radius - inner));
      slice.setAttribute("stroke", `var(--grt-series-${(i % SERIES_COLOURS) + 1})`);
    } else {
      const large = sweep > Math.PI ? 1 : 0;
      const outer = `M${pointAt(start, radius)} A${radius} ${radius} 0 ${large} 1 ${pointAt(end, radius)}`;
      const back =
        inner > 0
          ? `L${pointAt(end, inner)} A${px(inner)} ${px(inner)} 0 ${large} 0 ${pointAt(start, inner)} Z`
          : `L${cx} ${cy} Z`;
      slice = svg("path", { class: `${seriesClass(i)}`, d: `${outer} ${back}` });
      // The gap between slices is the surface colour, not an outline of the slice.
      slice.setAttribute("stroke", "var(--grt-surface)");
      slice.setAttribute("stroke-width", "2");
    }
    slice.append(svg("title", {}, label));
    root.append(slice);
    start = end;
  });
  return root;
}

function legend(names: string[]): HTMLElement {
  return el(
    "ul",
    { class: "legend" },
    ...names.map((name, i) => el("li", {}, el("span", { class: `swatch ${seriesClass(i)}` }), name)),
  );
}

/** The data behind the picture, for screen readers and anyone who wants the numbers. */
function dataTable(data: ChartData, locale: string | undefined): HTMLElement {
  const head = el(
    "tr",
    {},
    el("th", { scope: "col" }, data.xLabel ?? ""),
    ...data.series.map((item) => el("th", { scope: "col", class: "right" }, item.name)),
  );
  const rows = data.categories.map((category, c) =>
    el(
      "tr",
      {},
      el("th", { scope: "row" }, category),
      ...data.series.map((item) => {
        const value = item.values[c];
        return el("td", { class: "right" }, value === null || value === undefined ? "" : formatNumber(value, data.style, locale));
      }),
    ),
  );
  const table = el("table", {}, el("thead", {}, head), el("tbody", {}, ...rows));
  return el("details", {}, el("summary", {}, "Data"), el("div", { class: "scroll" }, table));
}

export function renderChart(fields: Fields, context: RenderContext): HTMLElement {
  const data = read(fields);
  const isRound = data.type === "pie" || data.type === "donut";
  const picture = isRound ? round(data, context.locale) : cartesian(data, context.locale);
  const summary = text(fields, "fallback_text") ?? text(fields, "title") ?? `${data.type} chart`;
  picture.setAttribute("aria-label", summary);
  const names = isRound ? data.categories : data.series.map((item) => item.name);
  const holder = el("div", { class: isRound ? "chart round" : "chart" }, picture);
  return frame(
    "chart-block",
    fields,
    holder,
    data.yLabel !== undefined && !isRound ? el("p", { class: "muted" }, data.yLabel) : document.createTextNode(""),
    names.length > 1 || isRound ? legend(names) : document.createTextNode(""),
    dataTable(data, context.locale),
  );
}
