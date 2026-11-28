# @graphrt-dev/graphrt

The browser side of [`graphrt serve`](https://github.com/Graphrt/graphrt/blob/main/docs/serving.md):
a typed client, web components that draw UI blocks, the chat widget, the
monitoring view and the admin console. One package, five entry points: import only the part you use,
and a bundler leaves the rest out.

```sh
npm install @graphrt-dev/graphrt
```

| Import | What |
| --- | --- |
| `@graphrt-dev/graphrt/client` | A typed client: blocking, streamed and background runs, events with replay, abort, resume, feedback, conversations |
| `@graphrt-dev/graphrt/ui` | Web components that draw UI blocks: Adaptive Cards, charts, tables, stat tiles, diagrams. No dependencies |
| `@graphrt-dev/graphrt/chat` | The chat component and the widget, built on the two above |
| `@graphrt-dev/graphrt/monitor` | The monitoring view: runs, run details, the approvals inbox, push notifications |
| `@graphrt-dev/graphrt/console` | The admin console of `graphrt serve --console`: agents and versions per environment, runs, secrets, people and roles, the audit log |

The package is ES modules with type declarations. `@graphrt-dev/graphrt/client` uses `fetch`
only, so it also runs in Node.js 18+, Deno, Bun and workers. The other parts
need a browser (or a DOM such as happy-dom). Nothing runs on import: the web
components are registered when you call their `define` function.

## Client

```ts
import { GraphrtClient } from "@graphrt-dev/graphrt/client";

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

## UI blocks

```ts
import { defineGraphrtUi } from "@graphrt-dev/graphrt/ui";

defineGraphrtUi();
const element = document.createElement("graphrt-block");
element.block = { $ui: "stats", version: 1, items: [{ label: "Orders", value: 42 }] };
document.body.append(element);
```

Blocks come from models and tools, so every block is validated before it is
drawn (a port of the `graphrt-ui` crate's validator, tested against the same
fixtures), all text reaches the page as text, and nothing is loaded from
anywhere.

## Chat

```html
<graphrt-chat server="https://agent.example.com" workflow="support"></graphrt-chat>
```

```ts
import { defineGraphrtChat } from "@graphrt-dev/graphrt/chat";

defineGraphrtChat();
```

| Export | What |
| --- | --- |
| `<graphrt-chat>` | The chat, to place in a page |
| `<graphrt-chat-widget>` | A bubble in a corner that opens it |
| `ChatSession` | The logic without a view, for a chat of your own design |
| `loadDiagrams` | Draws `diagram` blocks with the Mermaid a server serves (`--chat-mermaid`) |
| `GraphrtChatApi`, `ChatEventMap`, `ChatContext`, ... | Types of the widget script's API `window.GraphrtChat` |

Both elements have `send(text)`, `setContext(context)` and `reset()`, and fire
`graphrt-chat-ready`, `-message`, `-answer`, `-handoff`, `-error`, `-open` and
`-close` with the payload in `detail`. The context reaches the workflow as
`sys.context`: untrusted page data, never identity.

A server started with `--chat` serves both ready to use, without this package:
the page `/chat/` and the one script tag `/chat/embed.js`. See
[Embed SDK](https://github.com/Graphrt/graphrt/blob/main/docs/web.md#embed-sdk).

## Monitor

```html
<graphrt-monitor server="https://agent.example.com"></graphrt-monitor>
```

```ts
import { browserPush, defineGraphrtMonitor } from "@graphrt-dev/graphrt/monitor";

defineGraphrtMonitor();
const monitor = document.querySelector("graphrt-monitor");
monitor.push = browserPush(); // needs a service worker on the page
```

| Export | What |
| --- | --- |
| `<graphrt-monitor>` | The view, to place in a page |
| `MonitorSession` | The logic without a view, for a view of your own design |
| `browserPush` | Push notifications of the browser, for `monitor.push` |
| `notificationOf` | What a service worker shows for a push message of the server |
| `encodeQr`, `qrSvg` | The QR code of a pairing link, drawn in the page |

A server started with `--monitor` serves it ready to use at `/monitor/`.

## Admin console

```ts
import { defineGraphrtConsole } from "@graphrt-dev/graphrt/console";

defineGraphrtConsole(); // <graphrt-console server="https://agents.example.com">
```

| Export | What |
| --- | --- |
| `<graphrt-console>` | The console, to place in a page of the server's own origin |
| `ConsoleSession` | The logic without a view |

People sign in with their organization's account at the server, so the page
must come from the server's origin: the session is an `HttpOnly` cookie and
every change carries the session's CSRF token. A server started with
`--console` serves it ready to use at `/console/`.

## More

- [Browser guide](https://github.com/Graphrt/graphrt/blob/main/docs/web.md)
- [Monitoring guide](https://github.com/Graphrt/graphrt/blob/main/docs/monitoring.md)
- [Admin console](https://github.com/Graphrt/graphrt/blob/main/docs/console.md)

The version of this package is the version of the graphrt release it was
published with. Licensed under Apache-2.0.
