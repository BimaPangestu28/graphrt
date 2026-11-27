/**
 * `<graphrt-chat>`: a chat with one workflow of a `graphrt serve`.
 *
 * @example
 * <graphrt-chat server="https://agent.example.com" workflow="support"></graphrt-chat>
 *
 * Attributes: `server` (default: the page's origin), `workflow` (default: the
 * first the server offers), `heading`, `placeholder`, `greeting`, `theme`
 * (`light` or `dark`; absent follows the page) and `closable` (shows a close
 * button that fires `graphrt-chat-close`).
 *
 * Properties: `userToken` (a token or a function returning one, for servers
 * with `--chat-access users`) and `apiKey`.
 */
import type { TokenSource } from "@graphrt/client";
import {
  adoptStyles,
  type BlockAction,
  defineGraphrtUi,
  type GraphrtBlockElement,
  uiBlockKind,
} from "@graphrt/ui";
import { loadDiagrams } from "./diagrams.ts";
import { type ChatMessage, ChatSession, type ChatState, type KeyValueStore } from "./session.ts";
import { CHAT_CSS } from "./styles.ts";
import { richText } from "./text.ts";

function make<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attributes: Record<string, string> = {},
  ...children: (Node | string)[]
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    element.setAttribute(name, value);
  }
  element.append(...children);
  return element;
}

/** The browser's storage, or none where it is blocked. */
function browserStorage(): KeyValueStore | undefined {
  try {
    return globalThis.localStorage;
  } catch {
    return undefined;
  }
}

/** What is drawn for one message, kept to change it in place while text streams in. */
interface MessageView {
  root: HTMLElement;
  bubble: HTMLElement;
  extras: HTMLElement;
  /** What the view was last drawn from. */
  drawn: ChatMessage | undefined;
}

const BaseElement: typeof HTMLElement = globalThis.HTMLElement ?? (class {} as typeof HTMLElement);

export class GraphrtChatElement extends BaseElement {
  static readonly observedAttributes = ["heading", "theme", "placeholder"];
  /** A token for servers with `--chat-access users`. Set it before the element is connected. */
  userToken: TokenSource | undefined;
  /** An API key for servers with `--chat-access key`, instead of asking the person. */
  apiKey: string | undefined;
  #session: ChatSession | undefined;
  #unsubscribe: (() => void) | undefined;
  #views = new Map<string, MessageView>();
  #parts:
    | {
        title: HTMLElement;
        list: HTMLElement;
        empty: HTMLElement;
        composer: HTMLFormElement;
        input: HTMLTextAreaElement;
        send: HTMLButtonElement;
        stop: HTMLButtonElement;
        gate: HTMLElement;
        notice: HTMLElement;
        reset: HTMLButtonElement;
      }
    | undefined;

  /** The session behind the view, once the element is connected. */
  get session(): ChatSession | undefined {
    return this.#session;
  }

