/**
 * The events of a chat. The same names everywhere:
 *
 * - `GraphrtChat.on("answer", fn)` of the widget script,
 * - DOM events `graphrt-chat-answer` (a `CustomEvent` whose `detail` is the
 *   payload) on `<graphrt-chat>` and `<graphrt-chat-widget>`,
 * - `ChatSession.on(fn)` without a view.
 */

/** The panel of the widget opened. */
export interface ChatOpenDetail {
  readonly workflow: string | undefined;
}

/** The panel of the widget closed (or the close button of `<graphrt-chat closable>` was pressed). */
export interface ChatCloseDetail {
  readonly workflow: string | undefined;
}

/** The chat connected and takes messages. Again after a key was entered. */
export interface ChatReadyDetail {
  readonly workflow: string;
  readonly title: string;
}

/** The visitor sent a message (typed, or with `send`). */
export interface ChatMessageDetail {
  /** The id of the message in the chat's state. */
  readonly id: string;
  readonly text: string;
}

/** An answer is complete. */
export interface ChatAnswerDetail {
  /** The id of the answer in the chat's state. */
  readonly id: string;
  readonly runId: string | undefined;
  readonly text: string;
  /** The UI blocks of the run's outputs, as the run returned them (validated when drawn). */
  readonly blocks: readonly unknown[];
}

/**
 * The answer waits for a person: `visitor` when the chat shows a question to
 * answer, `team` when someone else must approve it first.
 */
export interface ChatHandoffDetail {
  readonly id: string;
  readonly runId: string | undefined;
  readonly waitingFor: "visitor" | "team";
}

/** Something went wrong, in words for the visitor. */
export interface ChatErrorDetail {
  /** `connect`: the chat cannot be used. `answer`: one answer failed. */
  readonly during: "connect" | "answer";
  readonly message: string;
  /** The id of the failed answer, for `answer`. */
  readonly id?: string | undefined;
}

/** Every event by name, with its payload. */
export interface ChatEventMap {
  open: ChatOpenDetail;
  close: ChatCloseDetail;
  ready: ChatReadyDetail;
  message: ChatMessageDetail;
  answer: ChatAnswerDetail;
  handoff: ChatHandoffDetail;
  error: ChatErrorDetail;
}

export type ChatEventName = keyof ChatEventMap;

/** The events a session fires itself: `open` and `close` belong to a view. */
export type SessionEventName = Exclude<ChatEventName, "open" | "close">;

/** One event of a session. */
export type SessionEvent = {
  [K in SessionEventName]: { readonly type: K; readonly detail: ChatEventMap[K] };
}[SessionEventName];

export const CHAT_EVENT_NAMES: readonly ChatEventName[] = [
  "open",
  "close",
  "ready",
  "message",
  "answer",
  "handoff",
  "error",
];

/** The DOM event name of a chat event: `answer` -> `graphrt-chat-answer`. */
export function chatEventType<K extends ChatEventName>(name: K): `graphrt-chat-${K}` {
  return `graphrt-chat-${name}`;
}

/** Fire `name` on `target` as a DOM event that leaves the shadow tree. */
export function dispatchChatEvent<K extends ChatEventName>(
  target: EventTarget,
  name: K,
  detail: ChatEventMap[K],
): void {
  target.dispatchEvent(new CustomEvent(chatEventType(name), { detail, bubbles: true, composed: true }));
}

/**
 * Report an error of the page's own listener without stopping the chat: the
 * browser's `reportError` (shown like an uncaught error), else thrown in a
 * microtask of its own.
 */
export function reportListenerError(error: unknown): void {
  const report = (globalThis as { reportError?: (error: unknown) => void }).reportError;
  if (typeof report === "function") {
    report(error);
  } else {
    queueMicrotask(() => {
      throw error;
    });
  }
}

declare global {
  interface HTMLElementEventMap {
    "graphrt-chat-open": CustomEvent<ChatOpenDetail>;
    "graphrt-chat-close": CustomEvent<ChatCloseDetail>;
    "graphrt-chat-ready": CustomEvent<ChatReadyDetail>;
    "graphrt-chat-message": CustomEvent<ChatMessageDetail>;
    "graphrt-chat-answer": CustomEvent<ChatAnswerDetail>;
    "graphrt-chat-handoff": CustomEvent<ChatHandoffDetail>;
    "graphrt-chat-error": CustomEvent<ChatErrorDetail>;
  }
}
