/**
 * `@graphrt-dev/graphrt/chat`: a chat for agents served by `graphrt serve`, as web components.
 *
 * - `<graphrt-chat>` is the chat itself, to place in a page.
 * - `<graphrt-chat-widget>` is a bubble in a corner that opens it.
 * - `ChatSession` is the logic without a view.
 * - `mountEmbed` and the `GraphrtChat*` types are the API of the widget script.
 *
 * A server started with `--chat WORKFLOW` serves both ready to use: the page
 * `/chat/` and the one script tag `/chat/embed.js`.
 */
export { defineGraphrtChat, GraphrtChatElement } from "./chat-element.ts";
export {
  type ChatContext,
  type ChatContextValue,
  checkChatContext,
  MAX_CONTEXT_BYTES,
  MAX_CONTEXT_DEPTH,
} from "./context.ts";
export { loadDiagrams, type ScriptLoader } from "./diagrams.ts";
export {
  type ChatEventListener,
  type ChatTheme,
  type GraphrtChatApi,
  type GraphrtChatCall,
  type GraphrtChatHolder,
  type GraphrtChatInitOptions,
  type GraphrtChatQueue,
  type GraphrtChatStub,
  mountEmbed,
} from "./embed-api.ts";
export {
  CHAT_EVENT_NAMES,
  type ChatAnswerDetail,
  type ChatCloseDetail,
  type ChatErrorDetail,
  type ChatEventMap,
  type ChatEventName,
  type ChatHandoffDetail,
  type ChatMessageDetail,
  type ChatOpenDetail,
  type ChatReadyDetail,
  chatEventType,
  type SessionEvent,
  type SessionEventName,
} from "./events.ts";
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
