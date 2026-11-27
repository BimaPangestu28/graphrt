# Browser packages

What a web page needs to use an agent served by `graphrt serve`. The guide is
[docs/web.md](../docs/web.md).

| Package | What |
| --- | --- |
| [`graphrt-client`](packages/client) | A typed client: blocking, streamed and background runs, events with replay, abort, resume, feedback, conversations |
| [`graphrt-ui`](packages/ui) | Web components that draw UI blocks: Adaptive Cards, charts, tables, stat tiles, diagrams |
| [`graphrt-chat`](packages/chat) | The chat component and the widget, built on the two above |

`graphrt serve --chat` serves the chat page and the widget from
`crates/graphrt-cli/assets/chat`, which `npm run build` writes from these
packages. The result is committed, so building graphrt needs no Node.js.

```sh
cargo build -p graphrt-cli   # the tests start a real graphrt serve ($GRAPHRT_BIN to use another binary)
npm ci
npm run check                # types, lint, tests, and that the embedded page is up to date
npm run build                # after a change: rebuild the embedded page, then commit it
```

The packages are TypeScript sources (`exports` points at `src/index.ts`) for
bundlers. They are not published to npm yet.
