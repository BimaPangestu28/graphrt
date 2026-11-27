/**
 * A chat with one workflow of a `graphrt serve`: the state a chat view shows
 * and the calls that change it. No DOM here, so it also serves a custom view.
 *
 * A message is sent as a background run that is followed by its events, so a
 * phone that loses its connection for a moment picks the answer up where it
 * was. The history lives on the server when it keeps conversations, and is
 * sent with every message (`sys.history`) when it does not.
 */
import { GraphrtClient, GraphrtError, } from "@graphrt/client";
import { collectUiBlocks } from "@graphrt/ui";
const FAILED = "Something went wrong while answering. Please try again.";
function describe(error) {
    if (error instanceof GraphrtError) {
        if (error.status === 429) {
            const wait = error.retryAfterSeconds;
            return wait
                ? `Too many messages. Try again in ${wait} seconds.`
                : "Too many messages. Try again in a moment.";
        }
        if (error.status === 401 || error.status === 403) {
            return "You are not allowed to do this.";
        }
        if (error.status === 0) {
            return "The connection was lost. Check your network and try again.";
        }
        return error.message;
    }
    return FAILED;
}
function isRecord(value) {
    return typeof value === "object" && value !== null && !Array.isArray(value);
}
/**
 * The card for a `human_input` pause: the node's own `card` when it has one,
 * else an Adaptive Card with its prompt, form fields and actions. The actions'
 * ids are the node's action ids, so the card's answer is the resume input.
 */
