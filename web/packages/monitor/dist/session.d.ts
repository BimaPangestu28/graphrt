/**
 * Monitoring one `graphrt serve`: the state a monitoring view shows and the
 * calls that change it. No DOM here, so it also serves a custom view.
 *
 * The person signs in with the API key (the owner) or with a device token
 * (a paired phone, which reads runs and answers what waits for a person).
 */
import { type Approval, type Device, type Json, type MonitorConfig, type PairedDevice, type PushSubscriptionKeys, type RunRecord, type RunStatus } from "@graphrt/client";
/** Where a session keeps the credential across reloads. `localStorage` fits. */
export interface KeyValueStore {
    getItem(key: string): string | null;
    setItem(key: string, value: string): void;
    removeItem(key: string): void;
}
/** What the browser offers for push notifications. The page brings the real one. */
export interface PushBrowser {
    /** Whether this browser can receive push notifications here and now. */
    supported(): boolean;
    /** Whether it could once the page is installed (an iPhone or iPad in a browser tab). */
    needsInstall(): boolean;
    permission(): "default" | "granted" | "denied";
    /** The subscription this browser has, if any. */
    subscription(): Promise<PushSubscriptionKeys | undefined>;
    /** Ask the person for permission and subscribe with the server's key. */
    subscribe(publicKey: string): Promise<PushSubscriptionKeys>;
    unsubscribe(): Promise<void>;
}
/** One line of what a run did. */
export interface EventLine {
    /** The event's place in the run's log. */
    index: number;
    /** The event type, e.g. `node_started`. */
    type: string;
    /** The node it is about, when it is about one. */
    node?: string | undefined;
    /** What to show next to it: the node kind, an error, a reason. */
    detail?: string | undefined;
    tone: "plain" | "good" | "bad" | "waiting";
}
export interface RunDetail {
    runId: string;
    record?: RunRecord | undefined;
    events: EventLine[];
    /** Answer text of the execution in progress, as it is written. */
    liveAnswer: string;
    /** Whether the run is being followed as it executes. */
    live: boolean;
    /** Why the run could not be loaded. */
    error?: string | undefined;
}
export interface PushState {
    /** Whether the server sends push notifications and this browser can receive them. */
    available: boolean;
    /** An iPhone or iPad that must add the page to the Home Screen first. */
    needsInstall: boolean;
    permission: "default" | "granted" | "denied";
    /** Whether this browser is subscribed at this server. */
    subscribed: boolean;
    busy: boolean;
    /** What went wrong, or what happened, in words for the person. */
    notice?: string | undefined;
}
export interface MonitorState {
    /** `loading` until the server said what it offers; `locked` until a credential is entered. */
    status: "loading" | "locked" | "ready" | "unavailable";
    /** Why the page cannot be used, or why a credential was refused. */
    notice?: string | undefined;
    config?: MonitorConfig | undefined;
    /** `owner` holds the API key, `device` a device token. */
    role?: "owner" | "device" | undefined;
    /** The device of the token in use. */
    device?: Device | undefined;
    runs: RunRecord[];
    /** Whether older runs can be loaded. */
    moreRuns: boolean;
    filter?: RunStatus | undefined;
    /** Why the runs or the inbox could not be refreshed. */
    listError?: string | undefined;
    approvals: Approval[];
    /** The run that is open. */
    detail?: RunDetail | undefined;
    /** Requests being answered right now, as `<run id>/<key>`. */
    resolving: string[];
    /** Why the last answer was not taken. */
    resolveError?: string | undefined;
    /** The paired devices (owner only). */
    devices?: Device[] | undefined;
    /** The device that was just paired: the only time its token is known. */
    paired?: PairedDevice | undefined;
    /** Why a device could not be paired or removed. */
    deviceError?: string | undefined;
    push: PushState;
}
export interface MonitorSessionOptions {
    /** Where the server is. Default: the page's own origin. */
    baseUrl?: string;
    /** Keeps the credential across reloads. Default: none. */
    storage?: KeyValueStore;
    /** Push notifications of this browser. Default: none. */
    push?: PushBrowser;
    fetch?: typeof fetch;
}
type Listener = (state: MonitorState) => void;
/** One stored or live event as a line of the run's log; `undefined` for one the log leaves out. */
export declare function eventLine(event: Record<string, Json>, index: number): EventLine | undefined;
/** Monitoring one server. Create it, {@link MonitorSession.start} it, then call what the person does. */
export declare class MonitorSession {
    #private;
    constructor(options?: MonitorSessionOptions);
    /** What a view shows now. A new object after every change. */
    get state(): MonitorState;
    /**
     * Call `listener` after every change of the state.
     *
     * @returns A function that stops the calls.
     */
    subscribe(listener: Listener): () => void;
    /**
     * Ask the server what it offers and sign in with the remembered credential.
     * Never throws: what went wrong is in the state.
     *
     * @param pairingToken - A device token from a pairing link, which replaces what was remembered.
     */
    start(pairingToken?: string): Promise<void>;
    /**
     * Sign in with the API key or a device token.
     *
     * @param remember - Keep the credential in the storage for the next visit.
     * @returns Whether the server accepted it.
     */
    unlock(credential: string, remember?: boolean): Promise<boolean>;
    /** Forget the credential on this browser. A device stays paired: remove it in the owner's list. */
    signOut(): void;
    /** Whether the person may answer what waits for a person. */
    get canResolve(): boolean;
    /** Whether the person may read runs. */
    get canReadRuns(): boolean;
    /** Load the newest runs and the inbox again. Never throws. */
    refresh(): Promise<void>;
    /** Refresh every `intervalMs` until {@link MonitorSession.stopPolling}. */
    startPolling(intervalMs?: number): void;
    stopPolling(): void;
    /** Show only runs in `status` (every run with `undefined`). */
    setFilter(status: RunStatus | undefined): Promise<void>;
    /** Load the next page of older runs. */
    loadMore(): Promise<void>;
    /**
     * Open a run: its record, what it did so far, and, while it executes, what
     * it does next. Never throws.
     */
    openRun(runId: string): Promise<void>;
    /** Close the open run and stop following it. */
    closeRun(): void;
    /**
     * Stop a run that executes (the owner only).
     *
     * @returns Whether the server took the request.
     */
    stopRun(runId: string): Promise<boolean>;
    /**
     * Answer one request of a paused run and let the run continue.
     *
     * @param key - The request's `key`.
     * @param input - `{action, values}` for a question, `{approved}` for a tool call.
     * @returns Whether the server took the answer.
     */
    resolve(runId: string, key: string, input: Json): Promise<boolean>;
    /** Load the paired devices (the owner only). */
    loadDevices(): Promise<void>;
    /**
     * Pair a device. Its token and pairing link are in `state.paired` until
     * {@link MonitorSession.dismissPaired}: they are never shown again.
     */
    pairDevice(name: string): Promise<boolean>;
    /** Forget the token of the device that was just paired. */
    dismissPaired(): void;
    /** Remove a device: its token stops working at once. */
    revokeDevice(id: string): Promise<void>;
    /** Unpair this device (a device only) and sign out. */
    unpair(): Promise<void>;
    /** Ask for permission and notify this browser when a run fails or waits for a person. */
    enablePush(): Promise<boolean>;
    /** Stop notifying this browser. */
    disablePush(): Promise<void>;
    /** Send a test notification to this browser. */
    testPush(): Promise<boolean>;
}
export {};
