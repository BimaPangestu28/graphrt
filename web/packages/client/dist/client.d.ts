/**
 * The client of the `graphrt serve` HTTP API for browsers.
 *
 * It uses only `fetch`, `ReadableStream` and `AbortSignal`, so it also runs in
 * Node.js 18+, Deno, Bun and workers. Tokens travel in headers, never in URLs.
 */
import type * as z from "zod/mini";
import { Approvals, Devices, Push } from "./monitoring.ts";
import { type AcceptedRun, type ChatConfig, type Conversation, type ConversationPage, type ConversationSummary, type GuestSession, type Json, laggedSchema, type MonitorConfig, type ResponseChunk, type RunPage, type RunRecord, type RunResult, type RunStatus, runAbortedSchema, runFailedSchema, runPausedSchema, runStartedSchema, runSucceededSchema, type Score } from "./schemas.ts";
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
export type RunEvent = {
    type: "response";
    id?: string;
    data: ResponseChunk;
} | {
    type: "run_started";
    id?: string;
    data: z.infer<typeof runStartedSchema>;
} | {
    type: "run_succeeded";
    id?: string;
    data: z.infer<typeof runSucceededSchema>;
} | {
    type: "run_failed";
    id?: string;
    data: z.infer<typeof runFailedSchema>;
} | {
    type: "run_aborted";
    id?: string;
    data: z.infer<typeof runAbortedSchema>;
} | {
    type: "run_paused";
    id?: string;
    data: z.infer<typeof runPausedSchema>;
} | {
    type: "lagged";
    data: z.infer<typeof laggedSchema>;
}
/** Any other engine event (`node_started`, `node_chunk`...), named by `name`. */
 | {
    type: "engine";
    name: string;
    id?: string;
    data: Record<string, Json>;
};
/** The events that end a stream. */
export type TerminalEvent = Extract<RunEvent, {
    type: "run_succeeded" | "run_failed" | "run_aborted" | "run_paused";
}>;
/**
 * Whether `event` is the last one of an execution.
 *
 * @example
 * for await (const event of client.stream("chat", request)) {
 *   if (isTerminal(event)) console.log(event.type);
 * }
 */
