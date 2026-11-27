/**
 * `<graphrt-chat-widget>`: a chat bubble in a corner of any page. Pressing it
 * opens a panel with a `<graphrt-chat>`; on a phone the panel fills the screen.
 *
 * Attributes: `server`, `workflow`, `heading`, `greeting`, `placeholder`,
 * `theme` (passed on to the chat), `position` (`right`, the default, or
 * `left`) and `open`.
 */
import type { TokenSource } from "@graphrt/client";
import { adoptStyles } from "@graphrt/ui";
import { defineGraphrtChat, type GraphrtChatElement } from "./chat-element.ts";
import { WIDGET_CSS } from "./styles.ts";

const PASSED_ON = ["server", "workflow", "heading", "greeting", "placeholder", "theme"];
const SVG_NS = "http://www.w3.org/2000/svg";

const BaseElement: typeof HTMLElement = globalThis.HTMLElement ?? (class {} as typeof HTMLElement);

export class GraphrtChatWidgetElement extends BaseElement {
  static readonly observedAttributes = ["open", ...PASSED_ON];
  /** A token for servers with `--chat-access users`. Set it before the chat is first opened. */
  userToken: TokenSource | undefined;
  #panel: HTMLElement | undefined;
  #launcher: HTMLButtonElement | undefined;
  #chat: GraphrtChatElement | undefined;

  connectedCallback(): void {
    if (this.#panel !== undefined) {
      return;
    }
    defineGraphrtChat();
    const root = this.attachShadow({ mode: "open" });
    adoptStyles(root, WIDGET_CSS);
    const icon = document.createElementNS(SVG_NS, "svg");
    icon.setAttribute("viewBox", "0 0 24 24");
    icon.setAttribute("aria-hidden", "true");
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("d", "M21 11.5a8.5 8.5 0 0 1-12.4 7.6L3 21l1.9-5.6A8.5 8.5 0 1 1 21 11.5z");
    icon.append(path);
    const launcher = document.createElement("button");
    launcher.type = "button";
    launcher.className = "launcher";
    launcher.setAttribute("aria-label", "Open chat");
    launcher.setAttribute("aria-expanded", "false");
    launcher.append(icon);
    launcher.addEventListener("click", () => this.toggle());
    const panel = document.createElement("div");
    panel.className = "panel";
    panel.hidden = true;
    panel.addEventListener("graphrt-chat-close", () => this.close());
    panel.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        this.close();
      }
    });
    root.append(panel, launcher);
    this.#panel = panel;
    this.#launcher = launcher;
    this.#sync();
  }

  attributeChangedCallback(name: string, _old: string | null, value: string | null): void {
    if (name === "open") {
      this.#sync();
    } else if (this.#chat !== undefined) {
      if (value === null) {
        this.#chat.removeAttribute(name);
      } else {
        this.#chat.setAttribute(name, value);
      }
    }
  }

  /** Open the chat panel. The chat connects to the server the first time. */
  open(): void {
    this.setAttribute("open", "");
  }

  close(): void {
    this.removeAttribute("open");
    this.#launcher?.focus();
  }

  toggle(): void {
    if (this.hasAttribute("open")) {
      this.close();
    } else {
      this.open();
    }
  }

  #sync(): void {
    const panel = this.#panel;
    if (panel === undefined) {
      return;
    }
    const open = this.hasAttribute("open");
    // The chat is created when first opened: a page that never opens it makes no request.
    if (open && this.#chat === undefined) {
      const chat = document.createElement("graphrt-chat") as GraphrtChatElement;
      chat.setAttribute("closable", "");
      for (const name of PASSED_ON) {
        const value = this.getAttribute(name);
        if (value !== null) {
          chat.setAttribute(name, value);
        }
      }
      chat.userToken = this.userToken;
      panel.append(chat);
      this.#chat = chat;
    }
    panel.hidden = !open;
    this.#launcher?.setAttribute("aria-expanded", String(open));
    this.#launcher?.setAttribute("aria-label", open ? "Close chat" : "Open chat");
  }
}

/** Register `<graphrt-chat-widget>` and what it needs. Safe to call more than once. */
export function defineGraphrtChatWidget(): void {
  defineGraphrtChat();
  if (typeof customElements !== "undefined" && customElements.get("graphrt-chat-widget") === undefined) {
    customElements.define("graphrt-chat-widget", GraphrtChatWidgetElement);
  }
}
