/**
 * A DOM for tests of the web components: happy-dom's window as the globals a
 * browser has. Import this module before the code under test.
 */
import { Window } from "happy-dom";

const window = new Window({ url: "https://agent.example/chat/" });

const NAMES = [
  "window",
  "document",
  "customElements",
  "HTMLElement",
  "HTMLInputElement",
  "HTMLTextAreaElement",
  "HTMLButtonElement",
  "HTMLSelectElement",
  "HTMLAnchorElement",
  "HTMLScriptElement",
  "HTMLImageElement",
  "Element",
  "Node",
  "ShadowRoot",
  "CustomEvent",
  "Event",
  "KeyboardEvent",
  "CSSStyleSheet",
  "MutationObserver",
  "localStorage",
  "sessionStorage",
  "location",
  "matchMedia",
  "requestAnimationFrame",
] as const;

for (const name of NAMES) {
  const value = (window as unknown as Record<string, unknown>)[name];
  Object.defineProperty(globalThis, name, {
    value: typeof value === "function" && name === "matchMedia" ? value.bind(window) : value,
    configurable: true,
    writable: true,
  });
}

export { window };
