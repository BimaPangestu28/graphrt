/**
 * `graphrt/chat`: a chat for agents served by `graphrt serve`, as web components.
 *
 * - `<graphrt-chat>` is the chat itself, to place in a page.
 * - `<graphrt-chat-widget>` is a bubble in a corner that opens it.
 * - `ChatSession` is the logic without a view.
 *
 * A server started with `--chat WORKFLOW` serves both ready to use: the page
 * `/chat/` and the one script tag `/chat/embed.js`.
 */
export { defineGraphrtChat, GraphrtChatElement } from "./chat-element.ts";
export { loadDiagrams, type ScriptLoader } from "./diagrams.ts";
export {
  type ChatMessage,
  ChatSession,
  type ChatSessionOptions,
  type ChatState,
  type KeyValueStore,
  type PendingQuestion,
  questionBlock,
} from "./session.ts";
export { richText } from "./text.ts";
export { defineGraphrtChatWidget, GraphrtChatWidgetElement } from "./widget.ts";
