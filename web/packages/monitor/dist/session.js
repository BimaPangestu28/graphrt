/**
 * Monitoring one `graphrt serve`: the state a monitoring view shows and the
 * calls that change it. No DOM here, so it also serves a custom view.
 *
 * The person signs in with the API key (the owner) or with a device token
 * (a paired phone, which reads runs and answers what waits for a person).
 */
import { GraphrtClient, GraphrtError, } from "@graphrt/client";
const DEVICE_TOKEN_PREFIX = "grd1.";
const PAGE_SIZE = 30;
/** Lines of a run kept in memory. A long run keeps its newest. */
const MAX_EVENT_LINES = 600;
/** Events that carry text piece by piece: the answer shows it, the log does not. */
const QUIET_EVENTS = new Set(["node_chunk", "response"]);
function describe(error) {
    if (error instanceof GraphrtError) {
        if (error.status === 401) {
            return "The key or device token was not accepted.";
        }
        if (error.status === 403) {
            return "This device is not allowed to do that.";
        }
        if (error.status === 0) {
            return "The connection was lost. Check your network and try again.";
        }
        return error.message;
    }
    return error instanceof Error && error.message !== "" ? error.message : "Something went wrong.";
}
function text(value) {
    return typeof value === "string" && value !== "" ? value : undefined;
}
/** One stored or live event as a line of the run's log; `undefined` for one the log leaves out. */
export function eventLine(event, index) {
    const type = text(event.type) ?? "event";
    if (QUIET_EVENTS.has(type)) {
        return undefined;
    }
    const node = text(event.node_id);
    let detail = text(event.kind);
    let tone = "plain";
    if (type.endsWith("_failed")) {
        tone = "bad";
        detail = text(event.error) ?? detail;
    }
    else if (type === "run_aborted") {
        tone = "bad";
        detail = text(event.reason);
    }
    else if (type === "run_paused" || type === "node_paused") {
        tone = "waiting";
    }
    else if (type === "run_succeeded") {
        tone = "good";
    }
    else if (type === "run_started" && event.resumed === true) {
        detail = "resumed";
    }
    return { index, type, node, detail, tone };
}
function lineOfRunEvent(event, index) {
    if (event.type === "response" || event.type === "lagged") {
        return undefined;
    }
    const type = event.type === "engine" ? event.name : event.type;
    return eventLine({ ...event.data, type }, index);
}
/** The place of an event in the run's log, from its SSE id (`<index>` or `<index>.<part>`). */
function indexOf(event, fallback) {
    const id = "id" in event ? event.id : undefined;
    const parsed = id === undefined ? Number.NaN : Number.parseInt(id, 10);
    return Number.isNaN(parsed) ? fallback : parsed;
}
/** Monitoring one server. Create it, {@link MonitorSession.start} it, then call what the person does. */
export class MonitorSession {
    #options;
    #listeners = new Set();
    #client;
    #token;
    #cursor;
    #following;
    #polling;
    #state = {
        status: "loading",
        runs: [],
        moreRuns: false,
        approvals: [],
        resolving: [],
        push: { available: false, needsInstall: false, permission: "default", subscribed: false, busy: false },
    };
    constructor(options = {}) {
        this.#options = options;
        this.#client = this.#newClient();
    }
    /** What a view shows now. A new object after every change. */
    get state() {
        return this.#state;
    }
    /**
     * Call `listener` after every change of the state.
     *
     * @returns A function that stops the calls.
     */
    subscribe(listener) {
        this.#listeners.add(listener);
        return () => this.#listeners.delete(listener);
    }
    /**
     * Ask the server what it offers and sign in with the remembered credential.
     * Never throws: what went wrong is in the state.
     *
     * @param pairingToken - A device token from a pairing link, which replaces what was remembered.
     */
    async start(pairingToken) {
        let config;
        try {
            config = await this.#client.monitorConfig();
        }
        catch (error) {
            const missing = error instanceof GraphrtError && error.status === 404;
            this.#set({
                status: "unavailable",
                notice: missing ? "This server has no monitoring page." : describe(error),
            });
            return;
        }
        this.#set({ config });
        if (pairingToken !== undefined) {
            await this.unlock(pairingToken, true);
            return;
        }
        const remembered = this.#stored();
        if (remembered !== undefined) {
            await this.unlock(remembered, true);
            return;
        }
        if (config.access === "open") {
            await this.#enter("owner");
            return;
        }
        this.#set({ status: "locked", notice: undefined });
    }
    /**
     * Sign in with the API key or a device token.
     *
     * @param remember - Keep the credential in the storage for the next visit.
     * @returns Whether the server accepted it.
     */
    async unlock(credential, remember = false) {
        const token = credential.trim();
        if (token === "") {
            return false;
        }
        this.#token = token;
        this.#client = this.#newClient();
        const isDevice = token.startsWith(DEVICE_TOKEN_PREFIX);
        try {
            if (isDevice) {
                this.#set({ device: await this.#client.devices.me() });
            }
            else {
                // The run list takes the API key (or a device token): a wrong key is refused here.
                await this.#client.listRuns({ limit: 1 });
            }
        }
        catch (error) {
            this.#token = undefined;
            this.#client = this.#newClient();
            this.#forget();
            this.#set({ status: "locked", notice: describe(error), device: undefined });
            return false;
        }
        if (remember) {
            this.#options.storage?.setItem(this.#storageKey(), token);
        }
        await this.#enter(isDevice ? "device" : "owner");
        return true;
    }
    /** Forget the credential on this browser. A device stays paired: remove it in the owner's list. */
    signOut() {
        this.stopPolling();
        this.#following?.abort();
        this.#forget();
        this.#token = undefined;
        this.#client = this.#newClient();
        this.#cursor = undefined;
        this.#set({
            status: this.#state.config?.access === "open" ? "ready" : "locked",
            notice: undefined,
            role: this.#state.config?.access === "open" ? "owner" : undefined,
            device: undefined,
            runs: [],
            approvals: [],
            detail: undefined,
            devices: undefined,
            paired: undefined,
        });
    }
    /** Whether the person may answer what waits for a person. */
    get canResolve() {
        const { role, device, config } = this.#state;
        return (config?.approvals === true &&
            (role === "owner" || device?.scopes.includes("approvals:resolve") === true));
    }
    /** Whether the person may read runs. */
    get canReadRuns() {
        const { role, device } = this.#state;
        return role === "owner" || device?.scopes.includes("runs:read") === true;
    }
    /** Load the newest runs and the inbox again. Never throws. */
    async refresh() {
        if (this.#state.status !== "ready") {
            return;
        }
        try {
            const [page, inbox] = await Promise.all([
                this.canReadRuns
                    ? this.#client.listRuns({ limit: PAGE_SIZE, ...this.#filterQuery() })
                    : Promise.resolve(undefined),
                this.canResolve ? this.#client.approvals.list({ limit: 100 }) : Promise.resolve(undefined),
            ]);
            const patch = { listError: undefined };
            if (page !== undefined) {
                // Older runs that were loaded stay below the newest page.
                const fresh = new Set(page.runs.map((run) => run.run_id));
                const older = this.#state.runs.slice(PAGE_SIZE).filter((run) => !fresh.has(run.run_id));
                patch.runs = [...page.runs, ...(this.#cursor === undefined ? [] : older)];
                if (this.#cursor === undefined || older.length === 0) {
                    this.#cursor = page.next_cursor ?? null;
                }
                patch.moreRuns = this.#cursor !== null && this.#cursor !== undefined;
            }
            if (inbox !== undefined) {
                patch.approvals = inbox.approvals;
            }
            this.#set(patch);
        }
        catch (error) {
            if (error instanceof GraphrtError && error.status === 401) {
                // Revoked or expired: back to the sign-in.
                this.signOut();
                this.#set({ notice: "This device was signed out. Pair it again or enter the key." });
                return;
            }
            this.#set({ listError: describe(error) });
        }
    }
    /** Refresh every `intervalMs` until {@link MonitorSession.stopPolling}. */
    startPolling(intervalMs = 5000) {
        this.stopPolling();
        this.#polling = setInterval(() => void this.refresh(), intervalMs);
        // Outside a browser, a timer must not keep the process alive.
        this.#polling.unref?.();
    }
    stopPolling() {
        if (this.#polling !== undefined) {
            clearInterval(this.#polling);
            this.#polling = undefined;
        }
    }
    /** Show only runs in `status` (every run with `undefined`). */
    async setFilter(status) {
        this.#cursor = undefined;
        this.#set({ filter: status, runs: [], moreRuns: false });
        await this.refresh();
    }
    /** Load the next page of older runs. */
    async loadMore() {
        if (typeof this.#cursor !== "string") {
            return;
        }
        try {
            const page = await this.#client.listRuns({
                limit: PAGE_SIZE,
                cursor: this.#cursor,
                ...this.#filterQuery(),
            });
            this.#cursor = page.next_cursor ?? null;
            const known = new Set(this.#state.runs.map((run) => run.run_id));
            this.#set({
                runs: [...this.#state.runs, ...page.runs.filter((run) => !known.has(run.run_id))],
                moreRuns: this.#cursor !== null,
                listError: undefined,
            });
        }
        catch (error) {
            this.#set({ listError: describe(error) });
        }
    }
    /**
     * Open a run: its record, what it did so far, and, while it executes, what
     * it does next. Never throws.
     */
    async openRun(runId) {
        this.#following?.abort();
        const following = new AbortController();
        this.#following = following;
        this.#set({ detail: { runId, events: [], liveAnswer: "", live: false } });
        try {
            const [record, stored] = await Promise.all([
                this.#client.getRun(runId, { signal: following.signal }),
                this.#client.listEvents(runId, { signal: following.signal }),
            ]);
            const events = [];
            stored.forEach((event, index) => {
                const line = eventLine(event, index);
                if (line !== undefined) {
                    events.push(line);
                }
            });
            this.#patchDetail(runId, { record, events: events.slice(-MAX_EVENT_LINES) });
            if (record.active === true) {
                // From the start of the execution in progress, so the answer it wrote
                // so far comes too. Its events up to here are shown already.
                let started = stored.length - 1;
                while (started >= 0 && stored[started]?.type !== "run_started") {
                    started -= 1;
                }
                await this.#follow(runId, started < 0 ? stored.length : started, stored.length, following.signal);
            }
        }
        catch (error) {
            if (!following.signal.aborted) {
                this.#patchDetail(runId, { error: describe(error), live: false });
            }
        }
    }
    /** Close the open run and stop following it. */
    closeRun() {
        this.#following?.abort();
        this.#following = undefined;
        this.#set({ detail: undefined });
    }
    /**
     * Stop a run that executes (the owner only).
     *
     * @returns Whether the server took the request.
     */
    async stopRun(runId) {
        try {
            await this.#client.abort(runId);
            return true;
        }
        catch (error) {
            this.#patchDetail(runId, { error: describe(error) });
            return false;
        }
    }
    /**
     * Answer one request of a paused run and let the run continue.
     *
     * @param key - The request's `key`.
     * @param input - `{action, values}` for a question, `{approved}` for a tool call.
     * @returns Whether the server took the answer.
     */
    async resolve(runId, key, input) {
        const id = `${runId}/${key}`;
        if (this.#state.resolving.includes(id)) {
            return false;
        }
        this.#set({ resolving: [...this.#state.resolving, id], resolveError: undefined });
        try {
            const accepted = await this.#client.approvals.resolve(runId, { [key]: input });
            // Answered: it leaves the inbox at once, the next refresh confirms it.
            this.#set({ approvals: this.#state.approvals.filter((approval) => approval.run_id !== runId) });
            if (this.#state.detail?.runId === runId) {
                void this.openRun(runId);
            }
            void accepted;
            return true;
        }
        catch (error) {
            const gone = error instanceof GraphrtError && error.status === 409;
            this.#set({
                resolveError: gone ? "Someone already answered this, or the run ended." : describe(error),
            });
            return false;
        }
        finally {
            this.#set({ resolving: this.#state.resolving.filter((other) => other !== id) });
            void this.refresh();
        }
    }
    /** Load the paired devices (the owner only). */
    async loadDevices() {
        if (this.#state.role !== "owner" || this.#state.config?.devices !== true) {
            return;
        }
        try {
            this.#set({ devices: await this.#client.devices.list(), deviceError: undefined });
        }
        catch (error) {
            this.#set({ deviceError: describe(error) });
        }
    }
    /**
     * Pair a device. Its token and pairing link are in `state.paired` until
     * {@link MonitorSession.dismissPaired}: they are never shown again.
     */
    async pairDevice(name) {
        try {
            // The address in the link is where this page reached the server.
            const url = this.#options.baseUrl;
            const paired = await this.#client.devices.pair({ name, ...(url === undefined ? {} : { url }) });
            this.#set({ paired, deviceError: undefined });
            await this.loadDevices();
            return true;
        }
        catch (error) {
            this.#set({ deviceError: describe(error) });
            return false;
        }
    }
    /** Forget the token of the device that was just paired. */
    dismissPaired() {
        this.#set({ paired: undefined });
    }
    /** Remove a device: its token stops working at once. */
    async revokeDevice(id) {
        try {
            await this.#client.devices.revoke(id);
            if (this.#state.paired?.device.id === id) {
                this.#set({ paired: undefined });
            }
            await this.loadDevices();
        }
        catch (error) {
            this.#set({ deviceError: describe(error) });
        }
    }
    /** Unpair this device (a device only) and sign out. */
    async unpair() {
        try {
            await this.disablePush();
            await this.#client.devices.unpair();
        }
        catch {
            // Already revoked: signing out is what is left to do.
        }
        this.signOut();
    }
    /** Ask for permission and notify this browser when a run fails or waits for a person. */
    async enablePush() {
        const browser = this.#options.push;
        if (browser === undefined || !this.#state.push.available) {
            return false;
        }
        this.#push({ busy: true, notice: undefined });
        try {
            const { public_key } = await this.#client.push.info();
            const subscription = await browser.subscribe(public_key);
            await this.#client.push.subscribe(subscription);
            this.#push({ subscribed: true, permission: browser.permission(), busy: false });
            return true;
        }
        catch (error) {
            const denied = browser.permission() === "denied";
            this.#push({
                busy: false,
                subscribed: false,
                permission: browser.permission(),
                notice: denied
                    ? "Notifications are blocked for this site. Allow them in the browser's settings."
                    : describe(error),
            });
            return false;
        }
    }
    /** Stop notifying this browser. */
    async disablePush() {
        const browser = this.#options.push;
        if (browser === undefined || !browser.supported()) {
            return;
        }
        this.#push({ busy: true, notice: undefined });
        try {
            const subscription = await browser.subscription();
            if (subscription !== undefined) {
                await this.#client.push.unsubscribe(subscription.endpoint).catch(() => undefined);
                await browser.unsubscribe();
            }
            this.#push({ subscribed: false, busy: false });
        }
        catch (error) {
            this.#push({ busy: false, notice: describe(error) });
        }
    }
    /** Send a test notification to this browser. */
    async testPush() {
        const browser = this.#options.push;
        if (browser === undefined) {
            return false;
        }
        this.#push({ busy: true, notice: undefined });
        try {
            const subscription = await browser.subscription();
            if (subscription === undefined) {
                this.#push({ busy: false, subscribed: false, notice: "Turn notifications on first." });
                return false;
            }
            await this.#client.push.test(subscription.endpoint);
            this.#push({ busy: false, notice: "A test notification is on its way." });
            return true;
        }
        catch (error) {
            this.#push({ busy: false, notice: describe(error) });
            return false;
        }
    }
    // ---- internals ---------------------------------------------------------------
    #newClient() {
        return new GraphrtClient({
            ...(this.#options.baseUrl === undefined ? {} : { baseUrl: this.#options.baseUrl }),
            ...(this.#options.fetch === undefined ? {} : { fetch: this.#options.fetch }),
            ...(this.#token === undefined ? {} : { token: this.#token }),
        });
    }
    #storageKey() {
        return `graphrt-monitor:${this.#options.baseUrl ?? ""}:credential`;
    }
    #stored() {
        try {
            return this.#options.storage?.getItem(this.#storageKey()) ?? undefined;
        }
        catch {
            return undefined;
        }
    }
    #forget() {
        try {
            this.#options.storage?.removeItem(this.#storageKey());
        }
        catch {
            // A storage that refuses is as good as an empty one.
        }
    }
    #filterQuery() {
        return this.#state.filter === undefined ? {} : { status: this.#state.filter };
    }
    async #enter(role) {
        this.#set({ status: "ready", role, notice: undefined });
        await Promise.all([this.refresh(), this.#loadPush(), this.loadDevices()]);
    }
    async #loadPush() {
        const browser = this.#options.push;
        const offered = this.#state.config?.push === true;
        if (browser === undefined || !offered) {
            this.#push({ available: false, needsInstall: false });
            return;
        }
        if (!browser.supported()) {
            this.#push({ available: false, needsInstall: browser.needsInstall() });
            return;
        }
        let subscribed = false;
        try {
            subscribed = (await browser.subscription()) !== undefined;
        }
        catch {
            // No service worker yet: not subscribed.
        }
        this.#push({ available: true, needsInstall: false, permission: browser.permission(), subscribed });
    }
    /**
     * Follow the execution in progress from event `from` until it ends. Events
     * before `shown` are in the log already: only their answer text is taken.
     */
    async #follow(runId, from, shown, signal) {
        this.#patchDetail(runId, { live: true });
        let next = from;
        try {
            for await (const event of this.#client.events(runId, { from, signal })) {
                const detail = this.#state.detail;
                if (detail?.runId !== runId) {
                    return;
                }
                if (event.type === "response") {
                    this.#patchDetail(runId, { liveAnswer: detail.liveAnswer + event.data.text });
                    continue;
                }
                const index = indexOf(event, next);
                next = index + 1;
                const line = index < shown ? undefined : lineOfRunEvent(event, index);
                if (line !== undefined) {
                    this.#patchDetail(runId, { events: [...detail.events, line].slice(-MAX_EVENT_LINES) });
                }
            }
            // Ended: the record has the outcome.
            const record = await this.#client.getRun(runId, { signal });
            this.#patchDetail(runId, { record, live: false, liveAnswer: "" });
            void this.refresh();
        }
        catch (error) {
            if (!signal.aborted) {
                this.#patchDetail(runId, { live: false, error: describe(error) });
            }
        }
    }
    #patchDetail(runId, patch) {
        const detail = this.#state.detail;
        if (detail?.runId === runId) {
            this.#set({ detail: { ...detail, ...patch } });
        }
    }
    #push(patch) {
        this.#set({ push: { ...this.#state.push, ...patch } });
    }
    #set(patch) {
        this.#state = { ...this.#state, ...patch };
        for (const listener of this.#listeners) {
            listener(this.#state);
        }
    }
}
