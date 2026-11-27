/**
 * The styles of every block. Colours are tokens on the host element: light by
 * default, dark when the page prefers it or the host has `theme="dark"`.
 * A page restyles blocks by setting the `--grt-*` properties on the element.
 *
 * The categorical series colours are a fixed, ordered palette checked for
 * colour-vision deficiencies in both modes. The order is part of that check.
 */

const LIGHT = `
  color-scheme: light;
  --grt-surface: #ffffff;
  --grt-surface-2: #f5f5f4;
  --grt-border: #e3e3e0;
  --grt-text: #191919;
  --grt-text-2: #666666;
  --grt-accent: #257de0;
  --grt-accent-text: #ffffff;
  --grt-good: #0b7a1e;
  --grt-warning: #9a6700;
  --grt-critical: #c62828;
  --grt-series-1: #257de0;
  --grt-series-2: #f47230;
  --grt-series-3: #00b894;
  --grt-series-4: #f2a600;
  --grt-series-5: #ee7fac;
  --grt-series-6: #008a17;
  --grt-series-7: #6a4ce2;
  --grt-series-8: #ec4647;
`;

const DARK = `
  color-scheme: dark;
  --grt-surface: #191919;
  --grt-surface-2: #242424;
  --grt-border: #3a3a3a;
  --grt-text: #efefef;
  --grt-text-2: #9c9c9c;
  --grt-accent: #338ced;
  --grt-accent-text: #ffffff;
  --grt-good: #35c24e;
  --grt-warning: #e0a52b;
  --grt-critical: #f27a7a;
  --grt-series-1: #338ced;
  --grt-series-2: #e76624;
  --grt-series-3: #00a888;
  --grt-series-4: #d28e05;
  --grt-series-5: #e24d8b;
  --grt-series-6: #0aac23;
  --grt-series-7: #9887ef;
  --grt-series-8: #eb6263;
`;

/** How many series colours there are before they repeat. */
export const SERIES_COLOURS = 8;

/** The colour tokens, for elements that host blocks in their own shadow tree. */
export const TOKENS_CSS = `
:host { ${LIGHT} }
@media (prefers-color-scheme: dark) { :host(:not([theme="light"])) { ${DARK} } }
:host([theme="dark"]) { ${DARK} }
`;

