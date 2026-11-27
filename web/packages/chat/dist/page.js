/**
 * The script of the chat page a server serves at `/chat/` and
 * `/chat/{workflow}`: one `<graphrt-chat>` that fills the window.
 *
 * A token in the URL fragment (`#token=...`) is used as the end-user token and
 * removed from the address bar at once. Fragments are never sent to a server,
 * so the token does not reach logs.
 */
import { defineGraphrtChat } from "./chat-element.js";
const PAGE_CSS = `
html, body { height: 100%; margin: 0; }
body { display: flex; background: Canvas; }
graphrt-chat { flex: 1; min-width: 0; max-width: 820px; margin: 0 auto; height: 100dvh; }
`;
function tokenFromFragment() {
    const params = new URLSearchParams(location.hash.replace(/^#/, ""));
    const token = params.get("token") ?? undefined;
    if (token !== undefined) {
        history.replaceState(null, "", location.pathname + location.search);
    }
    return token;
}
function start() {
    defineGraphrtChat();
    try {
        const sheet = new CSSStyleSheet();
        sheet.replaceSync(PAGE_CSS);
        document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
    }
    catch {
        // An old browser without constructed sheets: the chat still works, unstyled around it.
    }
    const chat = document.createElement("graphrt-chat");
    // `<server>/chat/chat.js` -> `<server>`, also below a path prefix of a proxy.
    chat.setAttribute("server", new URL("..", import.meta.url).href.replace(/\/$/, ""));
    // The page is `<server>/chat/` or `<server>/chat/<workflow>`.
    const pageRoot = new URL(".", import.meta.url).pathname;
    const workflow = location.pathname.startsWith(pageRoot)
        ? decodeURIComponent(location.pathname.slice(pageRoot.length).replace(/\/+$/, ""))
        : "";
    if (workflow !== "") {
        chat.setAttribute("workflow", workflow);
    }
    const token = tokenFromFragment();
    if (token !== undefined) {
        chat.userToken = token;
    }
    document.body.append(chat);
}
if (document.body) {
    start();
}
else {
    document.addEventListener("DOMContentLoaded", start, { once: true });
}
