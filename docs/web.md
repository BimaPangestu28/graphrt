# Using a served agent from a browser

`graphrt serve` can be used from a web page: a chat page it serves itself, a
chat widget for another site, or your own page built on the browser client.
Everything runs on your server. Nothing here needs a hosted service, and each
piece is off until you ask for it.

| You want | Start `graphrt serve` with | Then |
| --- | --- | --- |
| A chat page for yourself or your team | `--chat WORKFLOW` | Open `/chat/` and enter the API key once |
| A public chat page | `--chat WORKFLOW --chat-access guests` | Share the link `/chat/WORKFLOW` |
| A chat bubble on your site | the above, plus `--cors-origin https://your.site` | Add [one script tag](#the-widget) |
| A chat for signed in users | `--chat WORKFLOW --chat-access users --verify-jwt ...` | Hand the widget [the user's token](#signed-in-users) |
| The chat to know the page and to react to it | the widget | The [Embed SDK](#embed-sdk): events, `send`, page context |
| Your own page | `--cors-origin https://your.site` | Use the [browser client](#the-browser-client) |
| The server to keep the history | `--conversations` | Send `conversation_id` instead of `sys.history` |

```sh
GRAPHRT_API_KEY=... graphrt serve flows/ --store ./runs \
  --chat support --chat-access guests --conversations
```

Every flag in this table can also be set as an environment variable
(`GRAPHRT_CHAT=support`, `GRAPHRT_CHAT_ACCESS=guests`,
`GRAPHRT_CORS_ORIGINS=https://your.site`, `GRAPHRT_CONVERSATIONS=1`), for a
container without a command line of its own: see
[serving.md](serving.md#flags-as-environment-variables).

The cargo features are `cors`, `run-follow`, `conversations` and `web-chat`.
They are in the default build and in the released binaries. A build without
them (`--no-default-features --features serve,llm,...`) has none of this code.

## The chat page

`--chat WORKFLOW` serves a chat with that workflow:

| Path | What |
| --- | --- |
| `/chat/` | The chat with the first `--chat` workflow |
| `/chat/WORKFLOW` | The chat with that workflow (repeat `--chat` to offer several) |
| `/chat/embed.js` | The [widget](#the-widget) |
| `/chat/mermaid.js` | Mermaid, only with [`--chat-mermaid`](#diagrams-in-the-chat) |
| `GET /v1/chat/config` | What the page offers: title, workflows, who may chat. Public |
| `POST /v1/chat/guest` | A [guest session](#guests). Public, only with `--chat-access guests` |

The message of the person is `sys.query` of the run. A workflow whose start
requires exactly one input (required, no default) gets the message there, as a
[channel](channels.md) does, so `--chat support` works for a workflow that asks
for `message`. Otherwise name the input: `--chat support=message`. A workflow
that requires several inputs needs that name and defaults for the others, or
the server does not start.

The page shows the answer as it streams, [UI blocks](workflow-format.md#ui-blocks)
of the run's outputs (cards, charts, tables, stat tiles, diagrams), and
the questions of `human-input` nodes as forms. It works at phone width, in
light and dark, and follows the system setting.

`--chat-title TEXT` sets the title. Behind a reverse proxy the page also works
below a path prefix (`https://example.com/agent/chat/`): every URL it uses is
relative to where its script came from.

### Diagrams in the chat

A `diagram` block is Mermaid text. The chat shows that text unless the server
has a Mermaid build to draw it with:

```sh
curl -fLo mermaid.min.js https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.min.js
graphrt serve flows/ --chat support --chat-mermaid mermaid.min.js
```

The server reads the file at startup and serves it at `/chat/mermaid.js`. The
chat page and the widget load it from there when a diagram first shows, so a
chat without diagrams never downloads it, and visitors' browsers still load
code from your server only. Mermaid is about 3 MB, which is why it is your file
and not part of the graphrt binary (the whole chat page is 0.1 MB). The
picture is shown as an image, never as markup of the page, and the page's
`Content-Security-Policy` stays as strict as without it.

On a site that already has Mermaid (`window.mermaid`), the widget uses that
one and sets it to its strict security level.

### Who may chat

`--chat-access` decides it:

| Value | Who | How they sign in |
| --- | --- | --- |
| `key` (default) | Holders of the API key | The page asks for the key once. It stays in the page's memory, or in the browser's storage when "Remember on this device" is ticked |
| `users` | Users your identity provider signed in | The page or widget sends the user's token, which the server verifies (`--verify-jwt`, see [serving.md](serving.md#verifying-end-user-jwts)) |
| `guests` | Anyone who opens the page | Nothing. Each browser gets a guest session |

Without an API key (a loopback server while you develop), `key` asks for
nothing.

### Guests

With `--chat-access guests` each browser gets a guest token from
`POST /v1/chat/guest` and sends it as `Authorization: Bearer`. A guest is a
user like a signed in one, with the id `guest:<random>`:

- A guest starts only the `--chat` workflows. Every other workflow answers `403`.
- A guest reaches only their own runs and conversations, and sees the answer
  and the final result only, never node events (the rules are in
  [serving.md](serving.md#what-a-user-token-may-do)).
- `--chat-guest-rate-limit N` (default 10) limits the runs per minute of one
  guest. A session costs nothing to open, so this alone does not bound what a
  public page can spend: `--chat-guest-budget N` (default 120) limits the runs
  per minute of all guests together. `--max-active-runs` still caps the runs
  that execute at once.

The server keeps no state per guest. A token is `grg1.<id>.<expires>.<signature>`,
signed with a key derived from the API key (HMAC-SHA256), so it survives
restarts and works on every replica with the same API key. It lives 12 hours.
The page renews it (the same call with the old token) for up to 30 days after
it expired, which keeps the guest and their conversations.

A public page spends your model budget. Put destructive and outbound tools of
the workflow under `require_approval`, and do not offer a workflow to guests
that reads data of other people.

## The widget

One script tag adds a chat bubble to any page:

```html
<script src="https://agent.example.com/chat/embed.js" data-workflow="support" async></script>
```

The server is the one the script came from. It must allow the page's origin:
`--cors-origin https://www.example.com`.

| Attribute | Meaning |
| --- | --- |
| `data-workflow` | The workflow to chat with. Default: the first `--chat` workflow |
| `data-heading` | Title of the panel. Default: `--chat-title`, else the workflow name |
| `data-greeting` | Text shown before the first message |
| `data-placeholder` | Placeholder of the message box |
| `data-theme` | `light` or `dark`. Default: the visitor's system setting |
| `data-position` | `right` (default) or `left` |
| `data-open` | Open the panel when the page loads |

The bubble opens a panel. On a phone the panel fills the screen. The chat
connects to the server when the panel is first opened, so a page whose visitor
never opens it makes no request.

`window.GraphrtChat` is the [Embed SDK](#embed-sdk): events, `send`, the page
context and the options in JavaScript.

The widget lives in a shadow tree: the styles of your page do not change it and
its styles do not leak into your page. If your site has a
`Content-Security-Policy`, allow the script and the connection:
`script-src https://agent.example.com; connect-src https://agent.example.com`
(and `img-src data:` for [diagrams](#diagrams-in-the-chat)).

To place the chat inside your layout instead of in a bubble, use the component
of the npm package [`@graphrt-dev/graphrt`](../web/packages/graphrt) (`@graphrt-dev/graphrt/chat`):
`<graphrt-chat server="https://agent.example.com" workflow="support">`.

### Signed in users

With `--chat-access users` the chat sends the token of your identity provider
(Supabase Auth, Auth0, Firebase...) in the header `--verify-jwt` verifies:

```html
<script>
  window.GraphrtChat = window.GraphrtChat || { q: [] };
  // A function is asked for the token before every request, so a refreshed token is used.
  GraphrtChat.q.push(["setUserToken", () => auth.currentSession().access_token]);
</script>
<script src="https://agent.example.com/chat/embed.js" data-workflow="support" async></script>
```

For the chat page, pass the token in the fragment: `/chat/support#token=...`.
A fragment is never sent to a server, and the page removes it from the address
bar at once.

## Embed SDK

The widget script is also a small API, `window.GraphrtChat`. With it a site
passes what the visitor looks at, sends messages for them and reacts to the
chat. Everything below is in `/chat/embed.js`, which stays about 36 KB
gzipped.

### Calling it before the script loaded

The script loads `async`, so a page queues its calls in a stub. The script
runs the queue in order when it loads, before the bubble shows. After that,
`GraphrtChat.q.push` runs a call at once, so the same code works whenever it
runs:

```html
<script>
  window.GraphrtChat = window.GraphrtChat || { q: [] };
  GraphrtChat.q.push(["init", { workflow: "support", greeting: "Questions about your order?" }]);
  GraphrtChat.q.push((chat) => chat.on("answer", (answer) => console.log(answer.text)));
</script>
<script src="https://agent.example.com/chat/embed.js" async></script>
```

A queued call is `[method, ...arguments]` or a function that gets the API.
An error in one is reported in the console and the calls after it still run.

### Options

`GraphrtChat.init({...})` takes the same options as the `data-` attributes of
the script tag, and wins over them:

| `init` option | Attribute | Meaning |
| --- | --- | --- |
| `workflow` | `data-workflow` | The workflow to chat with |
| `heading` | `data-heading` | Title of the panel |
| `greeting` | `data-greeting` | Text shown before the first message |
| `placeholder` | `data-placeholder` | Placeholder of the message box |
| `theme` | `data-theme` | `light`, `dark` or `auto` (the visitor's system setting, the default) |
| `position` | `data-position` | `right` (default) or `left` |
| `open` | `data-open` | Open the panel at once |
| `userToken` | | The signed in user's token, or a function that returns it |
| `context` | | The [page context](#page-context) |

Every option counts until the chat first opens. After that, the look changes
(`heading`, `greeting`, `placeholder`, `theme`, `position`) and the workflow
stays.

### Methods

| Method | What |
| --- | --- |
| `open()`, `close()`, `toggle()` | Show or hide the panel |
| `send(text)` | Open the panel and send `text` as the visitor's message once the chat is ready. Calls made early are sent in order. Returns a promise that resolves when the answer is complete |
| `setContext(object)` | Set the [page context](#page-context) of the next messages. `null` clears it |
| `reset()` | Start a new conversation |
| `setTheme(theme)` | `light`, `dark` or `auto` |
| `setUserToken(token)` | The signed in user's token. Set it before the chat opens |
| `on(event, listener)` | Call `listener(detail)` on every `event`. Returns a function that removes it |
| `off(event, listener)` | Remove a listener |
| `element` | The `<graphrt-chat-widget>` element |

The chat connects to the server when it first opens (or on the first `send`),
so a page that never opens it still makes no request.

### Events

| Event | When | `detail` |
| --- | --- | --- |
| `open` | The panel opened | `{workflow}` |
| `close` | The panel closed, however it was closed | `{workflow}` |
| `ready` | The chat connected and takes messages | `{workflow, title}` |
| `message` | The visitor sent a message (typed, or with `send`) | `{id, text}` |
| `answer` | An answer is complete | `{id, runId, text, blocks}`, `blocks` are the [UI blocks](workflow-format.md#ui-blocks) of the run's outputs |
| `handoff` | The answer waits for a person: a question in the chat (`waitingFor: "visitor"`) or an approval by your team (`"team"`) | `{id, runId, waitingFor}` |
| `error` | The chat cannot be used (`during: "connect"`), or an answer failed (`"answer"`) | `{during, message, id}`, `message` in words for the visitor |

A first message gives `open`, `ready`, `message`, `answer`, in that order.

### Page context

`setContext` tells the workflow about the page: the product the visitor looks
at, the page URL, the cart, a display name. It goes with every following
message as `sys.context`, so a node reads `{{#sys.context.product_id#}}`, and
an LLM prompt can include `{{#sys.context#}}` as JSON.

- **A JSON object** of at most 8 KiB (as UTF-8 JSON) and 5 levels of nesting.
  Strings, numbers, booleans, `null`, arrays and plain objects only. A property
  that is `undefined` is left out. Anything else throws a `TypeError` in the
  page and the context before stays. The server checks the same limits and
  answers `400` for a context out of them (cargo feature `web-chat`, in the
  default build and the released binaries). A build without that feature
  cannot check a context, so it refuses every one with `400`.
- **Untrusted.** It comes from the browser, so a visitor can change it to
  anything. Never put a secret, a price to charge, a role or a permission in
  it. Nothing in the server reads it, and the user is always the
  [verified token](#signed-in-users) (`sys.user_id`), whatever the context
  says. A workflow that looks up an order must check that the order belongs
  to `sys.user_id`, not trust an order id because it came in the context.
- **Like the message.** It reaches the run like `sys.query`: it is part of the
  run's input and can appear in traces.

### Examples

A product page that tells the chat what the visitor looks at, and asks for
them:

```html
<script>
  window.GraphrtChat = window.GraphrtChat || { q: [] };
  GraphrtChat.q.push(["setContext", {
    product_id: "sku-1042",
    product_name: "Rain jacket, blue",
    page: location.href,
  }]);
</script>
<script src="https://agent.example.com/chat/embed.js" data-workflow="shop" async></script>

<button onclick="GraphrtChat.send('Is this jacket available in size M?')">Ask about sizes</button>
```

The workflow reads it:

```yaml
- id: answer
  type: llm
  model: { provider: openai, name: gpt-4o-mini }
  prompt_template:
    - role: system
      text: "The visitor looks at {{#sys.context.product_name#}} ({{#sys.context.product_id#}})."
    - role: user
      text: "{{#sys.query#}}"
```

A site with signed in users (`--chat-access users`): the token says who they
are, the context only how to greet them:

```html
<script>
  window.GraphrtChat = window.GraphrtChat || { q: [] };
  GraphrtChat.q.push(
    ["setUserToken", () => auth.currentSession().access_token],
    ["setContext", { display_name: currentUser.firstName, cart_items: cart.length }],
    (chat) => chat.on("handoff", () => analytics.track("chat_handoff")),
  );
</script>
<script src="https://agent.example.com/chat/embed.js" data-workflow="support" async></script>
```

React, with the script tag in the page's HTML. The context follows the
product shown, and the listener is removed with the component:

```tsx
import { useEffect } from "react";
import type { GraphrtChatApi, GraphrtChatStub } from "graphrt/chat";

declare global {
  interface Window {
    GraphrtChat?: GraphrtChatApi | GraphrtChatStub;
  }
}

function chat(call: (api: GraphrtChatApi) => void): void {
  window.GraphrtChat = window.GraphrtChat ?? { q: [] };
  window.GraphrtChat.q.push(call);
}

export function ProductPage({ product }: { product: { id: string; name: string } }) {
  useEffect(() => {
    chat((api) => api.setContext({ product_id: product.id, product_name: product.name }));
  }, [product.id, product.name]);

  useEffect(() => {
    let stop: (() => void) | undefined;
    chat((api) => {
      stop = api.on("answer", (answer) => console.log("answered", answer.runId));
    });
    return () => stop?.();
  }, []);

  return <button onClick={() => chat((api) => void api.send(`Tell me about ${product.name}`))}>Ask</button>;
}
```

The types (`GraphrtChatApi`, `GraphrtChatInitOptions`, `ChatEventMap`,
`ChatContext` and the `detail` of each event) come with the npm package
[`graphrt`](../web/packages/graphrt) as `graphrt/chat`. Only the types are
needed for the script tag. They are strict, without `any`.

### The same in the components

`<graphrt-chat>` and `<graphrt-chat-widget>` of `graphrt/chat` have the same
methods (`send`, `setContext`, `reset`, the widget also `open`, `close`,
`toggle`) and fire the events as DOM events named `graphrt-chat-<event>`,
with the payload in `detail`. They leave the shadow tree, so a listener on
the element or on `document` gets them:

```ts
import { defineGraphrtChat, type GraphrtChatElement } from "graphrt/chat";

defineGraphrtChat();
const chat = document.querySelector("graphrt-chat") as GraphrtChatElement;
chat.setContext({ product_id: "sku-1042" });
chat.addEventListener("graphrt-chat-answer", (event) => console.log(event.detail.text));
```

Without a view, `ChatSession` has `setContext` and `on(listener)` for the
`ready`, `message`, `answer`, `handoff` and `error` events.

## Cross-origin requests (CORS)

A page on another origin can call the API only when the server lists that
origin:

```sh
graphrt serve flows/ --cors-origin https://app.example.com --cors-origin http://localhost:5173
```

`$GRAPHRT_CORS_ORIGINS` takes the same list, separated by commas, for a
container without a shell.

- **Off by default.** Without the flag the server sends no CORS header, as
  before.
- **An explicit list.** An origin is `scheme://host[:port]` and is matched
  exactly: another scheme, port or subdomain is another origin. The answer
  echoes the request's origin, never a pattern.
- **No cookies.** `Access-Control-Allow-Credentials` is never sent. The API
  takes tokens in headers, which a page adds itself, so a browser's cookies or
  other ambient credentials are never accepted from another origin. That is
  also why the API needs no CSRF token: a request from a hostile page carries
  nothing the server trusts.
- **`*` allows any origin**, still without credentials, and only with an API
  key: the server refuses to start with `--cors-origin '*'` and no key, because
  any web page a user of that machine opens could then call the API.
- **Preflights** (`OPTIONS`) are answered before authentication, since browsers
  send them without the token. Allowed request headers are `authorization`,
  `content-type`, `last-event-id`, `traceparent`, `tracestate` and the headers
  mapped with `--input-header`.
- **Every answer has `Vary: Origin`**, so a shared cache never serves one
  origin's answer to another.
- **Event streams** are ordinary responses and are covered like the rest. The
  browser client reads them with `fetch`, not `EventSource`, so the token stays
  in a header and out of URLs and logs.

A request from an origin that is not listed is not blocked by the server: it
gets no CORS header, so the browser keeps the answer from the page, and it still
needs a token that page does not have. The protection of a keyless loopback
server against DNS rebinding (only loopback `Host` names) is unchanged, see
[security.md](security.md#graphrt-serve).

Chrome asks the visitor for permission before a public site may call a server
on the local network or on `localhost` (Local Network Access). There is no
header for the server to send: serve the page from the same host, or use a
public address for the agent.

## Conversations kept by the server

Without `--conversations` a chat client sends the earlier turns with every
message ([`sys.history`](workflow-format.md#conversation-history-syshistory)).
That keeps working unchanged. With `--conversations` the server keeps them:

```sh
curl -s localhost:8080/v1/conversations -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' -d '{"workflow": "support"}'
# {"id": "conv-18f2c...", "workflow": "support", "title": "", "message_count": 0, ...}

curl -s localhost:8080/v1/workflows/support/runs -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"conversation_id": "conv-18f2c...", "sys": {"query": "Where is my order?"}}'
```

A run that names a `conversation_id` gets:

- `sys.history`: the stored turns, oldest first. A node's `memory.window`
  decides how many reach the model.
- the `conversation` variables as the last run of the conversation left them
  (what the request sets wins).
- `sys.conversation_id`, and the conversation's id as the `session_id` of the
  run record, so traces group by conversation.

When the run succeeds, its message and its answer are added as a turn, before
the run's last event goes out: a client that sends the next message right
away finds the turn in the history. A run that fails or is aborted adds
nothing. A paused run adds its turn when a resume succeeds.

| Request | Answer |
| --- | --- |
| `POST /v1/conversations` `{workflow, input?, title?}` | `201` and the conversation |
| `GET /v1/conversations?workflow=&user=&limit=&cursor=` | `{conversations, next_cursor}`, most recently used first |
| `GET /v1/conversations/{id}` | The conversation with `messages` (`{role, content, run_id, created_ms}`) |
| `DELETE /v1/conversations/{id}` | `204` |

`input` names the start input that takes the message, for workflows that do
not read `sys.query`. The title is the first line of the first message unless
you set one.

**Who reaches what.** A conversation belongs to who created it. A verified
user (their token's `sub`, or a guest) reaches only their own: another user's
answers `404`, like an unknown id, also in a run request. Holders of the API
key reach all of them, see the `owner`, and can filter with `?user=`.

**Limits and retention.**

| Flag | Default | Meaning |
| --- | --- | --- |
| `--conversation-max-messages N` | 200 | Messages kept per conversation. Beyond it the oldest turns are dropped. A conversation also holds at most 4 MiB of text. The newest turn is always kept |
| `--conversations-per-user N` | 50 | Conversations per end user. A new one beyond it replaces the user's least recently used one. Key callers are not limited |
| `--conversation-ttl-days DAYS` | 30 | Conversations not used for this long are deleted, hourly (`0` keeps them) |

**Where they live.** With `--store DIR` in `DIR/conversations/`, one JSON file
each, written atomically, readable by the server's user only, and safe to share
between servers on one directory. Without a store they are in memory and gone
at a restart.

Two runs of one conversation at the same time both get the history as it was
when they started, and their turns are added in the order they finish.

## A user's own runs

A verified user (a token of `--verify-jwt`, or a guest) can follow, stop and
read the runs they started, so a page can start a run in the background and
pick it up again after a lost connection:

| Request | For a user |
| --- | --- |
| `GET /v1/runs/{id}` | Their own run: status, answer, outputs, what it waits for. No error text, inputs, metadata or trace ids |
| `GET /v1/runs/{id}/events` | The events they would have seen on the stream, with the answer text |
| `POST /v1/runs/{id}/abort` | Stops their own run |
| `POST /v1/runs/{id}/resume` | Only with `--user-resume`, see below |

A run of someone else answers `404`. Run lists and deletes stay with the API
key. The full table is in [serving.md](serving.md#what-a-user-token-may-do).

**`--user-resume`** lets a user answer the questions of their own paused runs.
It is off by default because a pause is not always a question to the person
chatting: an agent tool behind `require_approval` pauses the run so that
someone else approves the action. With the flag on, the user who asked for the
refund could approve it. Turn it on only when every pause of the workflows that
users start is a question to that user. A user can never send a `snapshot`.

### Following a run

`GET /v1/runs/{id}/events?responses=true` adds the answer text to the stream as
`response` events, as a streaming run start sends them (always for a user who
sees the answer only):

- For the execution in progress: the same events with the same ids as the
  caller that started the run got. A client that reconnects with
  `Last-Event-ID` gets exactly the rest.
- For a run that is not executing: one `response` event with the stored answer,
  just before the run's last event, when the stream starts at the first event.

The `202` answer of an async start or resume has `first_event`: the index of
the execution's first event. Follow a resumed run from there
(`?from=<first_event>`), because the events before it end with the pause.

## The browser client

`@graphrt-dev/graphrt/client`, part of the npm package [`@graphrt-dev/graphrt`](../web/packages/graphrt),
is a typed client for browsers. It uses `fetch` only, so it also runs in
Node.js 18+, Deno, Bun and workers.

```ts
import { GraphrtClient } from "@graphrt-dev/graphrt/client";

const client = new GraphrtClient({ baseUrl: "https://agent.example.com", token });

// Wait for the result.
const result = await client.run("support", { sys: { query: "Hi" } });

// Or receive the answer as it is written.
for await (const event of client.stream("support", { sys: { query: "Hi" } })) {
  if (event.type === "response") output.textContent += event.data.text;
}

// Or start it in the background and follow it. A broken connection is
// reopened after the last event received: every event arrives exactly once.
const { run_id } = await client.startAsync("support", { sys: { query: "Hi" } });
for await (const event of client.events(run_id)) { /* ... */ }
await client.abort(run_id);
```

It also has `resume`, `resumeStream`, `resumeAsync`, `feedback`, `getRun`,
`listRuns`, `conversations.create/list/get/delete`, `chatConfig` and
`guestSession`. Answers are checked at run time (zod), and a refusal is a
`GraphrtError` with the server's message, the status and `retryAfterSeconds`.

`token` is sent as `Authorization: Bearer` (the API key, or a guest token).
`userToken` is an end-user JWT, sent in `userTokenHeader` (default
`X-User-Authorization`). Both can be functions, asked before every request.

Do not ship the API key in a page that other people load: it reads and resumes
every run. Use guests or user tokens there.

## Drawing UI blocks

`@graphrt-dev/graphrt/ui` (in [`web/packages/graphrt`](../web/packages/graphrt)) draws
[UI blocks](workflow-format.md#ui-blocks) as a web component:

```ts
import { collectUiBlocks, defineGraphrtUi } from "@graphrt-dev/graphrt/ui";

defineGraphrtUi();
for (const block of collectUiBlocks(result.outputs)) {
  const element = document.createElement("graphrt-block");
  element.block = block;
  element.addEventListener("graphrt-action", (event) => resume(event.detail));
  page.append(element);
}
```

- **Kinds**: `adaptive_card` (inputs, `Action.Submit`, `Action.Execute`,
  `Action.OpenUrl`, `Action.ShowCard`, `Action.ToggleVisibility`), `chart`
  (bar, stacked, horizontal, line, area, pie, donut, as inline SVG), `table`,
  `stats` and `diagram`.
- **Validated first.** A block is checked by a port of `graphrt_ui::validate`
  before anything is drawn. Both implementations run against the same
  [fixtures](../crates/graphrt-ui/fixtures), with the same messages. A block
  that fails is not drawn: its `fallback_text` and the reason are shown.
- **Text stays text.** The page is built with DOM calls, never from markup, so
  nothing in a block is parsed as HTML. Images are `https` or `data:` raster
  images, links `https` or `mailto`, checked again when drawn.
- **No remote code.** No script, style or font is loaded from anywhere. The
  package has no dependencies.
- **Light and dark** follow the page (`prefers-color-scheme`), or the `theme`
  attribute. Colours are `--grt-*` custom properties you can set.
- **Diagrams** are shown as their Mermaid text until the page brings Mermaid
  itself: `setDiagramRenderer(mermaidRenderer(mermaid))`. `@graphrt-dev/graphrt/ui` never
  loads Mermaid on its own, and `@graphrt-dev/graphrt/client` carries none of this. Mermaid is then set
  to its strict security level without HTML labels, and the picture is shown
  as an image, never inserted as markup. The chat page of `graphrt serve` does
  this with the file of [`--chat-mermaid`](#diagrams-in-the-chat).
- **Charts** come with a legend for two or more series, the value of each mark
  as its tooltip, and the numbers as a table under the picture. The eight
  series colours are checked for colour vision deficiencies in both themes.
  From the ninth series on they repeat, so prefer at most eight.

A card action that sends data fires `graphrt-action` with `{type, id, verb,
data}`: `data` holds the card's input values by input id, then the action's own
`data`. For a `human-input` pause, resume with `{action: id, values: data}`.

## The npm package

```sh
npm install @graphrt-dev/graphrt
```

| Import | What |
| --- | --- |
| `@graphrt-dev/graphrt/client` | The typed client |
| `@graphrt-dev/graphrt/ui` | The web components that draw UI blocks |
| `@graphrt-dev/graphrt/chat` | The chat component and the widget |
| `@graphrt-dev/graphrt/monitor` | The [monitoring](monitoring.md) view |
| `@graphrt-dev/graphrt/console` | The [admin console](console.md) |

One package, ES modules with type declarations. Each import pulls only its own
part (and the parts it is built on: the chat uses the client and the UI blocks),
and nothing runs on import, so a bundler leaves out what a page does not use.
The version of `@graphrt-dev/graphrt` is the version of the graphrt release it was published
with.

The sources live in [`web/packages/graphrt`](../web/packages/graphrt) (an npm
workspace in [`web/`](../web)). `graphrt serve` embeds the chat page from
`crates/graphrt-cli/assets/chat`, which is built from them and committed, so
building graphrt needs no Node.js.

```sh
cargo build -p graphrt-cli        # the tests start a real graphrt serve
cd web && npm ci
npm run check                     # types, lint, tests, that the embedded pages are up to date and the package publishes cleanly
npm run build                     # after a change to the sources: rebuild the embedded pages
```

The release workflow publishes it with npm trusted publishing and a provenance
statement, see [web/README.md](../web/README.md#publishing).
