/** Styles of the console: the monitoring view's, with tables and forms. Colours are the tokens of `graphrt/ui`. */
import { TOKENS_CSS } from "../ui/index.ts";

export const CONSOLE_CSS = `
${TOKENS_CSS}
:host { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--grt-surface); color: var(--grt-text);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
* { box-sizing: border-box; }
[hidden] { display: none !important; }
header { display: flex; align-items: center; gap: 8px; padding: 10px 16px; border-bottom: 1px solid var(--grt-border);
  padding-top: max(10px, env(safe-area-inset-top)); flex-wrap: wrap; }
h1 { font-size: 17px; font-weight: 650; margin: 0; flex: 1 1 auto; min-width: 9ch; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
h2 { font-size: 15px; font-weight: 650; margin: 0; }
h3 { font-size: 14px; font-weight: 650; margin: 0; }
main { flex: 1; min-height: 0; overflow-y: auto; overscroll-behavior: contain; display: flex; flex-direction: column; }
.view { padding: 12px 16px 24px; width: 100%; max-width: 1000px; margin: 0 auto; display: flex; flex-direction: column; gap: 12px; }
.view.runs { padding: 0; max-width: none; flex: 1; min-height: 0; }
graphrt-monitor { flex: 1; min-height: 480px; }
nav { display: flex; border-top: 1px solid var(--grt-border); padding-bottom: env(safe-area-inset-bottom); background: var(--grt-surface);
  overflow-x: auto; }
nav button { flex: 1; min-height: 52px; min-width: 72px; border: 0; background: transparent; color: var(--grt-text-2); font: inherit;
  font-size: 13px; font-weight: 600; cursor: pointer; }
nav button[aria-current="page"] { color: var(--grt-accent); box-shadow: inset 0 2px 0 var(--grt-accent); }
.who { display: flex; align-items: center; gap: 8px; font-size: 13px; color: var(--grt-text-2); min-width: 0; }
.who .email { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 28ch; }
.pill { flex: none; align-self: center; padding: 1px 9px; border-radius: 10px; font-size: 12px; line-height: 18px; font-weight: 650;
  border: 1px solid currentColor; }
.pill.where { color: var(--grt-accent); }
.pill.ok { color: var(--grt-good); }
.pill.denied, .pill.warn { color: var(--grt-warning); }
.pill.failed, .pill.bad { color: var(--grt-critical); }
.card { border: 1px solid var(--grt-border); border-radius: 14px; padding: 14px; display: flex; flex-direction: column; gap: 10px; min-width: 0; }
.card.notice { border-color: var(--grt-warning); }
.line { display: flex; align-items: center; gap: 8px; min-width: 0; flex-wrap: wrap; }
.name { font-weight: 600; min-width: 0; overflow-wrap: anywhere; }
.grow { flex: 1; }
.muted { color: var(--grt-text-2); font-size: 13px; }
.note { color: var(--grt-text-2); font-size: 13px; margin: 0; }
.note.error, .outcome.bad { color: var(--grt-critical); }
.outcome { margin: 0; font-size: 14px; }
.outcome.good { color: var(--grt-good); }
.empty { margin: 24px auto; color: var(--grt-text-2); text-align: center; max-width: 40ch; }
code, .mono { font: 0.92em ui-monospace, SFMono-Regular, Menlo, monospace; }
.envs { display: grid; grid-template-columns: repeat(auto-fill, minmax(190px, 1fr)); gap: 8px; }
.env { border: 1px solid var(--grt-border); border-radius: 10px; padding: 10px; display: flex; flex-direction: column; gap: 6px; }
.env.here { border-color: var(--grt-accent); }
.versions { margin: 0; padding: 0; list-style: none; display: flex; flex-direction: column; gap: 4px; font-size: 13px; }
.versions li { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; border-bottom: 1px solid var(--grt-border); padding: 4px 0; }
.versions li:last-child { border-bottom: 0; }
table { width: 100%; border-collapse: collapse; font-size: 14px; }
th, td { text-align: left; padding: 8px 6px; border-bottom: 1px solid var(--grt-border); vertical-align: top; overflow-wrap: anywhere; }
th { font-size: 12px; color: var(--grt-text-2); font-weight: 650; }
.actions { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
button.primary, button.plain, button.danger, a.button { min-height: 40px; padding: 0 14px; border-radius: 10px; font: inherit;
  font-weight: 600; cursor: pointer; display: inline-flex; align-items: center; justify-content: center; text-decoration: none; }
button.primary, a.button.primary { border: 0; background: var(--grt-accent); color: var(--grt-accent-text); }
button.plain, a.button.plain { border: 1px solid var(--grt-border); background: var(--grt-surface); color: var(--grt-text); }
button.danger { border: 1px solid var(--grt-critical); background: var(--grt-surface); color: var(--grt-critical); }
button.small { min-height: 32px; padding: 0 10px; font-size: 13px; }
button:disabled { opacity: 0.5; cursor: default; }
input[type="text"], input[type="password"], input[type="search"], select { min-height: 40px; padding: 8px 10px; border-radius: 10px;
  border: 1px solid var(--grt-border); background: var(--grt-surface); color: var(--grt-text); font: inherit; font-size: 16px; min-width: 0; }
input[type="file"] { font: inherit; font-size: 14px; max-width: 100%; }
form.row { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
form.row > input[type="text"], form.row > input[type="password"], form.row > input[type="search"] { flex: 1 1 180px; }
label.field { display: flex; flex-direction: column; gap: 4px; font-size: 13px; color: var(--grt-text-2); }
:focus-visible { outline: 2px solid var(--grt-accent); outline-offset: 2px; }
a { color: var(--grt-accent); overflow-wrap: anywhere; }
.gate { margin: auto; padding: 24px 16px; width: min(100%, 420px); display: flex; flex-direction: column; gap: 12px; }
.gate p { margin: 0; }
details summary { cursor: pointer; font-size: 13px; color: var(--grt-text-2); }
pre.data { margin: 6px 0 0; padding: 8px; border-radius: 8px; overflow-x: auto; background: var(--grt-surface-2);
  font: 12px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; white-space: pre-wrap; overflow-wrap: anywhere; }
@media (max-width: 640px) {
  table.stack thead { display: none; }
  table.stack tr { display: flex; flex-direction: column; border-bottom: 1px solid var(--grt-border); padding: 6px 0; }
  table.stack td { border: 0; padding: 2px 0; }
  .who .email { display: none; }
}
@media (min-width: 720px) {
  nav { order: -1; border-top: 0; border-bottom: 1px solid var(--grt-border); justify-content: center; }
  nav button { flex: none; padding: 0 20px; min-height: 46px; font-size: 14px; }
  nav button[aria-current="page"] { box-shadow: inset 0 -2px 0 var(--grt-accent); }
  header { order: -2; }
}
`;
