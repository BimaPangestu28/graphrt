/** Styles of the chat view. Colours are the tokens of `@graphrt-dev/graphrt/ui`. */
import { TOKENS_CSS } from "../ui/index.ts";

export const CHAT_CSS = `
${TOKENS_CSS}
:host { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--grt-surface); color: var(--grt-text);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
* { box-sizing: border-box; }
[hidden] { display: none !important; }
header { display: flex; align-items: center; gap: 8px; padding: 10px 12px 10px 16px; border-bottom: 1px solid var(--grt-border);
  padding-top: max(10px, env(safe-area-inset-top)); }
h1 { font-size: 16px; font-weight: 650; margin: 0; flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.messages { flex: 1; min-height: 0; overflow-y: auto; overscroll-behavior: contain; padding: 16px; display: flex; flex-direction: column; gap: 14px; }
.empty { margin: auto; color: var(--grt-text-2); text-align: center; max-width: 32ch; }
.message { display: flex; flex-direction: column; gap: 8px; max-width: min(100%, 720px); min-width: 0; }
.message.user { align-self: flex-end; align-items: flex-end; }
.message.assistant { align-self: flex-start; width: 100%; }
.bubble { padding: 9px 13px; border-radius: 16px; overflow-wrap: anywhere; min-width: 0; max-width: 100%; }
.user .bubble { background: var(--grt-accent); color: var(--grt-accent-text); border-bottom-right-radius: 5px; white-space: pre-wrap; }
.assistant .bubble { background: var(--grt-surface-2); border-bottom-left-radius: 5px; }
.text { white-space: pre-wrap; }
.bubble pre { margin: 8px 0; padding: 10px; border-radius: 8px; overflow-x: auto; background: var(--grt-surface); border: 1px solid var(--grt-border);
  font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; }
code { font: 0.92em ui-monospace, SFMono-Regular, Menlo, monospace; background: var(--grt-surface); padding: 1px 4px; border-radius: 4px; }
a { color: var(--grt-accent); }
.typing { display: inline-flex; gap: 4px; padding: 6px 0; }
.typing i { width: 6px; height: 6px; border-radius: 50%; background: var(--grt-text-2); opacity: 0.5; }
@media (prefers-reduced-motion: no-preference) {
  .typing i { animation: blink 1.2s infinite both; }
  .typing i:nth-child(2) { animation-delay: 0.2s; } .typing i:nth-child(3) { animation-delay: 0.4s; }
  @keyframes blink { 0%, 80%, 100% { opacity: 0.25; } 40% { opacity: 0.9; } }
}
.note { color: var(--grt-text-2); font-size: 13px; }
.note.error { color: var(--grt-critical); }
.tools { display: flex; gap: 4px; }
.icon { min-width: 36px; min-height: 36px; padding: 0 8px; border-radius: 8px; border: 0; background: transparent; color: var(--grt-text-2); font: inherit; cursor: pointer; }
.icon:hover, .icon[aria-pressed="true"] { background: var(--grt-surface-2); color: var(--grt-text); }
form.composer { display: flex; gap: 8px; align-items: flex-end; padding: 10px 12px; border-top: 1px solid var(--grt-border);
  padding-bottom: max(10px, env(safe-area-inset-bottom)); }
textarea { flex: 1; min-width: 0; resize: none; overflow-y: hidden; max-height: 160px; min-height: 44px; padding: 10px 12px; border-radius: 14px;
  border: 1px solid var(--grt-border); background: var(--grt-surface); color: var(--grt-text); font: inherit; font-size: 16px; }
input { width: 100%; min-height: 44px; padding: 10px 12px; border-radius: 10px; border: 1px solid var(--grt-border);
  background: var(--grt-surface); color: var(--grt-text); font: inherit; font-size: 16px; }
button.primary { min-height: 44px; min-width: 44px; padding: 0 16px; border-radius: 14px; border: 0; background: var(--grt-accent);
  color: var(--grt-accent-text); font: inherit; font-weight: 600; cursor: pointer; }
button.primary:disabled { opacity: 0.5; cursor: default; }
button.stop { background: var(--grt-surface-2); color: var(--grt-text); border: 1px solid var(--grt-border); }
:focus-visible { outline: 2px solid var(--grt-accent); outline-offset: 2px; }
.gate { margin: auto; padding: 24px 16px; width: min(100%, 360px); display: flex; flex-direction: column; gap: 12px; }
.gate p { margin: 0; }
.check { display: flex; align-items: center; gap: 8px; font-size: 14px; }
.check input { width: auto; min-height: 0; }
`;

export const WIDGET_CSS = `
${TOKENS_CSS}
:host { position: fixed; z-index: 2147483000; bottom: max(16px, env(safe-area-inset-bottom)); right: 16px;
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
:host([position="left"]) { right: auto; left: 16px; }
* { box-sizing: border-box; }
[hidden] { display: none !important; }
.launcher { width: 56px; height: 56px; border-radius: 50%; border: 0; cursor: pointer; background: var(--grt-accent); color: var(--grt-accent-text);
  box-shadow: 0 6px 20px rgba(0, 0, 0, 0.22); display: grid; place-items: center; }
.launcher svg { width: 26px; height: 26px; fill: none; stroke: currentColor; stroke-width: 2; stroke-linecap: round; stroke-linejoin: round; }
.launcher:focus-visible { outline: 3px solid var(--grt-text); outline-offset: 2px; }
.panel { position: absolute; bottom: 68px; right: 0; width: min(400px, calc(100vw - 32px)); height: min(640px, calc(100dvh - 110px));
  border: 1px solid var(--grt-border); border-radius: 16px; overflow: hidden; background: var(--grt-surface);
  box-shadow: 0 16px 48px rgba(0, 0, 0, 0.24); display: flex; }
:host([position="left"]) .panel { right: auto; left: 0; }
graphrt-chat { flex: 1; min-width: 0; }
@media (max-width: 520px) {
  .panel { position: fixed; inset: 0; width: 100vw; height: 100dvh; border: 0; border-radius: 0; }
  :host([open]) .launcher { display: none; }
}
`;
