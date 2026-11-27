/**
 * `window.GraphrtChat`, the API of the widget script `/chat/embed.js`.
 *
 * A page can call it before the script loaded, through a queue:
 *
 *   window.GraphrtChat = window.GraphrtChat || { q: [] };
 *   GraphrtChat.q.push(["init", { workflow: "support" }]);
 *   GraphrtChat.q.push((chat) => chat.on("answer", (answer) => console.log(answer.text)));
 *
 * When the script loads it runs the queue in order, then mounts the widget.
 * After that `GraphrtChat.q.push(...)` runs a call at once, so the same code
 * works whenever it runs.
 */
import type { TokenSource } from "../client/index.ts";
import { type ChatContext, checkChatContext } from "./context.ts";
import {
  CHAT_EVENT_NAMES,
  type ChatEventMap,
  type ChatEventName,
  chatEventType,
  reportListenerError,
} from "./events.ts";
import { defineGraphrtChatWidget, type GraphrtChatWidgetElement } from "./widget.ts";

/** `auto` follows the visitor's system setting. */
export type ChatTheme = "light" | "dark" | "auto";

/**
 * The options of `GraphrtChat.init`. The same names as the `data-` attributes
 * of the script tag (`data-workflow`, `data-heading`, ...), which they override.
 */
export interface GraphrtChatInitOptions {
  /** The workflow to chat with. Default: the first `--chat` workflow. */
  workflow?: string;
  /** Title of the panel. Default: `--chat-title`, else the workflow name. */
  heading?: string;
  /** Text shown before the first message. */
  greeting?: string;
  /** Placeholder of the message box. */
  placeholder?: string;
  theme?: ChatTheme;
  /** Corner of the bubble. Default: `right`. */
  position?: "right" | "left";
  /** Open the panel at once. */
  open?: boolean;
  /** The signed in user's token, for a server with `--chat-access users`. */
  userToken?: TokenSource;
  /** The page context, see {@link GraphrtChatApi.setContext}. */
  context?: ChatContext;
}

/** A listener of `GraphrtChat.on`: it gets the payload of the event. */
export type ChatEventListener<K extends ChatEventName> = (detail: ChatEventMap[K]) => void;

/** The API of the widget script, `window.GraphrtChat` once it loaded. */
export interface GraphrtChatApi {
  /** Set options. Before the chat first opens every option counts; later the look changes, not the workflow. */
  init(options: GraphrtChatInitOptions): void;
  open(): void;
  close(): void;
  toggle(): void;
  /**
   * Open the panel and send `text` as the visitor's message, once the chat is
   * ready. Calls made early are sent in order.
   *
   * @returns When the answer is complete.
   */
  send(text: string): Promise<void>;
  /**
   * Set what the workflow should know about the page: it reads it as
   * `sys.context` with every following message. `null` clears it.
   *
   * It comes from the browser, so a visitor can change it: never put
   * identity, secrets or permissions in it. A JSON object of at most 8 KiB
   * and 5 levels.
   *
   * @throws TypeError - When it is not such an object. The context before stays.
   */
  setContext(context: ChatContext | null): void;
  /** Start a new conversation. */
  reset(): Promise<void>;
  setTheme(theme: ChatTheme): void;
  /** The signed in user's token, or a function asked for it before every request. Set it before the chat opens. */
  setUserToken(token: TokenSource): void;
  /**
   * Call `listener` with the payload of every `name` event.
   *
   * @returns A function that removes the listener.
   * @throws TypeError - For an unknown event name.
   */
  on<K extends ChatEventName>(name: K, listener: ChatEventListener<K>): () => void;
  off<K extends ChatEventName>(name: K, listener: ChatEventListener<K>): void;
  /** Runs calls pushed to it at once (see {@link GraphrtChatCall}). */
  readonly q: GraphrtChatQueue;
  /** The widget element itself, `<graphrt-chat-widget>`. */
  readonly element: GraphrtChatWidgetElement;
}

type Method =
  | "init"
  | "open"
  | "close"
  | "toggle"
  | "send"
  | "setContext"
  | "reset"
  | "setTheme"
  | "setUserToken"
  | "on"
  | "off";

/**
 * One call in the queue: a function that gets the API, or a method name and
 * its arguments (`["setContext", { product_id: "sku-1" }]`).
 */
export type GraphrtChatCall =
  | ((chat: GraphrtChatApi) => void)
  | { [M in Method]: [M, ...Parameters<GraphrtChatApi[M]>] }[Method];

export interface GraphrtChatQueue {
  push(...calls: GraphrtChatCall[]): number;
}

/** What a page sets before the script loaded: `window.GraphrtChat = window.GraphrtChat || { q: [] }`. */
export interface GraphrtChatStub {
  q: GraphrtChatCall[];
}

/** The global that holds the API, or the stub before it loaded. */
export interface GraphrtChatHolder {
  GraphrtChat?: GraphrtChatApi | GraphrtChatStub | undefined;
}

