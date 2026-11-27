/**
 * The client of the `graphrt serve` HTTP API for browsers.
 *
 * It uses only `fetch`, `ReadableStream` and `AbortSignal`, so it also runs in
 * Node.js 18+, Deno, Bun and workers. Tokens travel in headers, never in URLs.
 */
import type * as z from "zod/mini";
import { ConsoleApi } from "./console.ts";
import { Approvals, Devices, Push } from "./monitoring.ts";
import {
  type AcceptedRun,
  abortedSchema,
  acceptedRunSchema,
  type ChatConfig,
  type Conversation,
  type ConversationPage,
  type ConversationSummary,
  chatConfigSchema,
  conversationPageSchema,
  conversationSchema,
  conversationSummarySchema,
  engineEventSchema,
  errorBodySchema,
  type GuestSession,
  guestSessionSchema,
  type Json,
  laggedSchema,
  type MonitorConfig,
  monitorConfigSchema,
  type ResponseChunk,
  type RunPage,
  type RunRecord,
  type RunResult,
  type RunStatus,
  responseChunkSchema,
  runAbortedSchema,
  runFailedSchema,
  runPageSchema,
  runPausedSchema,
  runRecordSchema,
  runResultSchema,
  runStartedSchema,
  runSucceededSchema,
  type Score,
  scoreSchema,
  storedEventsSchema,
} from "./schemas.ts";
import { readSse, type SseMessage } from "./sse.ts";

/** A token, or a function that returns the current one (called for every request). */
export type TokenSource = string | (() => string | undefined | Promise<string | undefined>);

export interface ClientOptions {
  /** Where the server is, e.g. `https://agent.example.com`. Default: the page's own origin. */
  baseUrl?: string;
  /** Sent as `Authorization: Bearer`: the API key, a guest token or a device token. */
  token?: TokenSource;
  /** An end-user JWT the server verifies (`--verify-jwt`), sent in `userTokenHeader`. */
  userToken?: TokenSource;
  /** The header mapped with `--input-header`. Default: `X-User-Authorization`. */
  userTokenHeader?: string;
  /** A `fetch` to use instead of the global one. */
  fetch?: typeof fetch;
}

/** The input of a run. */
export interface RunRequest {
  inputs?: Record<string, Json>;
  sys?: Record<string, Json>;
  conversation?: Record<string, Json>;
  /** Continue a conversation the server stores: its history becomes `sys.history`. */
  conversation_id?: string;
  metadata?: Record<string, string>;
  session_id?: string;
  user_id?: string;
  tags?: string[];
  /** References of uploaded workspace files, which become `sys.files`. */
  files?: string[];
}

/** The input of a resume: what each paused node waits for, by its `key`. */
export interface ResumeRequest {
  inputs?: Record<string, Json>;
  secrets?: Record<string, string>;
}

export interface CallOptions {
  /** Cancels the request. A blocking or streaming run is aborted by the server. */
  signal?: AbortSignal;
}

export interface EventsOptions extends CallOptions {
  /** Index of the first event to receive. Default: 0, the whole run. */
  from?: number | undefined;
  /** Also receive `response` events (the answer text) of the part that still executes. Default: true. */
  responses?: boolean;
  /** How often in a row a broken connection is reopened before giving up. Default: 8. */
  retries?: number;
  /** The first delay before reconnecting. It doubles up to 10 s. Default: 500 ms. */
  retryDelayMs?: number;
}

export interface ListRunsQuery {
  workflow?: string;
  status?: RunStatus;
  limit?: number;
  cursor?: string;
}

export interface FeedbackRequest {
  /** What is rated, e.g. `helpful`. */
  name: string;
  value: number | boolean | string;
  comment?: string;
  user?: string;
}

