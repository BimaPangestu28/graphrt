# graphrt-chat

A chat for agents served by `graphrt serve --chat`, as web components.

```html
<graphrt-chat server="https://agent.example.com" workflow="support"></graphrt-chat>
```

```ts
import { defineGraphrtChat } from "graphrt-chat";

defineGraphrtChat();
```

| Export | What |
| --- | --- |
| `<graphrt-chat>` | The chat, to place in a page |
| `<graphrt-chat-widget>` | A bubble in a corner that opens it |
| `ChatSession` | The logic without a view, for a chat of your own design |
| `loadDiagrams` | Draws `diagram` blocks with the Mermaid a server serves (`--chat-mermaid`) |

A server started with `--chat` serves both ready to use: the page `/chat/` and
the script `/chat/embed.js`.

See [docs/web.md](../../../docs/web.md).
