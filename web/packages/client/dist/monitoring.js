import { acceptedRunSchema, approvalPageSchema, deviceListSchema, deviceSchema, pairedDeviceSchema, pushInfoSchema, } from "./schemas.js";
/**
 * The approvals inbox of a server (`serve --approvals`): every run that waits
 * for a person. Takes the API key or a device token with `approvals:resolve`.
 *
 * @example
 * const { approvals } = await client.approvals.list();
 * const [first] = approvals;
 * await client.approvals.resolve(first.run_id, { [first.requests[0].key]: { action: "approve" } });
 */
export class Approvals {
    #client;
    /** @internal */
    constructor(client) {
        this.#client = client;
    }
    /**
     * The runs that wait for a person, the most recently paused first.
     *
     * @throws GraphrtError - `404` when the server has no inbox, `403` for a device without the scope.
     */
    list(query = {}) {
        const { signal, ...params } = query;
        return this.#client.request("GET", "/v1/approvals", approvalPageSchema, { query: params, signal });
    }
    /**
     * Answer what a run waits for and let it continue in the background.
     * Follow it from the answer's `first_event`.
     *
     * @param inputs - The answers, keyed like the entry's `requests`: `{action, values}` for a
     *   `human_input` request, `{approved}` or `{approvals}` for a `tool_approval`.
     * @throws GraphrtError - `409` when the run waits no more, `400` for an answer nothing waits for.
     */
    resolve(runId, inputs, options = {}) {
        return this.#client.request("POST", `/v1/approvals/${encodeURIComponent(runId)}`, acceptedRunSchema, {
            body: { inputs },
            signal: options.signal,
        });
    }
}
/**
 * Monitoring devices of a server (`serve --devices`). Pairing, listing and
 * revoking take the API key. `me` and `unpair` take the device's own token.
 */
export class Devices {
    #client;
    /** @internal */
    constructor(client) {
        this.#client = client;
    }
    /**
     * Pair a device. The answer holds its token, which is never shown again.
     *
     * @throws GraphrtError - `401` without the API key, `409` at the server's limit.
     */
    pair(request, options = {}) {
        return this.#client.request("POST", "/v1/devices", pairedDeviceSchema, {
            body: request,
            signal: options.signal,
        });
    }
    /** Every paired device, oldest first. */
    async list(options = {}) {
        const listed = await this.#client.request("GET", "/v1/devices", deviceListSchema, {
            signal: options.signal,
        });
        return listed.devices;
    }
    /** End a device: its token stops working at once. */
    async revoke(id, options = {}) {
        await this.#client.request("DELETE", `/v1/devices/${encodeURIComponent(id)}`, undefined, {
            signal: options.signal,
        });
    }
    /**
     * The device of the token this client sends.
     *
     * @throws GraphrtError - `401` when the token was revoked or expired.
     */
    me(options = {}) {
        return this.#client.request("GET", "/v1/devices/me", deviceSchema, { signal: options.signal });
    }
    /** End the device of the token this client sends. */
    async unpair(options = {}) {
        await this.#client.request("DELETE", "/v1/devices/me", undefined, { signal: options.signal });
    }
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
export class Push {
    #client;
    /** @internal */
    constructor(client) {
        this.#client = client;
    }
    /**
     * The key a browser subscribes with (`applicationServerKey`).
     *
     * @throws GraphrtError - `404` when the server sends no push notifications.
     */
    info(options = {}) {
        return this.#client.request("GET", "/v1/push", pushInfoSchema, { signal: options.signal });
    }
    /**
     * Notify this browser from now on.
     *
     * @throws GraphrtError - `400` for a subscription the server cannot use.
     */
    async subscribe(subscription, options = {}) {
        const { endpoint, keys } = subscription;
        await this.#client.request("POST", "/v1/push/subscriptions", undefined, {
            body: { endpoint, keys: { p256dh: keys.p256dh, auth: keys.auth } },
            signal: options.signal,
        });
    }
    /** Stop notifying the browser of `endpoint`. */
    async unsubscribe(endpoint, options = {}) {
        await this.#client.request("DELETE", "/v1/push/subscriptions", undefined, {
            body: { endpoint },
            signal: options.signal,
        });
    }
    /**
     * Send one test notification to the browser of `endpoint`.
     *
     * @throws GraphrtError - `502` when the push service did not take the message.
     */
    async test(endpoint, options = {}) {
        await this.#client.request("POST", "/v1/push/test", undefined, {
            body: { endpoint },
            signal: options.signal,
        });
    }
}