/** An event of a run as the client yields it. `id` is its place in the run's event log. */
export type RunEvent =
  | { type: "response"; id?: string; data: ResponseChunk }
  | { type: "run_started"; id?: string; data: z.infer<typeof runStartedSchema> }
  | { type: "run_succeeded"; id?: string; data: z.infer<typeof runSucceededSchema> }
  | { type: "run_failed"; id?: string; data: z.infer<typeof runFailedSchema> }
  | { type: "run_aborted"; id?: string; data: z.infer<typeof runAbortedSchema> }
  | { type: "run_paused"; id?: string; data: z.infer<typeof runPausedSchema> }
  | { type: "lagged"; data: z.infer<typeof laggedSchema> }
  /** Any other engine event (`node_started`, `node_chunk`...), named by `name`. */
  | { type: "engine"; name: string; id?: string; data: Record<string, Json> };

/** The events that end a stream. */
export type TerminalEvent = Extract<
  RunEvent,
  { type: "run_succeeded" | "run_failed" | "run_aborted" | "run_paused" }
>;

/**
 * Whether `event` is the last one of an execution.
 *
 * @example
 * for await (const event of client.stream("chat", request)) {
 *   if (isTerminal(event)) console.log(event.type);
 * }
 */
export function isTerminal(event: RunEvent): event is TerminalEvent {
  return (
    event.type === "run_succeeded" ||
    event.type === "run_failed" ||
    event.type === "run_aborted" ||
    event.type === "run_paused"
  );
}

/** A request the server refused, or an answer this client does not understand. */
export class GraphrtError extends Error {
  /** HTTP status, or 0 when no response arrived or its content was not understood. */
  readonly status: number;
  /** Seconds the server asks to wait (`Retry-After`), on `429` and `503`. */
  readonly retryAfterSeconds: number | undefined;

  constructor(message: string, status: number, retryAfterSeconds?: number) {
    super(message);
    this.name = "GraphrtError";
    this.status = status;
    this.retryAfterSeconds = retryAfterSeconds;
  }
}

const DEFAULT_USER_TOKEN_HEADER = "X-User-Authorization";
const MAX_RETRY_DELAY_MS = 10_000;

/** HTTP statuses worth another attempt when following events. */
function isTransient(status: number): boolean {
  return status === 408 || status === 429 || status >= 500;
}

function parseWith<T>(schema: z.ZodMiniType<T>, value: unknown, what: string): T {
  const parsed = schema.safeParse(value);
  if (!parsed.success) {
    const issue = parsed.error.issues[0];
    const where = issue ? ` at ${issue.path.join(".") || "(root)"}: ${issue.message}` : "";
    throw new GraphrtError(`unexpected ${what} from the server${where}`, 0);
  }
  return parsed.data;
}

function toRunEvent(message: SseMessage): RunEvent {
  let data: unknown;
  try {
    data = JSON.parse(message.data);
  } catch {
    throw new GraphrtError(`event \`${message.event}\` does not carry JSON`, 0);
  }
  const id = message.id === undefined ? {} : { id: message.id };
  const what = `\`${message.event}\` event`;
  switch (message.event) {
    case "response":
      return { type: "response", ...id, data: parseWith(responseChunkSchema, data, what) };
    case "run_started":
      return { type: "run_started", ...id, data: parseWith(runStartedSchema, data, what) };
    case "run_succeeded":
      return { type: "run_succeeded", ...id, data: parseWith(runSucceededSchema, data, what) };
    case "run_failed":
      return { type: "run_failed", ...id, data: parseWith(runFailedSchema, data, what) };
    case "run_aborted":
      return { type: "run_aborted", ...id, data: parseWith(runAbortedSchema, data, what) };
    case "run_paused":
      return { type: "run_paused", ...id, data: parseWith(runPausedSchema, data, what) };
    case "lagged":
      return { type: "lagged", data: parseWith(laggedSchema, data, what) };
    default:
      return { type: "engine", name: message.event, ...id, data: parseWith(engineEventSchema, data, what) };
  }
}

