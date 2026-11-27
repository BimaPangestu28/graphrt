/**
 * Shapes of what `graphrt serve` answers, checked at runtime.
 *
 * Objects are loose: a newer server may add fields, and a client must not break
 * on them. Only the fields this package reads are checked.
 */
import * as z from "zod/mini";
/** JSON as the server sends it. */
export type Json = null | boolean | number | string | Json[] | {
    [key: string]: Json;
};
/** Token counts of a run. */
export declare const usageSchema: z.ZodMiniObject<{
    prompt_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
    completion_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
    total_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
}, z.core.$loose>;
export type Usage = z.infer<typeof usageSchema>;
/** Why a run paused: what a node waits for. */
export declare const pauseReasonSchema: z.ZodMiniObject<{
    frame: z.ZodMiniOptional<z.ZodMiniString<string>>;
    node_id: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
    kind: z.ZodMiniString<string>;
    data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
}, z.core.$loose>;
export type PauseReason = z.infer<typeof pauseReasonSchema>;
/** What a paused node expects in the resume `inputs`, under `key`. */
export declare const resumeInputSchema: z.ZodMiniObject<{
    key: z.ZodMiniString<string>;
    node_id: z.ZodMiniString<string>;
    kind: z.ZodMiniString<string>;
    data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
}, z.core.$loose>;
export type ResumeInput = z.infer<typeof resumeInputSchema>;
export declare const runStatusSchema: z.ZodMiniEnum<{
    running: "running";
    paused: "paused";
    succeeded: "succeeded";
    failed: "failed";
    aborted: "aborted";
}>;
export type RunStatus = z.infer<typeof runStatusSchema>;
/** The result of a blocking run start or resume. */
export declare const runResultSchema: z.ZodMiniObject<{
    run_id: z.ZodMiniString<string>;
    status: z.ZodMiniEnum<{
        paused: "paused";
        succeeded: "succeeded";
        failed: "failed";
        aborted: "aborted";
    }>;
    outputs: z.ZodMiniOptional<z.ZodMiniRecord<z.ZodMiniString<string>, z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>>;
    answer: z.ZodMiniOptional<z.ZodMiniString<string>>;
    usage: z.ZodMiniOptional<z.ZodMiniObject<{
        prompt_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        completion_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        total_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
    }, z.core.$loose>>;
    error: z.ZodMiniOptional<z.ZodMiniString<string>>;
    error_type: z.ZodMiniOptional<z.ZodMiniString<string>>;
    reason: z.ZodMiniOptional<z.ZodMiniString<string>>;
    reasons: z.ZodMiniOptional<z.ZodMiniArray<z.ZodMiniObject<{
        frame: z.ZodMiniOptional<z.ZodMiniString<string>>;
        node_id: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
        kind: z.ZodMiniString<string>;
        data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
    }, z.core.$loose>>>;
    elapsed_ms: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
}, z.core.$loose>;
export type RunResult = z.infer<typeof runResultSchema>;
/** The `202` answer of an async run start or resume. */
export declare const acceptedRunSchema: z.ZodMiniObject<{
    run_id: z.ZodMiniString<string>;
    status: z.ZodMiniString<string>;
    run: z.ZodMiniString<string>;
    events: z.ZodMiniString<string>;
    first_event: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
}, z.core.$loose>;
export type AcceptedRun = z.infer<typeof acceptedRunSchema>;
/** A stored run (`GET /v1/runs/{id}`). */
export declare const runRecordSchema: z.ZodMiniObject<{
    run_id: z.ZodMiniString<string>;
    workflow: z.ZodMiniString<string>;
    status: z.ZodMiniEnum<{
        running: "running";
        paused: "paused";
        succeeded: "succeeded";
        failed: "failed";
        aborted: "aborted";
    }>;
    created_ms: z.ZodMiniNumber<number>;
    updated_ms: z.ZodMiniNumber<number>;
    outputs: z.ZodMiniOptional<z.ZodMiniRecord<z.ZodMiniString<string>, z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>>;
    answer: z.ZodMiniOptional<z.ZodMiniString<string>>;
    error: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
    error_type: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
    usage: z.ZodMiniOptional<z.ZodMiniObject<{
        prompt_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        completion_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        total_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
    }, z.core.$loose>>;
    event_count: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
    active: z.ZodMiniOptional<z.ZodMiniBoolean<boolean>>;
    pause_reasons: z.ZodMiniOptional<z.ZodMiniArray<z.ZodMiniObject<{
        frame: z.ZodMiniOptional<z.ZodMiniString<string>>;
        node_id: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
        kind: z.ZodMiniString<string>;
        data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
    }, z.core.$loose>>>;
    resume_inputs: z.ZodMiniOptional<z.ZodMiniArray<z.ZodMiniObject<{
        key: z.ZodMiniString<string>;
        node_id: z.ZodMiniString<string>;
        kind: z.ZodMiniString<string>;
        data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
    }, z.core.$loose>>>;
    session_id: z.ZodMiniOptional<z.ZodMiniString<string>>;
    user_id: z.ZodMiniOptional<z.ZodMiniString<string>>;
}, z.core.$loose>;
export type RunRecord = z.infer<typeof runRecordSchema>;
export declare const runPageSchema: z.ZodMiniObject<{
    runs: z.ZodMiniArray<z.ZodMiniObject<{
        run_id: z.ZodMiniString<string>;
        workflow: z.ZodMiniString<string>;
        status: z.ZodMiniEnum<{
            running: "running";
            paused: "paused";
            succeeded: "succeeded";
            failed: "failed";
            aborted: "aborted";
        }>;
        created_ms: z.ZodMiniNumber<number>;
        updated_ms: z.ZodMiniNumber<number>;
        outputs: z.ZodMiniOptional<z.ZodMiniRecord<z.ZodMiniString<string>, z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>>;
        answer: z.ZodMiniOptional<z.ZodMiniString<string>>;
        error: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
        error_type: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
        usage: z.ZodMiniOptional<z.ZodMiniObject<{
            prompt_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
            completion_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
            total_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        }, z.core.$loose>>;
        event_count: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        active: z.ZodMiniOptional<z.ZodMiniBoolean<boolean>>;
        pause_reasons: z.ZodMiniOptional<z.ZodMiniArray<z.ZodMiniObject<{
            frame: z.ZodMiniOptional<z.ZodMiniString<string>>;
            node_id: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
            kind: z.ZodMiniString<string>;
            data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
        }, z.core.$loose>>>;
        resume_inputs: z.ZodMiniOptional<z.ZodMiniArray<z.ZodMiniObject<{
            key: z.ZodMiniString<string>;
            node_id: z.ZodMiniString<string>;
            kind: z.ZodMiniString<string>;
            data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
        }, z.core.$loose>>>;
        session_id: z.ZodMiniOptional<z.ZodMiniString<string>>;
        user_id: z.ZodMiniOptional<z.ZodMiniString<string>>;
    }, z.core.$loose>>;
    next_cursor: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
}, z.core.$loose>;
export type RunPage = z.infer<typeof runPageSchema>;
/** A stored score (`POST /v1/runs/{id}/feedback`). */
export declare const scoreSchema: z.ZodMiniObject<{
    id: z.ZodMiniString<string>;
    name: z.ZodMiniString<string>;
    value: z.ZodMiniUnion<readonly [z.ZodMiniNumber<number>, z.ZodMiniBoolean<boolean>, z.ZodMiniString<string>]>;
    source: z.ZodMiniString<string>;
    comment: z.ZodMiniOptional<z.ZodMiniString<string>>;
    user: z.ZodMiniOptional<z.ZodMiniString<string>>;
}, z.core.$loose>;
export type Score = z.infer<typeof scoreSchema>;
export declare const abortedSchema: z.ZodMiniObject<{
    run_id: z.ZodMiniString<string>;
    status: z.ZodMiniString<string>;
}, z.core.$loose>;
/** One turn of a stored conversation. */
export declare const conversationMessageSchema: z.ZodMiniObject<{
    role: z.ZodMiniEnum<{
        user: "user";
        assistant: "assistant";
    }>;
    content: z.ZodMiniString<string>;
    run_id: z.ZodMiniOptional<z.ZodMiniString<string>>;
    created_ms: z.ZodMiniNumber<number>;
}, z.core.$loose>;
export type ConversationMessage = z.infer<typeof conversationMessageSchema>;
/** A conversation as listed (`GET /v1/conversations`). */
export declare const conversationSummarySchema: z.ZodMiniObject<{
    id: z.ZodMiniString<string>;
    workflow: z.ZodMiniString<string>;
    title: z.ZodMiniString<string>;
    created_ms: z.ZodMiniNumber<number>;
    updated_ms: z.ZodMiniNumber<number>;
    message_count: z.ZodMiniNumber<number>;
}, z.core.$loose>;
export type ConversationSummary = z.infer<typeof conversationSummarySchema>;
/** A conversation with its messages (`GET /v1/conversations/{id}`). */
export declare const conversationSchema: z.ZodMiniObject<{
    id: z.ZodMiniString<string>;
    workflow: z.ZodMiniString<string>;
    title: z.ZodMiniString<string>;
    created_ms: z.ZodMiniNumber<number>;
    updated_ms: z.ZodMiniNumber<number>;
    message_count: z.ZodMiniNumber<number>;
    messages: z.ZodMiniArray<z.ZodMiniObject<{
        role: z.ZodMiniEnum<{
            user: "user";
            assistant: "assistant";
        }>;
        content: z.ZodMiniString<string>;
        run_id: z.ZodMiniOptional<z.ZodMiniString<string>>;
        created_ms: z.ZodMiniNumber<number>;
    }, z.core.$loose>>;
}, z.core.$loose>;
export type Conversation = z.infer<typeof conversationSchema>;
export declare const conversationPageSchema: z.ZodMiniObject<{
    conversations: z.ZodMiniArray<z.ZodMiniObject<{
        id: z.ZodMiniString<string>;
        workflow: z.ZodMiniString<string>;
        title: z.ZodMiniString<string>;
        created_ms: z.ZodMiniNumber<number>;
        updated_ms: z.ZodMiniNumber<number>;
        message_count: z.ZodMiniNumber<number>;
    }, z.core.$loose>>;
    next_cursor: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
}, z.core.$loose>;
export type ConversationPage = z.infer<typeof conversationPageSchema>;
/** What the chat page of a server offers (`GET /v1/chat/config`). */
export declare const chatConfigSchema: z.ZodMiniObject<{
    title: z.ZodMiniString<string>;
    workflows: z.ZodMiniArray<z.ZodMiniObject<{
        name: z.ZodMiniString<string>;
        input: z.ZodMiniNullable<z.ZodMiniString<string>>;
    }, z.core.$loose>>;
    access: z.ZodMiniEnum<{
        key: "key";
        users: "users";
        guests: "guests";
    }>;
    user_token_header: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
    conversations: z.ZodMiniBoolean<boolean>;
    resume: z.ZodMiniBoolean<boolean>;
    feedback: z.ZodMiniBoolean<boolean>;
    diagrams: z.ZodMiniOptional<z.ZodMiniBoolean<boolean>>;
}, z.core.$loose>;
export type ChatConfig = z.infer<typeof chatConfigSchema>;
/** A guest session of the chat page (`POST /v1/chat/guest`). */
export declare const guestSessionSchema: z.ZodMiniObject<{
    token: z.ZodMiniString<string>;
    expires_ms: z.ZodMiniNumber<number>;
}, z.core.$loose>;
export type GuestSession = z.infer<typeof guestSessionSchema>;
/** The stored events of a run (`GET /v1/runs/{id}/events?stream=false`). */
export declare const storedEventsSchema: z.ZodMiniArray<z.ZodMiniRecord<z.ZodMiniString<string>, z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>>;
/** What the monitoring page of a server offers (`GET /v1/monitor/config`). */
export declare const monitorConfigSchema: z.ZodMiniObject<{
    access: z.ZodMiniEnum<{
        key: "key";
        open: "open";
    }>;
    devices: z.ZodMiniBoolean<boolean>;
    approvals: z.ZodMiniBoolean<boolean>;
    expire_approvals: z.ZodMiniOptional<z.ZodMiniBoolean<boolean>>;
    push: z.ZodMiniBoolean<boolean>;
}, z.core.$loose>;
export type MonitorConfig = z.infer<typeof monitorConfigSchema>;
/** One node of a paused run that waits for a person. */
export declare const approvalRequestSchema: z.ZodMiniObject<{
    key: z.ZodMiniString<string>;
    node_id: z.ZodMiniString<string>;
    kind: z.ZodMiniString<string>;
    data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
    expires_ms: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniNumber<number>>>;
}, z.core.$loose>;
export type ApprovalRequest = z.infer<typeof approvalRequestSchema>;
/** A run that waits for a person (`GET /v1/approvals`). */
export declare const approvalSchema: z.ZodMiniObject<{
    run_id: z.ZodMiniString<string>;
    workflow: z.ZodMiniString<string>;
    paused_ms: z.ZodMiniNumber<number>;
    user_id: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
    session_id: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
    requests: z.ZodMiniArray<z.ZodMiniObject<{
        key: z.ZodMiniString<string>;
        node_id: z.ZodMiniString<string>;
        kind: z.ZodMiniString<string>;
        data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
        expires_ms: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniNumber<number>>>;
    }, z.core.$loose>>;
}, z.core.$loose>;
export type Approval = z.infer<typeof approvalSchema>;
export declare const approvalPageSchema: z.ZodMiniObject<{
    approvals: z.ZodMiniArray<z.ZodMiniObject<{
        run_id: z.ZodMiniString<string>;
        workflow: z.ZodMiniString<string>;
        paused_ms: z.ZodMiniNumber<number>;
        user_id: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
        session_id: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
        requests: z.ZodMiniArray<z.ZodMiniObject<{
            key: z.ZodMiniString<string>;
            node_id: z.ZodMiniString<string>;
            kind: z.ZodMiniString<string>;
            data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
            expires_ms: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniNumber<number>>>;
        }, z.core.$loose>>;
    }, z.core.$loose>>;
    next_cursor: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
    expiry_enforced: z.ZodMiniOptional<z.ZodMiniBoolean<boolean>>;
}, z.core.$loose>;
export type ApprovalPage = z.infer<typeof approvalPageSchema>;
export declare const deviceScopeSchema: z.ZodMiniEnum<{
    "runs:read": "runs:read";
    "approvals:resolve": "approvals:resolve";
}>;
export type DeviceScope = z.infer<typeof deviceScopeSchema>;
/** A paired device (`GET /v1/devices`). Its token is never listed. */
export declare const deviceSchema: z.ZodMiniObject<{
    id: z.ZodMiniString<string>;
    name: z.ZodMiniString<string>;
    scopes: z.ZodMiniArray<z.ZodMiniString<string>>;
    created_ms: z.ZodMiniNumber<number>;
    expires_ms: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniNumber<number>>>;
    last_used_ms: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniNumber<number>>>;
}, z.core.$loose>;
export type Device = z.infer<typeof deviceSchema>;
export declare const deviceListSchema: z.ZodMiniObject<{
    devices: z.ZodMiniArray<z.ZodMiniObject<{
        id: z.ZodMiniString<string>;
        name: z.ZodMiniString<string>;
        scopes: z.ZodMiniArray<z.ZodMiniString<string>>;
        created_ms: z.ZodMiniNumber<number>;
        expires_ms: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniNumber<number>>>;
        last_used_ms: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniNumber<number>>>;
    }, z.core.$loose>>;
}, z.core.$loose>;
/** A device that was just paired (`POST /v1/devices`): the only time its token is shown. */
export declare const pairedDeviceSchema: z.ZodMiniObject<{
    device: z.ZodMiniObject<{
        id: z.ZodMiniString<string>;
        name: z.ZodMiniString<string>;
        scopes: z.ZodMiniArray<z.ZodMiniString<string>>;
        created_ms: z.ZodMiniNumber<number>;
        expires_ms: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniNumber<number>>>;
        last_used_ms: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniNumber<number>>>;
    }, z.core.$loose>;
    token: z.ZodMiniString<string>;
    pairing: z.ZodMiniObject<{
        v: z.ZodMiniNumber<number>;
        url: z.ZodMiniString<string>;
        token: z.ZodMiniString<string>;
    }, z.core.$loose>;
    link: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
}, z.core.$loose>;
export type PairedDevice = z.infer<typeof pairedDeviceSchema>;
/** What a browser subscribes to push notifications with (`GET /v1/push`). */
export declare const pushInfoSchema: z.ZodMiniObject<{
    public_key: z.ZodMiniString<string>;
}, z.core.$loose>;
export type PushInfo = z.infer<typeof pushInfoSchema>;
export declare const errorBodySchema: z.ZodMiniObject<{
    error: z.ZodMiniString<string>;
}, z.core.$loose>;
/** The data of a `response` event: a piece of the ordered answer text. */
export declare const responseChunkSchema: z.ZodMiniObject<{
    node_id: z.ZodMiniString<string>;
    text: z.ZodMiniString<string>;
    is_final: z.ZodMiniBoolean<boolean>;
}, z.core.$loose>;
export type ResponseChunk = z.infer<typeof responseChunkSchema>;
export declare const runSucceededSchema: z.ZodMiniObject<{
    outputs: z.ZodMiniOptional<z.ZodMiniRecord<z.ZodMiniString<string>, z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>>;
    usage: z.ZodMiniOptional<z.ZodMiniObject<{
        prompt_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        completion_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        total_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
    }, z.core.$loose>>;
    elapsed_ms: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
}, z.core.$loose>;
export declare const runFailedSchema: z.ZodMiniObject<{
    error: z.ZodMiniOptional<z.ZodMiniString<string>>;
    error_type: z.ZodMiniOptional<z.ZodMiniString<string>>;
    usage: z.ZodMiniOptional<z.ZodMiniObject<{
        prompt_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        completion_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
        total_tokens: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
    }, z.core.$loose>>;
    elapsed_ms: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
}, z.core.$loose>;
export declare const runAbortedSchema: z.ZodMiniObject<{
    reason: z.ZodMiniOptional<z.ZodMiniString<string>>;
    elapsed_ms: z.ZodMiniOptional<z.ZodMiniNumber<number>>;
}, z.core.$loose>;
export declare const runPausedSchema: z.ZodMiniObject<{
    reasons: z.ZodMiniArray<z.ZodMiniObject<{
        frame: z.ZodMiniOptional<z.ZodMiniString<string>>;
        node_id: z.ZodMiniOptional<z.ZodMiniNullable<z.ZodMiniString<string>>>;
        kind: z.ZodMiniString<string>;
        data: z.ZodMiniOptional<z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
    }, z.core.$loose>>;
}, z.core.$loose>;
export declare const runStartedSchema: z.ZodMiniObject<{
    run_id: z.ZodMiniString<string>;
    resumed: z.ZodMiniOptional<z.ZodMiniBoolean<boolean>>;
}, z.core.$loose>;
export declare const laggedSchema: z.ZodMiniObject<{
    skipped: z.ZodMiniNumber<number>;
}, z.core.$loose>;
export declare const engineEventSchema: z.ZodMiniRecord<z.ZodMiniString<string>, z.ZodMiniType<Json, unknown, z.core.$ZodTypeInternals<Json, unknown>>>;
