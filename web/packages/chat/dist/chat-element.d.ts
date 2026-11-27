/**
 * `<graphrt-chat>`: a chat with one workflow of a `graphrt serve`.
 *
 * @example
 * <graphrt-chat server="https://agent.example.com" workflow="support"></graphrt-chat>
 *
 * Attributes: `server` (default: the page's origin), `workflow` (default: the
 * first the server offers), `heading`, `placeholder`, `greeting`, `theme`
 * (`light` or `dark`; absent follows the page) and `closable` (shows a close
 * button that fires `graphrt-chat-close`).
 *
 * Properties: `userToken` (a token or a function returning one, for servers
 * with `--chat-access users`) and `apiKey`.
 */
import type { TokenSource } from "@graphrt/client";
import { ChatSession } from "./session.ts";
declare const BaseElement: typeof HTMLElement;
export declare class GraphrtChatElement extends BaseElement {
    #private;
    static readonly observedAttributes: string[];
    /** A token for servers with `--chat-access users`. Set it before the element is connected. */
    userToken: TokenSource | undefined;
    /** An API key for servers with `--chat-access key`, instead of asking the person. */
    apiKey: string | undefined;
    /** The session behind the view, once the element is connected. */
    get session(): ChatSession | undefined;
    connectedCallback(): void;
    disconnectedCallback(): void;
    attributeChangedCallback(name: string, _old: string | null, value: string | null): void;
}
/** Register `<graphrt-chat>` (and `<graphrt-block>`). Safe to call more than once. */
export declare function defineGraphrtChat(): void;
export {};