function sleep(ms: number, signal: AbortSignal | undefined): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(signal.reason);
      return;
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = (): void => {
      clearTimeout(timer);
      reject(signal?.reason);
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/** The conversations the server stores for the caller (`serve --conversations`). */
export class Conversations {
  readonly #client: GraphrtClient;

  /** @internal */
  constructor(client: GraphrtClient) {
    this.#client = client;
  }

  /**
   * Start a conversation with `workflow`.
   *
   * @param input - The start input that takes each message. Default: `sys.query`.
   * @throws GraphrtError - `404` when the server keeps no conversations, `429` at the caller's limit.
   */
  create(
    workflow: string,
    options: { input?: string; title?: string } & CallOptions = {},
  ): Promise<ConversationSummary> {
    const { signal, ...body } = options;
    return this.#client.request("POST", "/v1/conversations", conversationSummarySchema, {
      body: { workflow, ...body },
      signal,
    });
  }

  /** The caller's conversations, the most recently used first. */
  list(
    query: { workflow?: string; limit?: number; cursor?: string } & CallOptions = {},
  ): Promise<ConversationPage> {
    const { signal, ...params } = query;
    return this.#client.request("GET", "/v1/conversations", conversationPageSchema, {
      query: params,
      signal,
    });
  }

  /** One conversation with its messages, oldest first. */
  get(id: string, options: CallOptions = {}): Promise<Conversation> {
    return this.#client.request("GET", `/v1/conversations/${encodeURIComponent(id)}`, conversationSchema, {
      signal: options.signal,
    });
  }

  /** Delete a conversation and its messages. */
  async delete(id: string, options: CallOptions = {}): Promise<void> {
    await this.#client.request("DELETE", `/v1/conversations/${encodeURIComponent(id)}`, undefined, {
      signal: options.signal,
    });
  }
}

interface RequestOptions {
  body?: unknown;
  /** A body sent as it is (a file), instead of `body` as JSON. */
  raw?: Blob | ArrayBuffer | Uint8Array | string;
  query?: Record<string, string | number | boolean | undefined>;
  signal?: AbortSignal | undefined;
  headers?: Record<string, string>;
  /** Send no credentials (public routes). */
  anonymous?: boolean;
}

/**
 * A client of one `graphrt serve`.
 *
 * @example
 * const client = new GraphrtClient({ baseUrl: "https://agent.example.com", token: guestToken });
 * for await (const event of client.stream("support", { sys: { query: "Hi" } })) {
 *   if (event.type === "response") output.textContent += event.data.text;
 * }
 */
export class GraphrtClient {
  readonly #baseUrl: string;
  readonly #token: TokenSource | undefined;
  readonly #userToken: TokenSource | undefined;
  readonly #userTokenHeader: string;
  readonly #fetch: typeof fetch;
  /** Conversations kept by the server. */
  readonly conversations: Conversations;
  /** The runs that wait for a person (`serve --approvals`). */
  readonly approvals: Approvals;
  /** Paired monitoring devices (`serve --devices`). */
  readonly devices: Devices;
  /** Push notifications (`serve --push`). */
  readonly push: Push;
  /** The admin console (`serve --console`). */
  readonly console: ConsoleApi;

  constructor(options: ClientOptions = {}) {
    this.#baseUrl = (options.baseUrl ?? "").replace(/\/+$/, "");
    this.#token = options.token;
    this.#userToken = options.userToken;
    this.#userTokenHeader = options.userTokenHeader ?? DEFAULT_USER_TOKEN_HEADER;
    // Bound late: a page may replace `fetch` after the client was created.
    this.#fetch = options.fetch ?? ((input, init) => globalThis.fetch(input, init));
    this.conversations = new Conversations(this);
    this.approvals = new Approvals(this);
    this.devices = new Devices(this);
    this.push = new Push(this);
    this.console = new ConsoleApi(this);
  }

