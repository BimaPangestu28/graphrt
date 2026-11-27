/**
 * What a monitoring device uses: the approvals inbox, paired devices and push
 * notifications. Reached as `client.approvals`, `client.devices` and `client.push`.
 */
import type { CallOptions, GraphrtClient } from "./client.ts";
import { type AcceptedRun, type ApprovalPage, type Device, type DeviceScope, type Json, type PairedDevice, type PushInfo } from "./schemas.ts";
/**
 * The approvals inbox of a server (`serve --approvals`): every run that waits
 * for a person. Takes the API key or a device token with `approvals:resolve`.
 *
 * @example
 * const { approvals } = await client.approvals.list();
 * const [first] = approvals;
 * await client.approvals.resolve(first.run_id, { [first.requests[0].key]: { action: "approve" } });
 */
export declare class Approvals {
    #private;
    /** @internal */
    constructor(client: GraphrtClient);
    /**
     * The runs that wait for a person, the most recently paused first.
     *
     * @throws GraphrtError - `404` when the server has no inbox, `403` for a device without the scope.
     */
    list(query?: {
        workflow?: string;
        limit?: number;
        cursor?: string;
    } & CallOptions): Promise<ApprovalPage>;
    /**
     * Answer what a run waits for and let it continue in the background.
     * Follow it from the answer's `first_event`.
     *
     * @param inputs - The answers, keyed like the entry's `requests`: `{action, values}` for a
     *   `human_input` request, `{approved}` or `{approvals}` for a `tool_approval`.
     * @throws GraphrtError - `409` when the run waits no more, `400` for an answer nothing waits for.
     */
    resolve(runId: string, inputs: Record<string, Json>, options?: CallOptions): Promise<AcceptedRun>;
}
export interface PairDeviceRequest {
    /** What the owner calls the device, e.g. "Ana's phone". */
    name: string;
    /** What its token may do. Default: every scope. */
    scopes?: DeviceScope[];
    /** Days until the token stops working. Default: no expiry. */
    expires_in_days?: number;
    /** The address the device reaches the server at. Default: what the server knows. */
    url?: string;
}
/**
 * Monitoring devices of a server (`serve --devices`). Pairing, listing and
 * revoking take the API key. `me` and `unpair` take the device's own token.
 */
export declare class Devices {
    #private;
    /** @internal */
    constructor(client: GraphrtClient);
    /**
     * Pair a device. The answer holds its token, which is never shown again.
     *
     * @throws GraphrtError - `401` without the API key, `409` at the server's limit.
     */
    pair(request: PairDeviceRequest, options?: CallOptions): Promise<PairedDevice>;
    /** Every paired device, oldest first. */
    list(options?: CallOptions): Promise<Device[]>;
    /** End a device: its token stops working at once. */
    revoke(id: string, options?: CallOptions): Promise<void>;
    /**
     * The device of the token this client sends.
     *
     * @throws GraphrtError - `401` when the token was revoked or expired.
     */
    me(options?: CallOptions): Promise<Device>;
    /** End the device of the token this client sends. */
    unpair(options?: CallOptions): Promise<void>;
}
/** A push subscription as a browser gives it (`PushSubscription.toJSON()`). */
export interface PushSubscriptionKeys {
    endpoint: string;
    keys: {
        p256dh: string;
        auth: string;
    };
}
/**
 * Push notifications of a server (`serve --push`): a subscribed browser hears
 * when a run fails or waits for a person. Takes the API key or a device token.
 *
 * @example
 * const { public_key } = await client.push.info();
 * const subscription = await registration.pushManager.subscribe({
 *   userVisibleOnly: true,
 *   applicationServerKey: public_key,
 * });
 * await client.push.subscribe(subscription.toJSON());
 */
export declare class Push {
    #private;
    /** @internal */
    constructor(client: GraphrtClient);
    /**
     * The key a browser subscribes with (`applicationServerKey`).
     *
     * @throws GraphrtError - `404` when the server sends no push notifications.
     */
    info(options?: CallOptions): Promise<PushInfo>;
    /**
     * Notify this browser from now on.
     *
     * @throws GraphrtError - `400` for a subscription the server cannot use.
     */
    subscribe(subscription: PushSubscriptionKeys, options?: CallOptions): Promise<void>;
    /** Stop notifying the browser of `endpoint`. */
    unsubscribe(endpoint: string, options?: CallOptions): Promise<void>;
    /**
     * Send one test notification to the browser of `endpoint`.
     *
     * @throws GraphrtError - `502` when the push service did not take the message.
     */
    test(endpoint: string, options?: CallOptions): Promise<void>;
}
