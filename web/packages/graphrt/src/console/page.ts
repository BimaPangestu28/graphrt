/**
 * The script of the console page a server serves at `/console/`: one
 * `<graphrt-console>` that fills the window.
 */
import { defineGraphrtConsole, type GraphrtConsoleElement } from "./console-element.ts";

const PAGE_CSS = `
html, body { height: 100%; margin: 0; }
body { display: flex; background: Canvas; }
graphrt-console { flex: 1; min-width: 0; height: 100dvh; }
`;

function start(): void {
  defineGraphrtConsole();
  try {
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(PAGE_CSS);
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
  } catch {
    // An old browser without constructed sheets: the page still works, unstyled around it.
  }
  const element = document.createElement("graphrt-console") as GraphrtConsoleElement;
  // `<server>/console/console.js` -> `<server>`, also below a path prefix of a proxy.
  element.setAttribute("server", new URL("..", import.meta.url).href.replace(/\/$/, ""));
  element.setAttribute("hash-routing", "");
  document.body.append(element);
}

if (document.body) {
  start();
} else {
  document.addEventListener("DOMContentLoaded", start, { once: true });
}
