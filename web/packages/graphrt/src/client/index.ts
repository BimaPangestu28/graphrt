/**
 * `@graphrt-dev/graphrt/client`: a typed client of `graphrt serve` for browsers.
 *
 * @example
 * import { GraphrtClient } from "@graphrt-dev/graphrt/client";
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
export {
  type AuditEntry,
  type AuditPage,
  type AuditQuery,
  type AuditVerification,
  ConsoleApi,
  type ConsoleRole,
  type ConsoleSession,
  CSRF_HEADER,
  type Deployment,
  type Member,
  type MembersView,
  type RegistryWorkflow,
  type SecretsView,
  type ServedWorkflow,
  type SessionInfo,
  sessionFetch,
  type UploadOptions,
  type VersionChange,
  type WorkflowsView,
  type WorkflowVersion,
} from "./console.ts";
export { Approvals, Devices, type PairDeviceRequest, Push, type PushSubscriptionKeys } from "./monitoring.ts";
export type {
  AcceptedRun,
  Approval,
  ApprovalPage,
  ApprovalRequest,
  ChatConfig,
  Conversation,
  ConversationMessage,
  ConversationPage,
  ConversationSummary,
  Device,
  DeviceScope,
  GuestSession,
  Json,
  MonitorConfig,
  PairedDevice,
  PauseReason,
  PushInfo,
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
