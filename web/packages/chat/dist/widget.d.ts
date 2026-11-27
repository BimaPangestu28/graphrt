/**
 * `<graphrt-chat-widget>`: a chat bubble in a corner of any page. Pressing it
 * opens a panel with a `<graphrt-chat>`; on a phone the panel fills the screen.
 *
 * Attributes: `server`, `workflow`, `heading`, `greeting`, `placeholder`,
 * `theme` (passed on to the chat), `position` (`right`, the default, or
 * `left`) and `open`.
 */
import type { TokenSource } from "@graphrt/client";
declare const BaseElement: typeof HTMLElement;
export declare class GraphrtChatWidgetElement extends BaseElement {
    #private;
    static readonly observedAttributes: string[];
    /** A token for servers with `--chat-access users`. Set it before the chat is first opened. */
    userToken: TokenSource | undefined;
    connectedCallback(): void;
    attributeChangedCallback(name: string, _old: string | null, value: string | null): void;
    /** Open the chat panel. The chat connects to the server the first time. */
    open(): void;
    close(): void;
    toggle(): void;
}
/** Register `<graphrt-chat-widget>` and what it needs. Safe to call more than once. */
export declare function defineGraphrtChatWidget(): void;
export {};
