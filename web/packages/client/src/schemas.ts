/**
 * Shapes of what `graphrt serve` answers, checked at runtime.
 *
 * Objects are loose: a newer server may add fields, and a client must not break
 * on them. Only the fields this package reads are checked.
 */
import * as z from "zod/mini";

/** JSON as the server sends it. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

const json: z.ZodMiniType<Json> = z.json();
const jsonObject = z.record(z.string(), json);

/** Token counts of a run. */
export const usageSchema = z.looseObject({
  prompt_tokens: z.optional(z.number()),
  completion_tokens: z.optional(z.number()),
  total_tokens: z.optional(z.number()),
});
export type Usage = z.infer<typeof usageSchema>;

/** Why a run paused: what a node waits for. */
export const pauseReasonSchema = z.looseObject({
  frame: z.optional(z.string()),
  node_id: z.optional(z.nullable(z.string())),
  kind: z.string(),
  data: z.optional(json),
});
export type PauseReason = z.infer<typeof pauseReasonSchema>;

/** What a paused node expects in the resume `inputs`, under `key`. */
export const resumeInputSchema = z.looseObject({
  key: z.string(),
  node_id: z.string(),
  kind: z.string(),
  data: z.optional(json),
});
export type ResumeInput = z.infer<typeof resumeInputSchema>;

export const runStatusSchema = z.enum(["running", "paused", "succeeded", "failed", "aborted"]);
export type RunStatus = z.infer<typeof runStatusSchema>;

/** The result of a blocking run start or resume. */
export const runResultSchema = z.looseObject({
  run_id: z.string(),
  status: z.enum(["succeeded", "failed", "aborted", "paused"]),
  outputs: z.optional(jsonObject),
  answer: z.optional(z.string()),
  usage: z.optional(usageSchema),
  error: z.optional(z.string()),
  error_type: z.optional(z.string()),
  reason: z.optional(z.string()),
  reasons: z.optional(z.array(pauseReasonSchema)),
  elapsed_ms: z.optional(z.number()),
});
export type RunResult = z.infer<typeof runResultSchema>;

/** The `202` answer of an async run start or resume. */
export const acceptedRunSchema = z.looseObject({
  run_id: z.string(),
  status: z.string(),
  run: z.string(),
  events: z.string(),
});
export type AcceptedRun = z.infer<typeof acceptedRunSchema>;

/** A stored run (`GET /v1/runs/{id}`). */
export const runRecordSchema = z.looseObject({
  run_id: z.string(),
  workflow: z.string(),
  status: runStatusSchema,
  created_ms: z.number(),
  updated_ms: z.number(),
  outputs: z.optional(jsonObject),
  answer: z.optional(z.string()),
  error: z.optional(z.nullable(z.string())),
  error_type: z.optional(z.nullable(z.string())),
  usage: z.optional(usageSchema),
  event_count: z.optional(z.number()),
  active: z.optional(z.boolean()),
  pause_reasons: z.optional(z.array(pauseReasonSchema)),
  resume_inputs: z.optional(z.array(resumeInputSchema)),
  session_id: z.optional(z.string()),
  user_id: z.optional(z.string()),
});
export type RunRecord = z.infer<typeof runRecordSchema>;

export const runPageSchema = z.looseObject({
  runs: z.array(runRecordSchema),
  next_cursor: z.optional(z.nullable(z.string())),
});
export type RunPage = z.infer<typeof runPageSchema>;

/** A stored score (`POST /v1/runs/{id}/feedback`). */
export const scoreSchema = z.looseObject({
  id: z.string(),
  name: z.string(),
  value: z.union([z.number(), z.boolean(), z.string()]),
  source: z.string(),
  comment: z.optional(z.string()),
  user: z.optional(z.string()),
});
export type Score = z.infer<typeof scoreSchema>;

export const abortedSchema = z.looseObject({ run_id: z.string(), status: z.string() });

/** One turn of a stored conversation. */
export const conversationMessageSchema = z.looseObject({
  role: z.enum(["user", "assistant"]),
  content: z.string(),
  run_id: z.optional(z.string()),
  created_ms: z.number(),
});
export type ConversationMessage = z.infer<typeof conversationMessageSchema>;

/** A conversation as listed (`GET /v1/conversations`). */
export const conversationSummarySchema = z.looseObject({
  id: z.string(),
  workflow: z.string(),
  title: z.string(),
  created_ms: z.number(),
  updated_ms: z.number(),
  message_count: z.number(),
});
export type ConversationSummary = z.infer<typeof conversationSummarySchema>;

/** A conversation with its messages (`GET /v1/conversations/{id}`). */
export const conversationSchema = z.extend(conversationSummarySchema, {
  messages: z.array(conversationMessageSchema),
});
export type Conversation = z.infer<typeof conversationSchema>;

export const conversationPageSchema = z.looseObject({
  conversations: z.array(conversationSummarySchema),
  next_cursor: z.optional(z.nullable(z.string())),
});
export type ConversationPage = z.infer<typeof conversationPageSchema>;

/** What the chat page of a server offers (`GET /v1/chat/config`). */
export const chatConfigSchema = z.looseObject({
  title: z.string(),
  workflows: z.array(
    z.looseObject({
      name: z.string(),
      /** The start input that takes the message, `null` when it is `sys.query`. */
      input: z.nullable(z.string()),
    }),
  ),
  /** Who may chat: holders of the API key, verified end users, or anyone (guests). */
  access: z.enum(["key", "users", "guests"]),
  /** The header that carries an end-user token, with `access: "users"`. */
  user_token_header: z.optional(z.nullable(z.string())),
  conversations: z.boolean(),
  /** Whether a user may answer the questions of their own paused runs. */
  resume: z.boolean(),
  feedback: z.boolean(),
});
export type ChatConfig = z.infer<typeof chatConfigSchema>;

/** A guest session of the chat page (`POST /v1/chat/guest`). */
export const guestSessionSchema = z.looseObject({
  token: z.string(),
  expires_ms: z.number(),
});
export type GuestSession = z.infer<typeof guestSessionSchema>;

export const errorBodySchema = z.looseObject({ error: z.string() });

/** The data of a `response` event: a piece of the ordered answer text. */
export const responseChunkSchema = z.looseObject({
  node_id: z.string(),
  text: z.string(),
  is_final: z.boolean(),
});
export type ResponseChunk = z.infer<typeof responseChunkSchema>;

export const runSucceededSchema = z.looseObject({
  outputs: z.optional(jsonObject),
  usage: z.optional(usageSchema),
  elapsed_ms: z.optional(z.number()),
});
export const runFailedSchema = z.looseObject({
  error: z.optional(z.string()),
  error_type: z.optional(z.string()),
  usage: z.optional(usageSchema),
  elapsed_ms: z.optional(z.number()),
});
export const runAbortedSchema = z.looseObject({
  reason: z.optional(z.string()),
  elapsed_ms: z.optional(z.number()),
});
export const runPausedSchema = z.looseObject({ reasons: z.array(pauseReasonSchema) });
export const runStartedSchema = z.looseObject({ run_id: z.string(), resumed: z.optional(z.boolean()) });
export const laggedSchema = z.looseObject({ skipped: z.number() });
export const engineEventSchema = jsonObject;
