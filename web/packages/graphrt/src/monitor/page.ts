/**
 * The script of the monitoring page a server serves at `/monitor/`: one
 * `<graphrt-monitor>` that fills the window, with the page's service worker
 * for push notifications.
 *
 * A pairing link carries the device token in the URL fragment
 * (`#pair=...`). Fragments are never sent to a server, and the token is
 * removed from the address bar at once.
 */
import { defineGraphrtMonitor, type GraphrtMonitorElement } from "./monitor-element.ts";
import { browserPush } from "./push.ts";

const PAGE_CSS = `
html, body { height: 100%; margin: 0; }
body { display: flex; background: Canvas; }
graphrt-monitor { flex: 1; min-width: 0; height: 100dvh; }
`;

function tokenFromFragment(): string | undefined {
  const match = /^#pair=(.+)$/.exec(location.hash);
  if (match === null) {
    return undefined;
  }
  history.replaceState(null, "", `${location.pathname}${location.search}#/approvals`);
  return decodeURIComponent(match[1] ?? "");
}

function start(): void {
  defineGraphrtMonitor();
  try {
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(PAGE_CSS);
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
  } catch {
    // An old browser without constructed sheets: the page still works, unstyled around it.
  }
  if ("serviceWorker" in navigator) {
    // Next to this script, so its scope is the page's own folder.
    navigator.serviceWorker.register(new URL("sw.js", import.meta.url)).catch(() => undefined);
    // A notification that was tapped while the page is open names the run to show.
    navigator.serviceWorker.addEventListener("message", (event: MessageEvent) => {
      const data: unknown = event.data;
      if (typeof data === "object" && data !== null && "open" in data && typeof data.open === "string") {
        location.hash = data.open;
      }
    });
  }
  const monitor = document.createElement("graphrt-monitor") as GraphrtMonitorElement;
  // `<server>/monitor/monitor.js` -> `<server>`, also below a path prefix of a proxy.
  monitor.setAttribute("server", new URL("..", import.meta.url).href.replace(/\/$/, ""));
  monitor.pairingToken = tokenFromFragment();
  monitor.setAttribute("hash-routing", "");
  monitor.push = browserPush();
  document.body.append(monitor);
}

if (document.body) {
  start();
} else {
  document.addEventListener("DOMContentLoaded", start, { once: true });
}
