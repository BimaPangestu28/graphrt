/** Styles of the monitoring view. Colours are the tokens of `graphrt/ui`. */
import { TOKENS_CSS } from "../ui/index.ts";

export const MONITOR_CSS = `
${TOKENS_CSS}
:host { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--grt-surface); color: var(--grt-text);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
* { box-sizing: border-box; }
[hidden] { display: none !important; }
header { display: flex; align-items: center; gap: 8px; padding: 10px 16px; border-bottom: 1px solid var(--grt-border);
  padding-top: max(10px, env(safe-area-inset-top)); }
h1 { font-size: 17px; font-weight: 650; margin: 0; flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
h2 { font-size: 15px; font-weight: 650; margin: 0 0 8px; }
main { flex: 1; min-height: 0; overflow-y: auto; overscroll-behavior: contain; }
.view { padding: 12px 16px 24px; max-width: 860px; margin: 0 auto; display: flex; flex-direction: column; gap: 12px; }
nav { display: flex; border-top: 1px solid var(--grt-border); padding-bottom: env(safe-area-inset-bottom); background: var(--grt-surface); }
nav button { flex: 1; min-height: 52px; border: 0; background: transparent; color: var(--grt-text-2); font: inherit; font-size: 13px;
  font-weight: 600; cursor: pointer; display: flex; align-items: center; justify-content: center; gap: 6px; }
nav button[aria-current="page"] { color: var(--grt-accent); box-shadow: inset 0 2px 0 var(--grt-accent); }
.badge { min-width: 20px; height: 20px; padding: 0 6px; border-radius: 10px; background: var(--grt-warning); color: #ffffff;
  font-size: 12px; line-height: 20px; text-align: center; }
.chips { display: flex; flex-wrap: wrap; gap: 8px; }
.chip { flex: none; min-height: 36px; padding: 0 14px; border-radius: 18px; border: 1px solid var(--grt-border); background: var(--grt-surface);
  color: var(--grt-text); font: inherit; font-size: 14px; cursor: pointer; }
.chip[aria-pressed="true"] { background: var(--grt-accent); border-color: var(--grt-accent); color: var(--grt-accent-text); }
.list { display: flex; flex-direction: column; gap: 8px; margin: 0; padding: 0; list-style: none; }
.row { display: flex; flex-direction: column; gap: 2px; width: 100%; padding: 12px 14px; border-radius: 12px; border: 1px solid var(--grt-border);
  background: var(--grt-surface); color: inherit; font: inherit; text-align: left; cursor: pointer; min-width: 0; }
.row:hover { background: var(--grt-surface-2); }
.line { display: flex; align-items: center; gap: 8px; min-width: 0; }
.name { font-weight: 600; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; }
.muted { color: var(--grt-text-2); font-size: 13px; }
.snippet { color: var(--grt-text-2); font-size: 14px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
.pill { flex: none; padding: 1px 9px; border-radius: 10px; font-size: 12px; font-weight: 650; border: 1px solid currentColor; }
.pill.running { color: var(--grt-accent); }
.pill.paused { color: var(--grt-warning); }
.pill.succeeded { color: var(--grt-good); }
.pill.failed { color: var(--grt-critical); }
.pill.aborted, .pill.queued { color: var(--grt-text-2); }
.card p, .request p, .pairing p { margin: 0; }
.request { display: flex; flex-direction: column; gap: 8px; min-width: 0; }
.card { border: 1px solid var(--grt-border); border-radius: 14px; padding: 14px; display: flex; flex-direction: column; gap: 10px; min-width: 0; }
.card.urgent { border-color: var(--grt-warning); }
.empty { margin: 32px auto; color: var(--grt-text-2); text-align: center; max-width: 34ch; }
.note { color: var(--grt-text-2); font-size: 13px; margin: 0; }
.note.error { color: var(--grt-critical); }
.answer { white-space: pre-wrap; overflow-wrap: anywhere; }
.answer pre, pre.data { margin: 8px 0 0; padding: 10px; border-radius: 8px; overflow-x: auto; background: var(--grt-surface-2);
  font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; white-space: pre-wrap; overflow-wrap: anywhere; }
code { font: 0.92em ui-monospace, SFMono-Regular, Menlo, monospace; }
a { color: var(--grt-accent); overflow-wrap: anywhere; }
.facts { display: flex; flex-wrap: wrap; gap: 4px 16px; color: var(--grt-text-2); font-size: 13px; }
.events { margin: 0; padding: 0; list-style: none; font-size: 13px; }
.events li { display: flex; gap: 8px; padding: 5px 0; border-bottom: 1px solid var(--grt-border); min-width: 0; }
.events li:last-child { border-bottom: 0; }
.events .type { flex: none; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.events .about { color: var(--grt-text-2); min-width: 0; overflow-wrap: anywhere; }
.events .good .type { color: var(--grt-good); }
.events .bad .type { color: var(--grt-critical); }
.events .waiting .type { color: var(--grt-warning); }
.actions { display: flex; flex-wrap: wrap; gap: 8px; }
button.primary, button.plain, button.danger { min-height: 44px; padding: 0 16px; border-radius: 12px; font: inherit; font-weight: 600; cursor: pointer; }
button.primary { border: 0; background: var(--grt-accent); color: var(--grt-accent-text); }
button.plain { border: 1px solid var(--grt-border); background: var(--grt-surface); color: var(--grt-text); }
button.danger { border: 1px solid var(--grt-critical); background: var(--grt-surface); color: var(--grt-critical); }
button:disabled { opacity: 0.5; cursor: default; }
a.primary.button { display: inline-flex; align-items: center; justify-content: center; min-height: 44px; padding: 0 16px;
  border-radius: 12px; background: var(--grt-accent); color: var(--grt-accent-text); font-weight: 600; text-decoration: none; }
button.link { border: 0; background: transparent; color: var(--grt-accent); font: inherit; padding: 0; min-height: 36px; cursor: pointer; }
input[type="text"], input[type="password"] { width: 100%; min-height: 44px; padding: 10px 12px; border-radius: 10px;
  border: 1px solid var(--grt-border); background: var(--grt-surface); color: var(--grt-text); font: inherit; font-size: 16px; }
:focus-visible { outline: 2px solid var(--grt-accent); outline-offset: 2px; }
.gate { margin: auto; padding: 24px 16px; width: min(100%, 380px); display: flex; flex-direction: column; gap: 12px; }
.gate p { margin: 0; }
.check { display: flex; align-items: center; gap: 8px; font-size: 14px; }
.pairing { display: flex; flex-direction: column; gap: 10px; align-items: flex-start; }
.pairing svg { width: min(100%, 260px); height: auto; border-radius: 8px; border: 1px solid var(--grt-border); }
.pairing .link { font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; overflow-wrap: anywhere; user-select: all;
  background: var(--grt-surface-2); padding: 8px; border-radius: 8px; width: 100%; }
.form { display: flex; gap: 8px; align-items: stretch; }
.form input { flex: 1; min-width: 0; }
.call { border: 1px solid var(--grt-border); border-radius: 10px; padding: 10px; min-width: 0; }
graphrt-block { display: block; min-width: 0; }
:host([embedded]) nav { order: -1; border-top: 0; border-bottom: 1px solid var(--grt-border); padding-bottom: 0; }
:host([embedded]) nav button { min-height: 44px; }
:host([embedded]) nav button[aria-current="page"] { box-shadow: inset 0 -2px 0 var(--grt-accent); }
@media (min-width: 720px) {
  :host { flex-direction: column; }
  nav { order: -1; border-top: 0; border-bottom: 1px solid var(--grt-border); justify-content: center; }
  nav button { flex: none; padding: 0 22px; min-height: 46px; font-size: 14px; }
  nav button[aria-current="page"] { box-shadow: inset 0 -2px 0 var(--grt-accent); }
  header { order: -2; }
}
`;