export const BLOCK_CSS = `
${TOKENS_CSS}
:host { display: block; font: 14px/1.45 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; color: var(--grt-text); }
* { box-sizing: border-box; }
.block { background: var(--grt-surface); border: 1px solid var(--grt-border); border-radius: 12px; padding: 14px 16px; overflow: hidden; }
.title { font-size: 15px; font-weight: 600; margin: 0; }
.subtitle { color: var(--grt-text-2); margin: 2px 0 0; }
.head { margin-bottom: 12px; }
.muted { color: var(--grt-text-2); }
.sr { position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }

.error { border-style: dashed; }
.error pre { margin: 8px 0 0; white-space: pre-wrap; overflow-wrap: anywhere; font: inherit; }
.error small { color: var(--grt-text-2); }

.stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(140px, 1fr)); gap: 10px; }
.stat { background: var(--grt-surface-2); border-radius: 10px; padding: 12px; min-width: 0; }
.stat-label { color: var(--grt-text-2); font-size: 12.5px; }
.stat-value { font-size: 24px; font-weight: 650; letter-spacing: -0.01em; overflow-wrap: anywhere; font-variant-numeric: tabular-nums; }
.stat-delta { font-size: 12.5px; font-weight: 600; }
.stat-delta.good { color: var(--grt-good); }
.stat-delta.bad { color: var(--grt-critical); }
.stat-caption { color: var(--grt-text-2); font-size: 12.5px; }

.scroll { overflow-x: auto; -webkit-overflow-scrolling: touch; }
table { border-collapse: collapse; width: 100%; font-variant-numeric: tabular-nums; }
th, td { padding: 8px 10px; text-align: left; border-bottom: 1px solid var(--grt-border); vertical-align: top; }
th { font-size: 12.5px; font-weight: 600; color: var(--grt-text-2); white-space: nowrap; }
tr:last-child td { border-bottom: 0; }
.right { text-align: right; }
.center { text-align: center; }
.status { display: inline-block; padding: 1px 8px; border-radius: 999px; background: var(--grt-surface-2); border: 1px solid var(--grt-border); font-size: 12.5px; white-space: nowrap; }

.chart svg { display: block; width: 100%; height: auto; }
.chart.round { max-width: 260px; margin: 0 auto; }
.chart text { fill: var(--grt-text-2); font-size: 11px; }
.chart .grid { stroke: var(--grt-border); stroke-width: 1; }
.chart .gap { stroke: var(--grt-surface); stroke-width: 2; }
.legend { display: flex; flex-wrap: wrap; gap: 4px 14px; margin: 10px 0 0; padding: 0; list-style: none; font-size: 12.5px; }
.legend li { display: flex; align-items: center; gap: 6px; }
.swatch { width: 10px; height: 10px; border-radius: 3px; flex: none; }
details { margin-top: 10px; }
summary { cursor: pointer; color: var(--grt-text-2); font-size: 12.5px; }
.s1 { fill: var(--grt-series-1); stroke: var(--grt-series-1); background: var(--grt-series-1); }
.s2 { fill: var(--grt-series-2); stroke: var(--grt-series-2); background: var(--grt-series-2); }
.s3 { fill: var(--grt-series-3); stroke: var(--grt-series-3); background: var(--grt-series-3); }
.s4 { fill: var(--grt-series-4); stroke: var(--grt-series-4); background: var(--grt-series-4); }
.s5 { fill: var(--grt-series-5); stroke: var(--grt-series-5); background: var(--grt-series-5); }
.s6 { fill: var(--grt-series-6); stroke: var(--grt-series-6); background: var(--grt-series-6); }
.s7 { fill: var(--grt-series-7); stroke: var(--grt-series-7); background: var(--grt-series-7); }
.s8 { fill: var(--grt-series-8); stroke: var(--grt-series-8); background: var(--grt-series-8); }
.line { fill: none; stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; }
.area { stroke: none; opacity: 0.22; }
.dot { stroke: var(--grt-surface); stroke-width: 2; }

.diagram pre { margin: 0; padding: 12px; background: var(--grt-surface-2); border-radius: 8px; overflow-x: auto; font: 12.5px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; }
.diagram img { display: block; max-width: 100%; height: auto; margin: 0 auto; }

.card { display: flex; flex-direction: column; gap: 10px; }
.card > *, .ac-container > *, .ac-column > * { min-width: 0; }
.ac-container, .ac-column { display: flex; flex-direction: column; gap: 8px; }
.ac-columns { display: flex; gap: 12px; flex-wrap: wrap; }
.ac-column { flex: 1 1 0; }
.ac-column.auto { flex: 0 0 auto; }
.ac-column.w2 { flex-grow: 2; } .ac-column.w3 { flex-grow: 3; } .ac-column.w4 { flex-grow: 4; } .ac-column.w5 { flex-grow: 5; } .ac-column.w6 { flex-grow: 6; }
.ac-italic { font-style: italic; } .ac-underline { text-decoration: underline; } .ac-strikethrough { text-decoration: line-through; }
.ac-underline.ac-strikethrough { text-decoration: underline line-through; }
.ac-highlight { background: color-mix(in srgb, var(--grt-series-4) 35%, transparent); }
.ac-text { margin: 0; overflow-wrap: anywhere; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ac-text.wrap { white-space: pre-wrap; overflow: visible; }
.ac-heading { font-size: 16px; font-weight: 600; }
.ac-small { font-size: 12.5px; } .ac-medium { font-size: 15px; } .ac-large { font-size: 18px; } .ac-extralarge { font-size: 22px; }
.ac-lighter { font-weight: 300; } .ac-bolder { font-weight: 650; }
.ac-subtle { color: var(--grt-text-2); }
.ac-accent { color: var(--grt-accent); } .ac-good { color: var(--grt-good); } .ac-warning { color: var(--grt-warning); } .ac-attention { color: var(--grt-critical); }
.ac-mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.ac-center { text-align: center; } .ac-right { text-align: right; }
.ac-separator { border-top: 1px solid var(--grt-border); padding-top: 8px; }
.ac-emphasis { background: var(--grt-surface-2); border-radius: 8px; padding: 10px; }
.ac-image { max-width: 100%; height: auto; border-radius: 6px; }
.ac-image.person { border-radius: 50%; }
.ac-image.small { width: 40px; } .ac-image.medium { width: 80px; } .ac-image.large { width: 160px; }
.ac-images { display: flex; flex-wrap: wrap; gap: 8px; }
.ac-facts { display: grid; grid-template-columns: auto 1fr; gap: 4px 12px; margin: 0; }
.ac-facts dt { font-weight: 600; } .ac-facts dd { margin: 0; overflow-wrap: anywhere; }
.ac-field { display: flex; flex-direction: column; gap: 4px; }
.ac-field > label, .ac-legend { font-size: 12.5px; font-weight: 600; }
.ac-required::after { content: " *"; color: var(--grt-critical); }
input[type="text"], input[type="number"], input[type="date"], input[type="time"], input[type="email"], input[type="tel"], input[type="url"], input[type="password"], textarea, select {
  width: 100%; font: inherit; font-size: 16px; color: var(--grt-text); background: var(--grt-surface);
  border: 1px solid var(--grt-border); border-radius: 8px; padding: 8px 10px; min-height: 40px;
}
textarea { min-height: 80px; resize: vertical; }
input:focus-visible, textarea:focus-visible, select:focus-visible, button:focus-visible, a:focus-visible { outline: 2px solid var(--grt-accent); outline-offset: 2px; }
.ac-choice { display: flex; align-items: center; gap: 8px; min-height: 32px; }
.ac-choices { border: 0; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 2px; }
.ac-invalid { color: var(--grt-critical); font-size: 12.5px; }
.ac-actions { display: flex; flex-wrap: wrap; gap: 8px; }
button, a.button {
  font: inherit; font-weight: 600; min-height: 40px; padding: 8px 14px; border-radius: 8px; cursor: pointer;
  border: 1px solid var(--grt-border); background: var(--grt-surface); color: var(--grt-text); text-decoration: none;
  display: inline-flex; align-items: center; gap: 6px;
}
button.positive { background: var(--grt-accent); border-color: var(--grt-accent); color: var(--grt-accent-text); }
button.destructive { color: var(--grt-critical); border-color: var(--grt-critical); }
button:disabled { opacity: 0.5; cursor: default; }
button img { width: 16px; height: 16px; }
.ac-clickable { cursor: pointer; }
.ac-showcard { border-top: 1px solid var(--grt-border); padding-top: 10px; }
[hidden] { display: none !important; }
@media (prefers-reduced-motion: no-preference) { button { transition: filter 0.12s; } button:hover:not(:disabled) { filter: brightness(0.96); } }
`;
