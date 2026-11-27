# graphrt-client

A typed client of [`graphrt serve`](../../../docs/serving.md) for browsers.
It uses `fetch` only, so it also runs in Node.js 18+, Deno, Bun and workers.

```ts
import { GraphrtClient } from "graphrt-client";

const client = new GraphrtClient({ baseUrl: "https://agent.example.com", token });

for await (const event of client.stream("support", { sys: { query: "Hi" } })) {
  if (event.type === "response") output.textContent += event.data.text;
}
```

| Call | Does |
| --- | --- |
| `run`, `stream`, `startAsync` | Start a run: wait for it, receive its events, or start it in the background |
| `events(runId, { from })` | The events of a run, stored then live. A broken connection is reopened after the last event received |
| `abort`, `resume`, `resumeStream`, `resumeAsync` | Stop a run, or answer what a paused run waits for |
| `getRun`, `listRuns`, `feedback` | Read runs and rate them |
| `conversations.create/list/get/delete` | Chat history kept by the server |
| `chatConfig`, `guestSession` | What the server's chat page offers, and a guest session for it |

Tokens travel in headers, never in URLs: streams are read with `fetch`, not
`EventSource`. Answers are checked at run time, and a refusal is a
`GraphrtError` with the server's message and status.

See [docs/web.md](../../../docs/web.md#the-browser-client).