  connectedCallback(): void {
    if (this.#parts === undefined) {
      defineGraphrtUi();
      this.#build();
    }
    if (this.#session === undefined) {
      const server = this.getAttribute("server");
      const workflow = this.getAttribute("workflow");
      const storage = browserStorage();
      this.#session = new ChatSession({
        ...(server === null ? {} : { baseUrl: server }),
        ...(workflow === null ? {} : { workflow }),
        ...(this.userToken === undefined ? {} : { userToken: this.userToken }),
        ...(this.apiKey === undefined ? {} : { apiKey: this.apiKey }),
        ...(storage === undefined ? {} : { storage }),
      });
      this.#unsubscribe = this.#session.subscribe((state) => this.#render(state));
      this.#render(this.#session.state);
      void this.#session.start();
    }
  }

  disconnectedCallback(): void {
    this.#unsubscribe?.();
    this.#unsubscribe = undefined;
    this.#session = undefined;
    this.#views.clear();
    this.#parts?.list.replaceChildren(this.#parts.empty);
  }

  attributeChangedCallback(name: string, _old: string | null, value: string | null): void {
    if (name === "theme") {
      // Blocks already drawn follow the chat's theme too.
      for (const block of this.#parts?.list.querySelectorAll("graphrt-block") ?? []) {
        if (value === null) {
          block.removeAttribute("theme");
        } else {
          block.setAttribute("theme", value);
        }
      }
    }
    if (this.#session !== undefined) {
      this.#render(this.#session.state);
    }
  }

  #build(): void {
    const root = this.attachShadow({ mode: "open" });
    adoptStyles(root, CHAT_CSS);
    const title = make("h1");
    const reset = make(
      "button",
      { type: "button", class: "icon", title: "New chat", "aria-label": "New chat" },
      "New",
    );
    reset.addEventListener("click", () => {
      this.#session?.reset();
      this.#parts?.input.focus();
    });
    const close = make(
      "button",
      { type: "button", class: "icon", title: "Close", "aria-label": "Close" },
      "✕",
    );
    close.hidden = !this.hasAttribute("closable");
    close.addEventListener("click", () => {
      this.dispatchEvent(new CustomEvent("graphrt-chat-close", { bubbles: true, composed: true }));
    });
    const empty = make("p", { class: "empty" });
    const list = make(
      "div",
      { class: "messages", role: "log", "aria-live": "polite", "aria-relevant": "additions" },
      empty,
    );
    const input = make("textarea", { rows: "1", "aria-label": "Message", enterkeyhint: "send" });
    const send = make("button", { type: "submit", class: "primary" }, "Send");
    const stop = make("button", { type: "button", class: "primary stop" }, "Stop");
    stop.addEventListener("click", () => void this.#session?.stop());
    const composer = make("form", { class: "composer" }, input, send, stop);
    composer.addEventListener("submit", (event) => {
      event.preventDefault();
      this.#send();
    });
    input.addEventListener("keydown", (event) => {
      // Enter sends, Shift+Enter breaks the line. While an input method composes, Enter is its own.
      if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
        event.preventDefault();
        this.#send();
      }
    });
    input.addEventListener("input", () => this.#fitInput());
    const notice = make("p", { class: "note" });
    const gate = make("div", { class: "gate" }, notice);
    root.append(make("header", {}, title, reset, close), list, gate, composer);
    this.#parts = { title, list, empty, composer, input, send, stop, gate, notice, reset };
  }

  #fitInput(): void {
    const input = this.#parts?.input;
    if (input !== undefined) {
      input.style.height = "auto";
      input.style.height = `${Math.min(input.scrollHeight, 160)}px`;
      // A scroll bar only once the text is taller than the box may grow.
      input.style.overflowY = input.scrollHeight > 160 ? "auto" : "hidden";
    }
  }

  #send(): void {
    const input = this.#parts?.input;
    if (input === undefined || this.#session === undefined || this.#session.state.busy) {
      return;
    }
    const text = input.value;
    if (text.trim() === "") {
      return;
    }
    input.value = "";
    this.#fitInput();
    void this.#session.send(text);
  }

  /** The form that asks for the API key (a server with `--chat-access key`). */
  #keyForm(): HTMLFormElement {
    const key = make("input", {
      type: "password",
      autocomplete: "current-password",
      "aria-label": "API key",
      placeholder: "API key",
    });
    const remember = make("input", { type: "checkbox", id: "remember" });
    const form = make(
      "form",
      { class: "gate" },
      make("p", {}, "Enter the API key of this server to chat."),
      key,
      make(
        "div",
        { class: "check" },
        remember,
        make("label", { for: "remember" }, "Remember on this device"),
      ),
      make("button", { type: "submit", class: "primary" }, "Continue"),
    );
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (key.value !== "") {
        void this.#session?.unlock(key.value, remember.checked);
      }
    });
    return form;
  }

  #render(state: ChatState): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    parts.title.textContent = this.getAttribute("heading") ?? (state.title || "Chat");
    parts.empty.textContent = this.getAttribute("greeting") ?? "How can I help?";
    parts.input.placeholder = this.getAttribute("placeholder") ?? "Write a message";
    const ready = state.status === "ready";
    parts.composer.hidden = !ready;
    parts.list.hidden = !ready;
    parts.reset.hidden = !ready || state.messages.length === 0;
    parts.send.hidden = state.busy;
    parts.stop.hidden = !state.busy;
    parts.gate.hidden = ready;
    if (!ready) {
      const locked = state.status === "locked";
      const hasForm = parts.gate.querySelector("form") !== null;
      if (locked && !hasForm) {
        parts.gate.replaceChildren(this.#keyForm(), parts.notice);
      } else if (!locked && hasForm) {
        parts.gate.replaceChildren(parts.notice);
      }
      parts.notice.textContent = state.notice ?? (state.status === "loading" ? "Loading…" : "");
      parts.notice.className = state.status === "loading" ? "note" : "note error";
      return;
    }
    this.#renderMessages(state);
  }

  #renderMessages(state: ChatState): void {
    const parts = this.#parts;
    if (parts === undefined) {
      return;
    }
    const list = parts.list;
    const pinned = list.scrollHeight - list.scrollTop - list.clientHeight < 80;
    const known = new Set(state.messages.map((message) => message.id));
    for (const [id, view] of this.#views) {
      if (!known.has(id)) {
        view.root.remove();
        this.#views.delete(id);
      }
    }
    parts.empty.hidden = state.messages.length > 0;
    for (const message of state.messages) {
      let view = this.#views.get(message.id);
      if (view === undefined) {
        const bubble = make("div", { class: "bubble" });
        const extras = make("div", { class: "message assistant" });
        const root = make("div", { class: `message ${message.role}` }, bubble, extras);
        view = { root, bubble, extras, drawn: undefined };
        this.#views.set(message.id, view);
        list.append(root);
      }
      if (view.drawn !== message) {
        this.#renderMessage(view, message, state);
        view.drawn = message;
      }
    }
    // Follow the answer while the person is at the bottom, not while they read back.
    if (pinned) {
      list.scrollTop = list.scrollHeight;
    }
  }

  #renderMessage(view: MessageView, message: ChatMessage, state: ChatState): void {
    const before = view.drawn;
    if (before?.text !== message.text || before.state !== message.state) {
      if (message.role === "user") {
        view.bubble.textContent = message.text;
      } else if (message.text === "" && message.state === "streaming") {
        view.bubble.replaceChildren(
          make("span", { class: "typing", "aria-label": "Answering" }, make("i"), make("i"), make("i")),
        );
      } else {
        view.bubble.replaceChildren(...richText(message.text));
      }
      view.bubble.hidden = message.text === "" && message.state !== "streaming";
    }
    // The parts below the text change only with the message's state, not with every piece of text.
    const sameExtras =
      before !== undefined &&
      before.state === message.state &&
      before.blocks === message.blocks &&
      before.questions === message.questions &&
      before.error === message.error &&
      before.rating === message.rating;
    if (sameExtras) {
      return;
    }
    const extras: Node[] = [];
    const theme = this.getAttribute("theme");
    const block = (value: unknown): GraphrtBlockElement => {
      const element = document.createElement("graphrt-block") as GraphrtBlockElement;
      if (theme !== null) {
        element.setAttribute("theme", theme);
      }
      element.block = value;
      return element;
    };
    for (const value of message.blocks) {
      extras.push(block(value));
    }
    if (state.config?.diagrams && message.blocks.some((value) => uiBlockKind(value) === "diagram")) {
      void this.#drawDiagrams();
    }
    for (const question of message.questions) {
      const element = block(question.block);
      element.addEventListener("graphrt-action", (event) => {
        const action = (event as CustomEvent<BlockAction>).detail;
        void this.#session?.answer(message.id, question.key, action.id, action.data);
      });
      extras.push(element);
    }
    if (message.error !== undefined) {
      extras.push(
        make(
          "p",
          { class: message.state === "failed" ? "note error" : "note", role: "status" },
          message.error,
        ),
      );
    } else if (message.state === "stopped") {
      extras.push(make("p", { class: "note" }, "Stopped."));
    }
    if (
      message.role === "assistant" &&
      message.state === "done" &&
      message.runId !== undefined &&
      state.config?.feedback
    ) {
      const rate = (rating: "up" | "down", label: string, sign: string): HTMLButtonElement => {
        const button = make(
          "button",
          {
            type: "button",
            class: "icon",
            "aria-label": label,
            title: label,
            "aria-pressed": String(message.rating === rating),
          },
          sign,
        );
        button.addEventListener("click", () => void this.#session?.rate(message.id, rating));
        return button;
      };
      extras.push(
        make("div", { class: "tools" }, rate("up", "Helpful", "👍"), rate("down", "Not helpful", "👎")),
      );
    }
    view.extras.replaceChildren(...extras);
    view.extras.hidden = extras.length === 0;
  }

  /**
   * Load the server's Mermaid (once per page), then draw again the diagrams
   * that were shown as text while it loaded.
   */
  async #drawDiagrams(): Promise<void> {
    const server = this.getAttribute("server") ?? location.origin;
    if (!(await loadDiagrams(server))) {
      return;
    }
    for (const view of this.#views.values()) {
      for (const element of view.extras.querySelectorAll<GraphrtBlockElement>("graphrt-block")) {
        const shown: unknown = element.block;
        const drawnAsText = element.shadowRoot?.querySelector(".diagram pre") != null;
        if (uiBlockKind(shown) === "diagram" && drawnAsText) {
          // Setting the block draws it again, now with the renderer.
          element.block = shown;
        }
      }
    }
  }
}

/** Register `<graphrt-chat>` (and `<graphrt-block>`). Safe to call more than once. */
export function defineGraphrtChat(): void {
  defineGraphrtUi();
  if (typeof customElements !== "undefined" && customElements.get("graphrt-chat") === undefined) {
    customElements.define("graphrt-chat", GraphrtChatElement);
  }
}
