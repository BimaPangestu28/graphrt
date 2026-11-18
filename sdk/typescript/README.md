# graphrt-plugin (JavaScript / TypeScript)

Write graphrt node kinds and tools for Node.js 18+. No dependencies; ships ESM
with TypeScript declarations.

```js
import { Plugin, PluginError, pause } from "graphrt-plugin";

const plugin = new Plugin("acme")
  .node("acme.upper", async (ctx, req) => {
    const text = String(req.config.text ?? "");
    ctx.chunk("text", text.toUpperCase());
    return { outputs: { text: text.toUpperCase() } };
  })
  .node("acme.approve", async (ctx, req) => {
    if (!req.resume) return pause("approval", { message: "Publish?" }, { draft: req.config.text });
    return { outputs: { approved: req.resume.input === "yes" }, branch: req.resume.input === "yes" ? "approved" : "rejected" };
  })
  .tool({ name: "shout", description: "Uppercase a text" }, async (args) => String(args.text).toUpperCase());

plugin.run();
```

Pair it with a `graphrt-plugin.json` manifest; see `docs/plugins.md`.

- `req.config`: node config with variables resolved by the host.
- `req.resume`: `{state, input}` after a pause, else `null`.
- `ctx.chunk(field, text)`, `ctx.progress(name, data)`, `ctx.log(message)`.
- `ctx.signal` aborts when the host cancels the request.
- Never `console.log` (stdout carries the protocol); use `console.error`.
- Tools can return images for the model:
  `{ text, images: [{ media_type: "image/png", data: pngBuffer }] }` (or
  `data_base64`, or `{ url: "https://..." }`); at most 4, png/jpeg/webp/gif,
  5 MiB each. Hosts without image support get the text plus a note instead.

Tests: `npm test` (Node's built-in runner).