  /**
   * Run a workflow and wait for its result.
   *
   * @throws GraphrtError - When the server refuses the request (`401`, `404`, `429`...).
   */
  run(workflow: string, request: RunRequest = {}, options: CallOptions = {}): Promise<RunResult> {
    return this.request("POST", this.#runsPath(workflow), runResultSchema, {
      body: request,
      signal: options.signal,
    });
  }

  /**
   * Run a workflow and receive its events as they happen. The server aborts
   * the run when this stream is closed early: use {@link startAsync} with
   * {@link events} for a run that survives a lost connection.
   *
   * @throws GraphrtError - When the server refuses the request, or the stream
   *   ends before the run did.
   */
  stream(workflow: string, request: RunRequest = {}, options: CallOptions = {}): AsyncGenerator<RunEvent> {
    return this.#streamOnce("POST", this.#runsPath(workflow), { ...request, stream: true }, options.signal);
  }

  /**
   * Start a run in the background. Follow it with {@link events} or {@link getRun}.
   *
   * @throws GraphrtError - When the server refuses the request.
   */
  startAsync(workflow: string, request: RunRequest = {}, options: CallOptions = {}): Promise<AcceptedRun> {
    return this.request("POST", this.#runsPath(workflow), acceptedRunSchema, {
      body: { ...request, async: true },
      signal: options.signal,
    });
  }

  /**
   * The events of a run: the stored ones from `from`, then the live ones until
   * the run ends. A broken connection is reopened after the last event
   * received, so every event arrives exactly once.
   *
   * @throws GraphrtError - For an unknown run, a refused request, or when the
   *   connection cannot be reopened.
   */
  async *events(runId: string, options: EventsOptions = {}): AsyncGenerator<RunEvent> {
    const retries = options.retries ?? 8;
    const firstDelay = options.retryDelayMs ?? 500;
    const path = `/v1/runs/${encodeURIComponent(runId)}/events`;
    const query = { from: options.from, responses: (options.responses ?? true) ? true : undefined };
    let lastId: string | undefined;
    let failures = 0;
    for (;;) {
      let failure: unknown;
      try {
        const headers: Record<string, string> = lastId === undefined ? {} : { "Last-Event-ID": lastId };
        for await (const event of this.#sse("GET", path, { query, headers, signal: options.signal })) {
          failures = 0;
          if ("id" in event && event.id !== undefined) {
            lastId = event.id;
          }
          yield event;
          if (isTerminal(event)) {
            return;
          }
        }
        failure = new GraphrtError("the event stream ended before the run did", 0);
      } catch (error) {
        const refused = error instanceof GraphrtError && error.status !== 0 && !isTransient(error.status);
        if (refused || options.signal?.aborted) {
          throw error;
        }
        failure = error;
      }
      failures += 1;
      if (failures > retries) {
        throw failure;
      }
      await sleep(Math.min(firstDelay * 2 ** (failures - 1), MAX_RETRY_DELAY_MS), options.signal);
    }
  }

  /**
   * The stored events of a run as they are, from index `from` on: every
   * execution of it, a pause and what came after a resume included. Unlike
   * {@link GraphrtClient.events} it does not wait for a run that still executes.
   *
   * @throws GraphrtError - `404` for an unknown run (or one of another user).
   */
  listEvents(runId: string, options: { from?: number } & CallOptions = {}): Promise<Record<string, Json>[]> {
    return this.request("GET", `/v1/runs/${encodeURIComponent(runId)}/events`, storedEventsSchema, {
      query: { stream: false, from: options.from },
      signal: options.signal,
    });
  }

  /**
   * A stored run: its status, answer and outputs, and what it waits for when paused.
   *
   * @throws GraphrtError - `404` for an unknown run (or one of another user).
   */
  getRun(runId: string, options: CallOptions = {}): Promise<RunRecord> {
    return this.request("GET", `/v1/runs/${encodeURIComponent(runId)}`, runRecordSchema, {
      signal: options.signal,
    });
  }

  /**
   * Stored runs, newest first. Needs the API key.
   *
   * @throws GraphrtError - When the server refuses the request.
   */
  listRuns(query: ListRunsQuery & CallOptions = {}): Promise<RunPage> {
    const { signal, ...params } = query;
    return this.request("GET", "/v1/runs", runPageSchema, { query: params, signal });
  }

  /**
   * Stop a run that executes.
   *
   * @returns Whether a run was executing under this id.
   * @throws GraphrtError - When the server refuses the request.
   */
  async abort(runId: string, options: CallOptions = {}): Promise<boolean> {
    try {
      await this.request("POST", `/v1/runs/${encodeURIComponent(runId)}/abort`, abortedSchema, {
        signal: options.signal,
      });
      return true;
    } catch (error) {
      if (error instanceof GraphrtError && error.status === 404) {
        return false;
      }
      throw error;
    }
  }

  /**
   * Continue a paused run and wait for its result.
   *
   * @throws GraphrtError - `409` when the run was resumed already.
   */
  resume(runId: string, request: ResumeRequest = {}, options: CallOptions = {}): Promise<RunResult> {
    return this.request("POST", this.#resumePath(runId), runResultSchema, {
      body: request,
      signal: options.signal,
    });
  }

  /**
   * Continue a paused run and receive its events as they happen.
   *
   * @throws GraphrtError - `409` when the run was resumed already.
   */
  resumeStream(
    runId: string,
    request: ResumeRequest = {},
    options: CallOptions = {},
  ): AsyncGenerator<RunEvent> {
    return this.#streamOnce("POST", this.#resumePath(runId), { ...request, stream: true }, options.signal);
  }

  /**
   * Continue a paused run in the background. Follow it from the answer's
   * `first_event`: the events before it end with the pause.
   *
   * @example
   * const accepted = await client.resumeAsync(runId, { inputs });
   * for await (const event of client.events(runId, { from: accepted.first_event })) { ... }
   *
   * @throws GraphrtError - `409` when the run was resumed already.
   */
  resumeAsync(runId: string, request: ResumeRequest = {}, options: CallOptions = {}): Promise<AcceptedRun> {
    return this.request("POST", this.#resumePath(runId), acceptedRunSchema, {
      body: { ...request, async: true },
      signal: options.signal,
    });
  }

  /**
   * Rate a run (a thumbs up, a star count, a comment).
   *
   * @throws GraphrtError - `403` for a run of another user.
   */
  feedback(runId: string, feedback: FeedbackRequest, options: CallOptions = {}): Promise<Score> {
    return this.request("POST", `/v1/runs/${encodeURIComponent(runId)}/feedback`, scoreSchema, {
      body: feedback,
      signal: options.signal,
    });
  }

  /**
   * What the server's chat page offers. Needs no credentials.
   *
   * @throws GraphrtError - `404` when the server has no chat page (`serve --chat`).
   */
  chatConfig(options: CallOptions = {}): Promise<ChatConfig> {
    return this.request("GET", "/v1/chat/config", chatConfigSchema, {
      signal: options.signal,
      anonymous: true,
    });
  }

  /**
   * What the server's monitoring page offers. Needs no credentials.
   *
   * @throws GraphrtError - `404` when the server has no monitoring page (`serve --monitor`).
   */
  monitorConfig(options: CallOptions = {}): Promise<MonitorConfig> {
    return this.request("GET", "/v1/monitor/config", monitorConfigSchema, {
      signal: options.signal,
      anonymous: true,
    });
  }

  /**
   * Open a guest session of the chat page, or renew `previous` so the guest
   * keeps their conversations.
   *
   * @throws GraphrtError - `404` unless the server lets guests chat, `429` when too many sessions were opened.
   */
  guestSession(previous?: string, options: CallOptions = {}): Promise<GuestSession> {
    return this.request("POST", "/v1/chat/guest", guestSessionSchema, {
      body: {},
      signal: options.signal,
      anonymous: true,
      headers: previous === undefined ? {} : { Authorization: `Bearer ${previous}` },
    });
  }

  /**
   * Send one JSON request and check the answer against `schema`.
   *
   * @internal
   * @throws GraphrtError - On a status outside 2xx or an answer of another shape.
   */
  async request<T>(
    method: string,
    path: string,
    schema: z.ZodMiniType<T> | undefined,
    options: RequestOptions = {},
  ): Promise<T> {
    const response = await this.#send(method, path, options, "application/json");
    if (schema === undefined || response.status === 204) {
      await response.body?.cancel().catch(() => undefined);
      return undefined as T;
    }
    let body: unknown;
    try {
      body = await response.json();
    } catch {
      throw new GraphrtError(`${method} ${path}: the answer is not JSON`, 0);
    }
    return parseWith(schema, body, `answer to ${method} ${path}`);
  }

  #runsPath(workflow: string): string {
    return `/v1/workflows/${encodeURIComponent(workflow)}/runs`;
  }

  #resumePath(runId: string): string {
    return `/v1/runs/${encodeURIComponent(runId)}/resume`;
  }

  /** One SSE request whose stream must end with a terminal event. */
  async *#streamOnce(
    method: string,
    path: string,
    body: unknown,
    signal: AbortSignal | undefined,
  ): AsyncGenerator<RunEvent> {
    for await (const event of this.#sse(method, path, { body, signal })) {
      yield event;
      if (isTerminal(event)) {
        return;
      }
    }
    throw new GraphrtError("the stream ended before the run did", 0);
  }

  async *#sse(method: string, path: string, options: RequestOptions): AsyncGenerator<RunEvent> {
    const response = await this.#send(method, path, options, "text/event-stream");
    if (response.body === null) {
      throw new GraphrtError(`${method} ${path}: the answer has no body`, 0);
    }
    for await (const message of readSse(response.body)) {
      yield toRunEvent(message);
    }
  }

  async #send(method: string, path: string, options: RequestOptions, accept: string): Promise<Response> {
    const headers = new Headers({ Accept: accept });
    if (!options.anonymous) {
      await this.#authorize(headers);
    }
    for (const [name, value] of Object.entries(options.headers ?? {})) {
      headers.set(name, value);
    }
    const init: RequestInit = { method, headers };
    if (options.raw !== undefined) {
      headers.set("Content-Type", "application/octet-stream");
      init.body = options.raw as BodyInit;
    } else if (options.body !== undefined) {
      headers.set("Content-Type", "application/json");
      init.body = JSON.stringify(options.body);
    }
    if (options.signal) {
      init.signal = options.signal;
    }
    const response = await this.#fetch(this.#baseUrl + path + queryString(options.query), init);
    if (!response.ok) {
      throw await refusal(method, path, response);
    }
    return response;
  }

  async #authorize(headers: Headers): Promise<void> {
    const token = await resolveToken(this.#token);
    if (token) {
      headers.set("Authorization", `Bearer ${token}`);
    }
    const userToken = await resolveToken(this.#userToken);
    if (userToken) {
      headers.set(this.#userTokenHeader, `Bearer ${userToken}`);
    }
  }
}

async function resolveToken(source: TokenSource | undefined): Promise<string | undefined> {
  return typeof source === "function" ? await source() : source;
}

function queryString(query: RequestOptions["query"]): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query ?? {})) {
    if (value !== undefined) {
      params.set(key, String(value));
    }
  }
  const text = params.toString();
  return text === "" ? "" : `?${text}`;
}

async function refusal(method: string, path: string, response: Response): Promise<GraphrtError> {
  let message = `${method} ${path} answered ${response.status}`;
  try {
    const parsed = errorBodySchema.safeParse(await response.json());
    if (parsed.success) {
      message = parsed.data.error;
    }
  } catch {
    // Not JSON (a proxy's error page): the status says enough.
  }
  const retryAfter = Number(response.headers.get("Retry-After"));
  const seconds = Number.isFinite(retryAfter) && retryAfter > 0 ? retryAfter : undefined;
  return new GraphrtError(message, response.status, seconds);
}