const TEXT_OPTIONS = ["workflow", "heading", "greeting", "placeholder", "position"] as const;

function isApi(value: unknown): value is GraphrtChatApi {
  return typeof value === "object" && value !== null && "element" in value && "on" in value;
}

function setTheme(widget: GraphrtChatWidgetElement, theme: string | undefined): void {
  if (theme === "light" || theme === "dark") {
    widget.setAttribute("theme", theme);
  } else {
    widget.removeAttribute("theme");
  }
}

function createApi(widget: GraphrtChatWidgetElement): GraphrtChatApi {
  const listeners = new Map<ChatEventName, Map<(detail: never) => void, (event: Event) => void>>();
  const run = (call: GraphrtChatCall): void => {
    try {
      if (typeof call === "function") {
        call(api);
      } else {
        const [method, ...args] = call;
        (api[method] as (...values: unknown[]) => unknown)(...args);
      }
    } catch (error) {
      // An error of the page's code: reported, and the calls after it still run.
      reportListenerError(error);
    }
  };
  const api: GraphrtChatApi = {
    init(options) {
      for (const name of TEXT_OPTIONS) {
        const value = options[name];
        if (value !== undefined) {
          widget.setAttribute(name, value);
        }
      }
      if (options.theme !== undefined) {
        setTheme(widget, options.theme);
      }
      if (options.userToken !== undefined) {
        widget.userToken = options.userToken;
      }
      if (options.context !== undefined) {
        widget.setContext(checkChatContext(options.context));
      }
      if (options.open === true) {
        widget.open();
      } else if (options.open === false) {
        widget.close();
      }
    },
    open: () => widget.open(),
    close: () => widget.close(),
    toggle: () => widget.toggle(),
    send: (text) => widget.send(text),
    setContext: (context) => widget.setContext(context ?? undefined),
    reset: () => widget.reset(),
    setTheme: (theme) => setTheme(widget, theme),
    setUserToken: (token) => {
      widget.userToken = token;
    },
    on(name, listener) {
      if (!CHAT_EVENT_NAMES.includes(name)) {
        throw new TypeError(`Unknown chat event \`${String(name)}\`: ${CHAT_EVENT_NAMES.join(", ")}`);
      }
      let byListener = listeners.get(name);
      if (byListener === undefined) {
        byListener = new Map();
        listeners.set(name, byListener);
      }
      if (!byListener.has(listener)) {
        const wrapper = (event: Event): void => {
          listener((event as CustomEvent<ChatEventMap[typeof name]>).detail);
        };
        byListener.set(listener, wrapper);
        widget.addEventListener(chatEventType(name), wrapper);
      }
      return () => api.off(name, listener);
    },
    off(name, listener) {
      const wrapper = listeners.get(name)?.get(listener);
      if (wrapper !== undefined) {
        widget.removeEventListener(chatEventType(name), wrapper);
        listeners.get(name)?.delete(listener);
      }
    },
    q: {
      push(...calls) {
        for (const call of calls) {
          run(call);
        }
        return 0;
      },
    },
    element: widget,
  };
  return api;
}

/**
 * Mount the widget for the script tag `script` and set `holder.GraphrtChat`:
 * read its `data-` attributes, run the calls a page queued before, then add
 * the bubble to the page.
 *
 * @returns The API, or `undefined` when the page already has one (the script
 *   was included twice) or there is no script tag to read.
 */
export function mountEmbed(
  script: HTMLScriptElement | null,
  holder: GraphrtChatHolder,
): GraphrtChatApi | undefined {
  const existing = holder.GraphrtChat;
  if (isApi(existing) || script === null) {
    // Included twice: one bubble is enough.
    return undefined;
  }
  const queued = Array.isArray(existing?.q) ? existing.q : [];
  defineGraphrtChatWidget();
  const widget = document.createElement("graphrt-chat-widget") as GraphrtChatWidgetElement;
  // `<server>/chat/embed.js` -> `<server>`, also below a path prefix of a proxy.
  widget.setAttribute("server", new URL("..", script.src).href.replace(/\/$/, ""));
  const api = createApi(widget);
  const data = script.dataset;
  for (const name of TEXT_OPTIONS) {
    const value = data[name];
    if (value !== undefined) {
      widget.setAttribute(name, value);
    }
  }
  if (data.theme !== undefined) {
    setTheme(widget, data.theme);
  }
  if (data.open !== undefined && data.open !== "false") {
    widget.open();
  }
  holder.GraphrtChat = api;
  // The queue runs before the bubble is in the page, so `init` options and
  // listeners are in place before anything shows or connects.
  api.q.push(...queued);
  const attach = (): void => {
    document.body.append(widget);
  };
  if (document.body) {
    attach();
  } else {
    document.addEventListener("DOMContentLoaded", attach, { once: true });
  }
  return api;
}
