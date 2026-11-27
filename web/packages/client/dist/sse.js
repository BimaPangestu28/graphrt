/**
 * A parser for `text/event-stream` bodies read with `fetch`.
 *
 * `EventSource` cannot send an `Authorization` header, so tokens would have to
 * travel in the URL. Reading the stream with `fetch` keeps them in headers and
 * works the same for `POST` run starts and `GET` event replays.
 */
/**
 * Incremental SSE parser: feed it text as it arrives, it returns the messages
 * that are complete. Follows the WHATWG event stream rules (CR, LF and CRLF line
 * ends, comment lines, a single leading space after the colon stripped).
 */
export class SseParser {
    #buffer = "";
    #event = "";
    #data = [];
    #id;
    /** A CR ended the last chunk, so a LF that starts the next one belongs to it. */
    #pendingCr = false;
    /**
     * @param chunk - Decoded text of the stream, in order.
     * @returns The messages completed by this chunk.
     */
    push(chunk) {
        let text = chunk;
        if (this.#pendingCr && text.startsWith("\n")) {
            text = text.slice(1);
        }
        this.#pendingCr = text.endsWith("\r");
        this.#buffer += text;
        const messages = [];
        for (;;) {
            const end = this.#buffer.search(/\r\n|\n|\r/);
            if (end < 0) {
                break;
            }
            // A CR at the very end may be the first half of a CRLF: wait for more text.
            const isCrLf = this.#buffer.startsWith("\r\n", end);
            const line = this.#buffer.slice(0, end);
            this.#buffer = this.#buffer.slice(end + (isCrLf ? 2 : 1));
            const message = this.#line(line);
            if (message) {
                messages.push(message);
            }
        }
        return messages;
    }
    #line(line) {
        if (line === "") {
            return this.#dispatch();
        }
        if (line.startsWith(":")) {
            return undefined;
        }
        const colon = line.indexOf(":");
        const field = colon < 0 ? line : line.slice(0, colon);
        let value = colon < 0 ? "" : line.slice(colon + 1);
        if (value.startsWith(" ")) {
            value = value.slice(1);
        }
        if (field === "event") {
            this.#event = value;
        }
        else if (field === "data") {
            this.#data.push(value);
        }
        else if (field === "id" && !value.includes("\0")) {
            this.#id = value;
        }
        return undefined;
    }
    #dispatch() {
        const data = this.#data;
        const event = this.#event;
        const id = this.#id;
        this.#data = [];
        this.#event = "";
        this.#id = undefined;
        if (data.length === 0) {
            return undefined;
        }
        const message = { event: event === "" ? "message" : event, data: data.join("\n") };
        if (id !== undefined) {
            message.id = id;
        }
        return message;
    }
}
/**
 * Read an SSE response body as messages.
 *
 * @param body - The body of a `text/event-stream` response.
 * @returns The messages, in order, until the stream ends.
 * @throws The reader's error when the connection breaks.
 */
export async function* readSse(body) {
    const reader = body.getReader();
    const decoder = new TextDecoder();
    const parser = new SseParser();
    try {
        for (;;) {
            const { done, value } = await reader.read();
            if (done) {
                return;
            }
            yield* parser.push(decoder.decode(value, { stream: true }));
        }
    }
    finally {
        // Cancelling tells the server the client is gone (a streaming run start is aborted).
        await reader.cancel().catch(() => undefined);
    }
}
