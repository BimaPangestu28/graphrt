import type { PushBrowser } from "./session.ts";
/** A base64url text as the bytes browsers take for `applicationServerKey`. */
export declare function base64UrlBytes(text: string): Uint8Array<ArrayBuffer>;
/**
 * The push notifications of this browser.
 *
 * @example
 * const monitor = document.createElement("graphrt-monitor");
 * monitor.push = browserPush();
 */
export declare function browserPush(): PushBrowser;
