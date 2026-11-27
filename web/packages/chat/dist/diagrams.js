/**
 * Drawing `diagram` blocks in the chat. The chat bundles no Mermaid (about
 * 3 MB): a server started with `--chat-mermaid FILE` serves the operator's
 * copy at `/chat/mermaid.js`, and the chat loads it when a diagram first
 * shows. It comes from the chat's own server, never from another host.
 */
import { mermaidRenderer, setDiagramRenderer } from "@graphrt/ui";
function loadScript(url) {
    return new Promise((resolve, reject) => {
        const script = document.createElement("script");
        script.src = url;
        script.async = true;
        script.addEventListener("load", () => resolve(), { once: true });
        script.addEventListener("error", () => reject(new Error(`cannot load ${url}`)), { once: true });
        document.head.append(script);
    });
}
/** The Mermaid of this page, which its script sets as a global. */
function pageMermaid() {
    const candidate = globalThis.mermaid;
    if (typeof candidate !== "object" || candidate === null) {
        return undefined;
    }
    const { initialize, render } = candidate;
    return typeof initialize === "function" && typeof render === "function"
        ? candidate
        : undefined;
}
let loading;
/**
 * Make `diagram` blocks draw as pictures, with the Mermaid that `server`
 * serves. A page that already has Mermaid keeps its own, and no script is
 * loaded. The work is done once per page: later calls get the same answer.
 *
 * @param server - The base URL of the `graphrt serve`, without a trailing slash.
 * @param load - How a script is loaded. Default: a script element.
 * @returns Whether diagrams now draw as pictures. `false` leaves them as text.
 */
export function loadDiagrams(server, load = loadScript) {
    loading ??= (async () => {
        if (pageMermaid() === undefined) {
            try {
                await load(`${server}/chat/mermaid.js`);
            }
            catch (error) {
                console.warn("graphrt chat: diagrams stay text,", error);
                return false;
            }
        }
        const mermaid = pageMermaid();
        if (mermaid === undefined) {
            console.warn("graphrt chat: diagrams stay text, /chat/mermaid.js did not define `mermaid`");
            return false;
        }
        setDiagramRenderer(mermaidRenderer(mermaid));
        return true;
    })();
    return loading;
}
