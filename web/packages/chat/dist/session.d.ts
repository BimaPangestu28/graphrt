/**
 * A chat with one workflow of a `graphrt serve`: the state a chat view shows
 * and the calls that change it. No DOM here, so it also serves a custom view.
 *
 * A message is sent as a background run that is followed by its events, so a
 * phone that loses its connection for a moment picks the answer up where it
 * was. The history lives on the server when it keeps conversations, and is
 * sent with every message (`sys.history`) when it does not.
 */
import { type ChatConfig, type Json, type PauseReason, type TokenSource } from "@graphrt/client";
/** Where a session keeps what must survive a reload. `localStorage` fits. */
export interface KeyValueStore {
    getItem(key: string): string | null;
    setItem(key: string, value: string): void;
    removeItem(key: string): void;
}
/** A question a paused run asks the person. */
export interface PendingQuestion {
    /** The key of the resume input that answers it. */
    key: string;
    /** A UI block to show: the node's own card, or a card built from its form. */
    block: Json;
}
export interface ChatMessage {
    id: string;
    role: "user" | "assistant";
    text: string;
    /** UI blocks of the run's outputs, as they came: the view validates them. */
    blocks: unknown[];
    /** The run that took the message or gave the answer. */
    runId?: string | undefined;
    /** `streaming` while the answer arrives; `waiting` while its run is paused. */
    state: "done" | "streaming" | "waiting" | "failed" | "stopped";
    /** Why it failed, in words for the person. */
    error?: string | undefined;
    questions: PendingQuestion[];
    /** The rating the person gave, when they did. */
    rating?: "up" | "down" | undefined;
}
export interface ChatState {
    /** `loading` until the server said what it offers; `locked` until a key is entered. */
    status: "loading" | "locked" | "ready" | "unavailable";
    title: string;
    messages: ChatMessage[];
    /** Whether an answer is being produced. */
    busy: boolean;
    /** Why the chat cannot be used, or why the key was refused. */
    notice?: string | undefined;
    config?: ChatConfig | undefined;
}
export interface ChatSessionOptions {
    /** Where the server is. Default: the page's own origin. */
    baseUrl?: string;
    /** The workflow to chat with. Default: the first one the server offers. */
    workflow?: string;
    /** An end-user token for a server with `--chat-access users`. */
    userToken?: TokenSource;
    /** An API key, for a server with `--chat-access key` (else the view asks for it). */
    apiKey?: string;
    /** Keeps the guest session and the conversation across reloads. Default: none. */
    storage?: KeyValueStore;
    fetch?: typeof fetch;
}
type Listener = (state: ChatState) => void;
/**
 * The card for a `human_input` pause: the node's own `card` when it has one,
 * else an Adaptive Card with its prompt, form fields and actions. The actions'
 * ids are the node's action ids, so the card's answer is the resume input.
 */
export declare function questionBlock(reason: PauseReason): Json | undefined;
/** One chat. Create it, {@link ChatSession.start} it, then {@link ChatSession.send} messages. */
export declare class ChatSession {
    #private;
    constructor(options?: ChatSessionOptions);
    /** What a view shows now. A new object after every change. */
    get state(): ChatState;
    /**
     * Call `listener` after every change of the state.
     *
     * @returns A function that stops the calls.
     */
    subscribe(listener: Listener): () => void;
    /**
     * Ask the server what it offers, sign in as it requires and load the
     * conversation of an earlier visit. Never throws: what went wrong is in the state.
     */
    start(): Promise<void>;
    /**
     * Use `key` as the API key (a server with `--chat-access key`).
     *
     * @param remember - Keep the key in the storage for the next visit.
     * @returns Whether the server accepted it.
     */
    unlock(key: string, remember?: boolean): Promise<boolean>;
    /** Forget the key and lock the chat again. */
    lock(): void;
    /**
     * Send a message and follow its answer until the run ends. Never throws: a
     * failure is shown on the answer.
     */
    send(text: string): Promise<void>;
    /** Stop the answer that is being produced. */
    stop(): Promise<void>;
    /**
     * Answer a question of a paused run and follow the rest of its answer.
     *
     * @param action - The id of the action the person chose.
     * @param values - The values of the question's inputs.
     */
    answer(messageId: string, key: string, action: string | undefined, values: Record<string, Json>): Promise<void>;
    /** Rate an answer. A second call changes the rating. */
    rate(messageId: string, rating: "up" | "down"): Promise<void>;
    /** Start over: a new conversation, the old one stays on the server until it expires. */
    reset(): void;
}
export {};
