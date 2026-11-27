/**
 * `@graphrt/client`: a typed client of `graphrt serve` for browsers.
 *
 * @example
 * import { GraphrtClient } from "@graphrt/client";
 *
 * const client = new GraphrtClient({ baseUrl: "https://agent.example.com", token: apiKey });
 * const result = await client.run("hello", { inputs: { name: "Ada" } });
 */
export { Conversations, GraphrtClient, GraphrtError, isTerminal, } from "./client.js";
export { Approvals, Devices, Push } from "./monitoring.js";
export { readSse, SseParser } from "./sse.js";