export function questionBlock(reason) {
    if (reason.kind !== "human_input" || !isRecord(reason.data)) {
        return undefined;
    }
    const data = reason.data;
    if (isRecord(data.card)) {
        return data.card;
    }
    const body = [];
    if (typeof data.prompt === "string" && data.prompt !== "") {
        body.push({ type: "TextBlock", text: data.prompt.replaceAll("${", "$ {"), wrap: true });
    }
    for (const field of Array.isArray(data.fields) ? data.fields : []) {
        if (!isRecord(field) || typeof field.name !== "string") {
            continue;
        }
        const common = {
            id: field.name,
            label: typeof field.label === "string" ? field.label : field.name,
            isRequired: field.required === true,
            ...(typeof field.default === "string" || typeof field.default === "number"
                ? { value: field.default }
                : {}),
        };
        const options = Array.isArray(field.options) ? field.options : [];
        if (field.type === "number") {
            body.push({ type: "Input.Number", ...common });
        }
        else if (field.type === "boolean" || field.type === "checkbox") {
            body.push({ type: "Input.Toggle", id: field.name, title: common.label });
        }
        else if (options.length > 0) {
            const choices = options.map((option) => {
                const value = isRecord(option) ? String(option.value ?? "") : String(option);
                const title = isRecord(option) && typeof option.label === "string" ? option.label : value;
                return { title, value };
            });
            body.push({ type: "Input.ChoiceSet", ...common, choices });
        }
        else {
            body.push({
                type: "Input.Text",
                ...common,
                isMultiline: field.type === "paragraph" || field.type === "textarea",
            });
        }
    }
    const actions = [];
    for (const action of Array.isArray(data.actions) ? data.actions : []) {
        if (isRecord(action) && typeof action.id === "string") {
            const title = typeof action.title === "string" ? action.title : action.id;
            actions.push({ type: "Action.Submit", id: action.id, title });
        }
    }
    if (actions.length === 0) {
        actions.push({ type: "Action.Submit", id: "submit", title: "Send" });
    }
    return { $ui: "adaptive_card", version: 1, card: { type: "AdaptiveCard", version: "1.5", body, actions } };
}
/** One chat. Create it, {@link ChatSession.start} it, then {@link ChatSession.send} messages. */
export class ChatSession {
    #options;
    #listeners = new Set();
    #client;
    #state = { status: "loading", title: "", messages: [], busy: false };
    #workflow = "";
    #input = null;
    #conversationId;
    #token;
    #nextId = 0;
    #activeRun;
    constructor(options = {}) {
        this.#options = options;
        this.#token = options.apiKey;
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
     * Ask the server what it offers, sign in as it requires and load the
     * conversation of an earlier visit. Never throws: what went wrong is in the state.
     */
    async start() {
        let config;
        try {
            config = await this.#client.chatConfig();
        }
        catch (error) {
            const missing = error instanceof GraphrtError && error.status === 404;
            this.#set({ status: "unavailable", notice: missing ? "This server has no chat." : describe(error) });
            return;
        }
        const offered = config.workflows.find((workflow) => workflow.name === this.#options.workflow) ?? config.workflows[0];
        if (offered === undefined ||
            (this.#options.workflow !== undefined && offered.name !== this.#options.workflow)) {
            this.#set({ status: "unavailable", notice: "This chat is not available.", config });
            return;
        }
        this.#workflow = offered.name;
        this.#input = offered.input;
        this.#set({ config, title: config.title });
        try {
            if (config.access === "guests") {
                await this.#openGuestSession();
            }
            else if (config.access === "key" && this.#token === undefined) {
                const remembered = this.#stored("key");
                if (remembered === undefined) {
                    this.#set({ status: "locked" });
                    return;
                }
                this.#token = remembered;
            }
            else if (config.access === "users" && this.#options.userToken === undefined) {
                this.#set({ status: "unavailable", notice: "Sign in to use this chat." });
                return;
            }
            this.#client = this.#newClient();
            await this.#loadConversation();
            this.#set({ status: "ready", notice: undefined });
        }
        catch (error) {
            this.#set({ status: "unavailable", notice: describe(error) });
        }
    }
    /**
     * Use `key` as the API key (a server with `--chat-access key`).
     *
     * @param remember - Keep the key in the storage for the next visit.
     * @returns Whether the server accepted it.
     */
    async unlock(key, remember = false) {
        const candidate = new GraphrtClient({ ...this.#clientOptions(), token: key });
        try {
            // Any route that takes the key tells whether it is the right one.
            await candidate.listRuns({ limit: 1 });
        }
        catch (error) {
            const refused = error instanceof GraphrtError && error.status === 401;
            this.#set({ status: "locked", notice: refused ? "This key was not accepted." : describe(error) });
            return false;
        }
        this.#token = key;
        this.#client = candidate;
        if (remember) {
            this.#store("key", key);
        }
        await this.#loadConversation().catch(() => undefined);
        this.#set({ status: "ready", notice: undefined });
        return true;
    }
    /** Forget the key and lock the chat again. */
    lock() {
        this.#forget("key");
        this.#token = undefined;
        this.#client = this.#newClient();
        this.#set({ status: "locked", messages: [], notice: undefined });
    }
    /**
     * Send a message and follow its answer until the run ends. Never throws: a
     * failure is shown on the answer.
     */
    async send(text) {
        const content = text.trim();
        if (content === "" || this.#state.busy || this.#state.status !== "ready") {
            return;
        }
        const history = this.#history();
        const question = this.#message("user", content, "done");
        const answer = this.#message("assistant", "", "streaming");
        this.#set({ messages: [...this.#state.messages, question, answer], busy: true });
        try {
            const request = await this.#request(content, history);
            const accepted = await this.#client.startAsync(this.#workflow, request);
            this.#activeRun = accepted.run_id;
            this.#update(question.id, { runId: accepted.run_id });
            this.#update(answer.id, { runId: accepted.run_id });
            await this.#follow(answer.id, accepted.run_id, 0);
        }
        catch (error) {
            this.#update(answer.id, { state: "failed", error: describe(error) });
        }
        finally {
            this.#activeRun = undefined;
            this.#set({ busy: false });
        }
    }
    /** Stop the answer that is being produced. */
    async stop() {
        const run = this.#activeRun;
        if (run !== undefined) {
            await this.#client.abort(run).catch(() => false);
        }
    }
    /**
     * Answer a question of a paused run and follow the rest of its answer.
     *
     * @param action - The id of the action the person chose.
     * @param values - The values of the question's inputs.
     */
    async answer(messageId, key, action, values) {
        const message = this.#state.messages.find((candidate) => candidate.id === messageId);
        if (message?.runId === undefined || this.#state.busy) {
            return;
        }
        const runId = message.runId;
        this.#update(messageId, { state: "streaming", questions: [] });
        this.#set({ busy: true });
        try {
            const input = action === undefined ? { values } : { action, values };
            const accepted = await this.#client.resumeAsync(runId, { inputs: { [key]: input } });
            this.#activeRun = runId;
            await this.#follow(messageId, runId, accepted.first_event ?? 0);
        }
        catch (error) {
            this.#update(messageId, { state: "failed", error: describe(error) });
        }
        finally {
            this.#activeRun = undefined;
            this.#set({ busy: false });
        }
    }
    /** Rate an answer. A second call changes the rating. */
    async rate(messageId, rating) {
        const message = this.#state.messages.find((candidate) => candidate.id === messageId);
        if (message?.runId === undefined) {
            return;
        }
        this.#update(messageId, { rating });
        try {
            await this.#client.feedback(message.runId, { name: "helpful", value: rating === "up" });
        }
        catch {
            this.#update(messageId, { rating: undefined });
        }
    }
    /** Start over: a new conversation, the old one stays on the server until it expires. */
    reset() {
        this.#conversationId = undefined;
        this.#forget(this.#conversationKey());
        this.#set({ messages: [], notice: undefined });
    }
    // ---- internals ---------------------------------------------------------------
    #clientOptions() {
        return {
            ...(this.#options.baseUrl === undefined ? {} : { baseUrl: this.#options.baseUrl }),
            ...(this.#options.fetch === undefined ? {} : { fetch: this.#options.fetch }),
        };
    }
    #newClient() {
        const header = this.#state.config?.user_token_header;
        return new GraphrtClient({
            ...this.#clientOptions(),
            ...(this.#token === undefined ? {} : { token: this.#token }),
            ...(this.#options.userToken === undefined ? {} : { userToken: this.#options.userToken }),
            ...(header ? { userTokenHeader: header } : {}),
        });
    }
    /** A guest session: the one of an earlier visit renewed, else a new one. */
    async #openGuestSession() {
        const session = await this.#client.guestSession(this.#stored("guest"));
        this.#token = session.token;
        this.#store("guest", session.token);
    }
    #conversationKey() {
        return `conversation:${this.#workflow}`;
    }
    /** The conversation of an earlier visit, when the server still has it. */
    async #loadConversation() {
        const id = this.#stored(this.#conversationKey());
        if (id === undefined || !this.#state.config?.conversations) {
            return;
        }
        try {
            const conversation = await this.#client.conversations.get(id);
            this.#conversationId = id;
            const messages = conversation.messages.map((stored) => ({
                ...this.#message(stored.role, stored.content, "done"),
                runId: stored.run_id,
            }));
            this.#set({ messages });
        }
        catch (error) {
            if (error instanceof GraphrtError && error.status === 404) {
                this.#forget(this.#conversationKey());
                return;
            }
            throw error;
        }
    }
    /** The earlier turns, for a server that keeps no conversations. */
    #history() {
        return this.#state.messages
            .filter((message) => message.state === "done" && message.text !== "")
            .map((message) => ({ role: message.role, content: message.text }));
    }
    async #request(content, history) {
        const message = this.#input === null ? { sys: { query: content } } : { inputs: { [this.#input]: content } };
        if (this.#state.config?.conversations) {
            if (this.#conversationId === undefined) {
                const created = await this.#client.conversations.create(this.#workflow, this.#input === null ? {} : { input: this.#input });
                this.#conversationId = created.id;
                this.#store(this.#conversationKey(), created.id);
            }
            return { ...message, conversation_id: this.#conversationId };
        }
        return { ...message, sys: { ...message.sys, history } };
    }
    /** Apply the events of `runId` from `from` to the answer until the run ends. */
    async #follow(messageId, runId, from) {
        let text = this.#state.messages.find((message) => message.id === messageId)?.text ?? "";
        for await (const event of this.#client.events(runId, { from })) {
            text = this.#apply(messageId, event, text);
        }
        const message = this.#state.messages.find((candidate) => candidate.id === messageId);
        // A stream reopened after a lost connection can miss text of a run that
        // finished meanwhile: the record has the whole answer.
        if (message?.state === "done") {
            const record = await this.#client.getRun(runId).catch(() => undefined);
            if (record?.answer !== undefined && record.answer.length > message.text.length) {
                this.#update(messageId, { text: record.answer });
            }
        }
    }
    #apply(messageId, event, text) {
        switch (event.type) {
            case "response": {
                const next = text + event.data.text;
                this.#update(messageId, { text: next });
                return next;
            }
            case "run_succeeded":
                this.#update(messageId, { state: "done", blocks: collectUiBlocks(event.data.outputs ?? {}) });
                return text;
            case "run_failed":
                this.#update(messageId, { state: "failed", error: FAILED });
                return text;
            case "run_aborted":
                this.#update(messageId, { state: "stopped" });
                return text;
            case "run_paused": {
                const questions = [];
                for (const reason of event.data.reasons) {
                    const block = questionBlock(reason);
                    if (block !== undefined && reason.node_id) {
                        const key = reason.frame ? `${reason.frame}/${reason.node_id}` : reason.node_id;
                        questions.push({ key, block });
                    }
                }
                const canAnswer = this.#state.config?.resume === true && questions.length > 0;
                this.#update(messageId, {
                    state: "waiting",
                    questions: canAnswer ? questions : [],
                    ...(canAnswer ? {} : { error: "This request is waiting for someone to approve it." }),
                });
                return text;
            }
            default:
                return text;
        }
    }
    #message(role, text, state) {
        this.#nextId += 1;
        return { id: `m${this.#nextId}`, role, text, blocks: [], state, questions: [] };
    }
    #update(id, change) {
        const messages = this.#state.messages.map((message) => message.id === id ? { ...message, ...change } : message);
        this.#set({ messages });
    }
    #set(change) {
        this.#state = { ...this.#state, ...change };
        for (const listener of this.#listeners) {
            listener(this.#state);
        }
    }
    /** Storage keys are per server, so two chats on one site do not mix. */
    #storageKey(name) {
        return `graphrt-chat:${this.#options.baseUrl ?? ""}:${name}`;
    }
    #stored(name) {
        try {
            return this.#options.storage?.getItem(this.#storageKey(name)) ?? undefined;
        }
        catch {
            // Storage can be blocked (private mode, a sandboxed frame): the chat works without it.
            return undefined;
        }
    }
    #store(name, value) {
        try {
            this.#options.storage?.setItem(this.#storageKey(name), value);
        }
        catch {
            // See `#stored`.
        }
    }
    #forget(name) {
        try {
            this.#options.storage?.removeItem(this.#storageKey(name));
        }
        catch {
            // See `#stored`.
        }
    }
}
