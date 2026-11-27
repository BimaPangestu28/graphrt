/**
 * The widget script a server serves at `/chat/embed.js`:
 *
 *   <script src="https://agent.example.com/chat/embed.js" data-workflow="support" async></script>
 *
 * It adds a chat bubble to the page. The server is the one the script came
 * from, which must list the page's origin in `--cors-origin`. Options are
 * `data-` attributes of the script tag: `workflow`, `heading`, `greeting`,
 * `placeholder`, `theme` (`light` or `dark`), `position` (`right` or `left`)
 * and `open`.
 *
 * `window.GraphrtChat` then has `open()`, `close()`, `toggle()` and
 * `setUserToken(token)` for servers that verify end-user tokens.
 */
import type { TokenSource } from "../client/index.ts";
import { defineGraphrtChatWidget, type GraphrtChatWidgetElement } from "./widget.ts";

const OPTIONS = ["workflow", "heading", "greeting", "placeholder", "theme", "position"];

interface WidgetApi {
  open(): void;
  close(): void;
  toggle(): void;
  setUserToken(token: TokenSource): void;
}

function mount(script: HTMLScriptElement | null): void {
  const holder = globalThis as { GraphrtChat?: WidgetApi };
  if (holder.GraphrtChat !== undefined || script === null) {
    // Included twice: one bubble is enough.
    return;
  }
  defineGraphrtChatWidget();
  const widget = document.createElement("graphrt-chat-widget") as GraphrtChatWidgetElement;
  // `<server>/chat/embed.js` -> `<server>`, also below a path prefix of a proxy.
  widget.setAttribute("server", new URL("..", script.src).href.replace(/\/$/, ""));
  for (const name of OPTIONS) {
    const value = script.dataset[name];
    if (value !== undefined) {
      widget.setAttribute(name, value);
    }
  }
  holder.GraphrtChat = {
    open: () => widget.open(),
    close: () => widget.close(),
    toggle: () => widget.toggle(),
    setUserToken: (token) => {
      widget.userToken = token;
    },
  };
  const attach = (): void => {
    document.body.append(widget);
    if (script.dataset.open !== undefined) {
      widget.open();
    }
  };
  if (document.body) {
    attach();
  } else {
    document.addEventListener("DOMContentLoaded", attach, { once: true });
  }
}

mount(document.currentScript instanceof HTMLScriptElement ? document.currentScript : null);