export declare function isTerminal(event: RunEvent): event is TerminalEvent;
/** A request the server refused, or an answer this client does not understand. */
export declare class GraphrtError extends Error {
    /** HTTP status, or 0 when no response arrived or its content was not understood. */
    readonly status: number;
    /** Seconds the server asks to wait (`Retry-After`), on `429` and `503`. */
    readonly retryAfterSeconds: number | undefined;
    constructor(message: string, status: number, retryAfterSeconds?: number);
}
/** The conversations the server stores for the caller (`serve --conversations`). */
export declare class Conversations {
    #private;
    /** @internal */
    constructor(client: GraphrtClient);
    /**
     * Start a conversation with `workflow`.
     *
     * @param input - The start input that takes each message. Default: `sys.query`.
     * @throws GraphrtError - `404` when the server keeps no conversations, `429` at the caller's limit.
     */
    create(workflow: string, options?: {
        input?: string;
        title?: string;
    } & CallOptions): Promise<ConversationSummary>;
    /** The caller's conversations, the most recently used first. */
    list(query?: {
        workflow?: string;
        limit?: number;
        cursor?: string;
    } & CallOptions): Promise<ConversationPage>;
    /** One conversation with its messages, oldest first. */
    get(id: string, options?: CallOptions): Promise<Conversation>;
    /** Delete a conversation and its messages. */
    delete(id: string, options?: CallOptions): Promise<void>;
}
interface RequestOptions {
    body?: unknown;
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
export declare class GraphrtClient {
    #private;
    /** Conversations kept by the server. */
    readonly conversations: Conversations;
    /** The runs that wait for a person (`serve --approvals`). */
    readonly approvals: Approvals;
    /** Paired monitoring devices (`serve --devices`). */
    readonly devices: Devices;
    /** Push notifications (`serve --push`). */
    readonly push: Push;
    constructor(options?: ClientOptions);
    /**
     * Run a workflow and wait for its result.
     *
     * @throws GraphrtError - When the server refuses the request (`401`, `404`, `429`...).
     */
    run(workflow: string, request?: RunRequest, options?: CallOptions): Promise<RunResult>;
    /**
     * Run a workflow and receive its events as they happen. The server aborts
     * the run when this stream is closed early: use {@link startAsync} with
     * {@link events} for a run that survives a lost connection.
     *
     * @throws GraphrtError - When the server refuses the request, or the stream
     *   ends before the run did.
     */
    stream(workflow: string, request?: RunRequest, options?: CallOptions): AsyncGenerator<RunEvent>;
    /**
     * Start a run in the background. Follow it with {@link events} or {@link getRun}.
     *
     * @throws GraphrtError - When the server refuses the request.
     */
    startAsync(workflow: string, request?: RunRequest, options?: CallOptions): Promise<AcceptedRun>;
    /**
     * The events of a run: the stored ones from `from`, then the live ones until
     * the run ends. A broken connection is reopened after the last event
     * received, so every event arrives exactly once.
     *
     * @throws GraphrtError - For an unknown run, a refused request, or when the
     *   connection cannot be reopened.
     */
    events(runId: string, options?: EventsOptions): AsyncGenerator<RunEvent>;
    /**
     * The stored events of a run as they are, from index `from` on: every
     * execution of it, a pause and what came after a resume included. Unlike
     * {@link GraphrtClient.events} it does not wait for a run that still executes.
     *
     * @throws GraphrtError - `404` for an unknown run (or one of another user).
     */
    listEvents(runId: string, options?: {
        from?: number;
    } & CallOptions): Promise<Record<string, Json>[]>;
    /**
     * A stored run: its status, answer and outputs, and what it waits for when paused.
     *
     * @throws GraphrtError - `404` for an unknown run (or one of another user).
     */
    getRun(runId: string, options?: CallOptions): Promise<RunRecord>;
    /**
     * Stored runs, newest first. Needs the API key.
     *
     * @throws GraphrtError - When the server refuses the request.
     */
    listRuns(query?: ListRunsQuery & CallOptions): Promise<RunPage>;
    /**
     * Stop a run that executes.
     *
     * @returns Whether a run was executing under this id.
     * @throws GraphrtError - When the server refuses the request.
     */
    abort(runId: string, options?: CallOptions): Promise<boolean>;
    /**
     * Continue a paused run and wait for its result.
     *
     * @throws GraphrtError - `409` when the run was resumed already.
     */
    resume(runId: string, request?: ResumeRequest, options?: CallOptions): Promise<RunResult>;
    /**
     * Continue a paused run and receive its events as they happen.
     *
     * @throws GraphrtError - `409` when the run was resumed already.
     */
    resumeStream(runId: string, request?: ResumeRequest, options?: CallOptions): AsyncGenerator<RunEvent>;
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
    resumeAsync(runId: string, request?: ResumeRequest, options?: CallOptions): Promise<AcceptedRun>;
    /**
     * Rate a run (a thumbs up, a star count, a comment).
     *
     * @throws GraphrtError - `403` for a run of another user.
     */
    feedback(runId: string, feedback: FeedbackRequest, options?: CallOptions): Promise<Score>;
    /**
     * What the server's chat page offers. Needs no credentials.
     *
     * @throws GraphrtError - `404` when the server has no chat page (`serve --chat`).
     */
    chatConfig(options?: CallOptions): Promise<ChatConfig>;
    /**
     * What the server's monitoring page offers. Needs no credentials.
     *
     * @throws GraphrtError - `404` when the server has no monitoring page (`serve --monitor`).
     */
    monitorConfig(options?: CallOptions): Promise<MonitorConfig>;
    /**
     * Open a guest session of the chat page, or renew `previous` so the guest
     * keeps their conversations.
     *
     * @throws GraphrtError - `404` unless the server lets guests chat, `429` when too many sessions were opened.
     */
    guestSession(previous?: string, options?: CallOptions): Promise<GuestSession>;
    /**
     * Send one JSON request and check the answer against `schema`.
     *
     * @internal
     * @throws GraphrtError - On a status outside 2xx or an answer of another shape.
     */
    request<T>(method: string, path: string, schema: z.ZodMiniType<T> | undefined, options?: RequestOptions): Promise<T>;
}
export {};
