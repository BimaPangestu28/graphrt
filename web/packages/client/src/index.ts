/**
 * `graphrt-client`: a typed client of `graphrt serve` for browsers.
 *
 * @example
 * import { GraphrtClient } from "graphrt-client";
 *
 * const client = new GraphrtClient({ baseUrl: "https://agent.example.com", token: apiKey });
 * const result = await client.run("hello", { inputs: { name: "Ada" } });
 */
export {
  type CallOptions,
  type ClientOptions,
  Conversations,
  type EventsOptions,
  type FeedbackRequest,
  GraphrtClient,
  GraphrtError,
  isTerminal,
  type ListRunsQuery,
  type ResumeRequest,
  type RunEvent,
  type RunRequest,
  type TerminalEvent,
  type TokenSource,
} from "./client.ts";
export type {
  AcceptedRun,
  ChatConfig,
  Conversation,
  ConversationMessage,
  ConversationPage,
  ConversationSummary,
  GuestSession,
  Json,
  PauseReason,
  ResponseChunk,
  ResumeInput,
  RunPage,
  RunRecord,
  RunResult,
  RunStatus,
  Score,
  Usage,
} from "./schemas.ts";
export { readSse, type SseMessage, SseParser } from "./sse.ts";
