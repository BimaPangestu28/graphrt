/**
 * What a monitoring device uses: the approvals inbox, paired devices and push
 * notifications. Reached as `client.approvals`, `client.devices` and `client.push`.
 */
import type { CallOptions, GraphrtClient } from "./client.ts";
import {
  type AcceptedRun,
  type ApprovalPage,
  acceptedRunSchema,
  approvalPageSchema,
  type Device,
  type DeviceScope,
  deviceListSchema,
  deviceSchema,
  type Json,
  type PairedDevice,
  type PushInfo,
  pairedDeviceSchema,
  pushInfoSchema,
} from "./schemas.ts";

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
  readonly #client: GraphrtClient;

  /** @internal */
  constructor(client: GraphrtClient) {
    this.#client = client;
  }

  /**
   * The runs that wait for a person, the most recently paused first.
   *
   * @throws GraphrtError - `404` when the server has no inbox, `403` for a device without the scope.
   */
  list(
    query: { workflow?: string; limit?: number; cursor?: string } & CallOptions = {},
  ): Promise<ApprovalPage> {
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
  resolve(runId: string, inputs: Record<string, Json>, options: CallOptions = {}): Promise<AcceptedRun> {
    return this.#client.request("POST", `/v1/approvals/${encodeURIComponent(runId)}`, acceptedRunSchema, {
      body: { inputs },
      signal: options.signal,
    });
  }
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
export class Devices {
  readonly #client: GraphrtClient;

  /** @internal */
  constructor(client: GraphrtClient) {
    this.#client = client;
  }

  /**
   * Pair a device. The answer holds its token, which is never shown again.
   *
   * @throws GraphrtError - `401` without the API key, `409` at the server's limit.
   */
  pair(request: PairDeviceRequest, options: CallOptions = {}): Promise<PairedDevice> {
    return this.#client.request("POST", "/v1/devices", pairedDeviceSchema, {
      body: request,
      signal: options.signal,
    });
  }

  /** Every paired device, oldest first. */
  async list(options: CallOptions = {}): Promise<Device[]> {
    const listed = await this.#client.request("GET", "/v1/devices", deviceListSchema, {
      signal: options.signal,
    });
    return listed.devices;
  }

  /** End a device: its token stops working at once. */
  async revoke(id: string, options: CallOptions = {}): Promise<void> {
    await this.#client.request("DELETE", `/v1/devices/${encodeURIComponent(id)}`, undefined, {
      signal: options.signal,
    });
  }

  /**
   * The device of the token this client sends.
   *
   * @throws GraphrtError - `401` when the token was revoked or expired.
   */
  me(options: CallOptions = {}): Promise<Device> {
    return this.#client.request("GET", "/v1/devices/me", deviceSchema, { signal: options.signal });
  }

  /** End the device of the token this client sends. */
  async unpair(options: CallOptions = {}): Promise<void> {
    await this.#client.request("DELETE", "/v1/devices/me", undefined, { signal: options.signal });
  }
}

/** A push subscription as a browser gives it (`PushSubscription.toJSON()`). */
export interface PushSubscriptionKeys {
  endpoint: string;
  keys: { p256dh: string; auth: string };
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
  readonly #client: GraphrtClient;

  /** @internal */
  constructor(client: GraphrtClient) {
    this.#client = client;
  }

  /**
   * The key a browser subscribes with (`applicationServerKey`).
   *
   * @throws GraphrtError - `404` when the server sends no push notifications.
   */
  info(options: CallOptions = {}): Promise<PushInfo> {
    return this.#client.request("GET", "/v1/push", pushInfoSchema, { signal: options.signal });
  }

  /**
   * Notify this browser from now on.
   *
   * @throws GraphrtError - `400` for a subscription the server cannot use.
   */
  async subscribe(subscription: PushSubscriptionKeys, options: CallOptions = {}): Promise<void> {
    const { endpoint, keys } = subscription;
    await this.#client.request("POST", "/v1/push/subscriptions", undefined, {
      body: { endpoint, keys: { p256dh: keys.p256dh, auth: keys.auth } },
      signal: options.signal,
    });
  }

  /** Stop notifying the browser of `endpoint`. */
  async unsubscribe(endpoint: string, options: CallOptions = {}): Promise<void> {
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
  async test(endpoint: string, options: CallOptions = {}): Promise<void> {
    await this.#client.request("POST", "/v1/push/test", undefined, {
      body: { endpoint },
      signal: options.signal,
    });
  }
}
