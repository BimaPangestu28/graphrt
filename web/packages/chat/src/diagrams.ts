/**
 * Drawing `diagram` blocks in the chat. The chat bundles no Mermaid (about
 * 3 MB): a server started with `--chat-mermaid FILE` serves the operator's
 * copy at `/chat/mermaid.js`, and the chat loads it when a diagram first
 * shows. It comes from the chat's own server, never from another host.
 */
import { type MermaidLike, mermaidRenderer, setDiagramRenderer } from "graphrt-ui";

/** Loads a classic script into the page. Rejects when it cannot be loaded. */
export type ScriptLoader = (url: string) => Promise<void>;

function loadScript(url: string): Promise<void> {
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
function pageMermaid(): MermaidLike | undefined {
  const candidate: unknown = (globalThis as { mermaid?: unknown }).mermaid;
  if (typeof candidate !== "object" || candidate === null) {
    return undefined;
  }
  const { initialize, render } = candidate as Partial<MermaidLike>;
  return typeof initialize === "function" && typeof render === "function"
    ? (candidate as MermaidLike)
    : undefined;
}

let loading: Promise<boolean> | undefined;

/**
 * Make `diagram` blocks draw as pictures, with the Mermaid that `server`
 * serves. A page that already has Mermaid keeps its own, and no script is
 * loaded. The work is done once per page: later calls get the same answer.
 *
 * @param server - The base URL of the `graphrt serve`, without a trailing slash.
 * @param load - How a script is loaded. Default: a script element.
 * @returns Whether diagrams now draw as pictures. `false` leaves them as text.
 */
export function loadDiagrams(server: string, load: ScriptLoader = loadScript): Promise<boolean> {
  loading ??= (async () => {
    if (pageMermaid() === undefined) {
      try {
        await load(`${server}/chat/mermaid.js`);
      } catch (error) {
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
