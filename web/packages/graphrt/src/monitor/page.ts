/**
 * The script of the monitoring page a server serves at `/monitor/`: one
 * `<graphrt-monitor>` that fills the window, with the page's service worker
 * for push notifications.
 *
 * A pairing link carries the device token in the URL fragment
 * (`#pair=...`). Fragments are never sent to a server, and the token is
 * removed from the address bar at once.
 *
 * On a server with the admin console (`graphrt serve --console`), a person
 * signed in with their organization's account uses the page with that sign
 * in, and one who is not is offered it next to the key.
 */
import { GraphrtClient, sessionFetch } from "../client/index.ts";
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

/** The console sign in of this browser, when the server has a console. */
async function useConsole(monitor: GraphrtMonitorElement, server: string): Promise<void> {
  let session: Awaited<ReturnType<GraphrtClient["console"]["session"]>>;
  try {
    session = await new GraphrtClient({ baseUrl: server, fetch: sessionFetch("") }).console.session();
  } catch {
    // No console on this server: the key and device tokens, as before.
    return;
  }
  const role = session.role;
  if (
    session.signed_in &&
    session.via === "session" &&
    (role === "owner" || role === "operator" || role === "viewer")
  ) {
    const csrf = session.csrf_token ?? "";
    const fetchWithSession = sessionFetch(csrf);
    monitor.fetch = fetchWithSession;
    monitor.member = {
      role,
      name: session.user?.email ?? session.user?.name ?? session.user?.subject ?? "a member",
      signOut: () => {
        const client = new GraphrtClient({ baseUrl: server, fetch: fetchWithSession });
        void client.console.signOut().finally(() => location.reload());
      },
    };
    return;
  }
  // Back here after the sign in (the server adds its path prefix itself).
  monitor.organizationSignIn = `${session.sign_in_url}?return_to=${encodeURIComponent("/monitor/")}`;
}

async function start(): Promise<void> {
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
  const server = new URL("..", import.meta.url).href.replace(/\/$/, "");
  monitor.setAttribute("server", server);
  await useConsole(monitor, server);
  monitor.pairingToken = tokenFromFragment();
  monitor.setAttribute("hash-routing", "");
  monitor.push = browserPush();
  document.body.append(monitor);
}

if (document.body) {
  void start();
} else {
  document.addEventListener("DOMContentLoaded", () => void start(), { once: true });
}
