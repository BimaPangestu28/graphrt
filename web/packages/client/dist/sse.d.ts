/**
 * A parser for `text/event-stream` bodies read with `fetch`.
 *
 * `EventSource` cannot send an `Authorization` header, so tokens would have to
 * travel in the URL. Reading the stream with `fetch` keeps them in headers and
 * works the same for `POST` run starts and `GET` event replays.
 */
/** One server-sent event. */
export interface SseMessage {
    /** The `event:` field, `message` when the server sent none. */
    event: string;
    /** The `data:` lines joined with a line feed. */
    data: string;
    /** The `id:` of this message, when it carried one. */
    id?: string;
}
/**
 * Incremental SSE parser: feed it text as it arrives, it returns the messages
 * that are complete. Follows the WHATWG event stream rules (CR, LF and CRLF line
 * ends, comment lines, a single leading space after the colon stripped).
 */
export declare class SseParser {
    #private;
    /**
     * @param chunk - Decoded text of the stream, in order.
     * @returns The messages completed by this chunk.
     */
    push(chunk: string): SseMessage[];
}
/**
 * Read an SSE response body as messages.
 *
 * @param body - The body of a `text/event-stream` response.
 * @returns The messages, in order, until the stream ends.
 * @throws The reader's error when the connection breaks.
 */
export declare function readSse(body: ReadableStream<Uint8Array>): AsyncGenerator<SseMessage, void, void>;
