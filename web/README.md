# Browser packages

What a web page needs to use an agent served by `graphrt serve`. The guide is
[docs/web.md](../docs/web.md).

| Package | What |
| --- | --- |
| [`@graphrt/client`](packages/client) | A typed client: blocking, streamed and background runs, events with replay, abort, resume, feedback, conversations |
| [`@graphrt/ui`](packages/ui) | Web components that draw UI blocks: Adaptive Cards, charts, tables, stat tiles, diagrams |
| [`@graphrt/chat`](packages/chat) | The chat component and the widget, built on the two above |
| [`@graphrt/monitor`](packages/monitor) | The monitoring view: runs, run details, the approvals inbox, push notifications |

`graphrt serve --chat` serves the chat page and the widget from
`crates/graphrt-cli/assets/chat`, and `graphrt serve --monitor` the monitoring
page from `crates/graphrt-cli/assets/monitor`. `npm run build` writes both from
these packages. The result is committed, so building graphrt needs no Node.js.

```sh
cargo build -p graphrt-cli   # the tests start a real graphrt serve ($GRAPHRT_BIN to use another binary)
npm ci
npm run check                # types, lint, tests, that the embedded pages are up to date and the packages publish cleanly
npm run build                # after a change: rebuild the embedded pages, then commit them
```

## Publishing

The packages go to npm as `@graphrt/client`, `@graphrt/ui`, `@graphrt/chat` and
`@graphrt/monitor`, built to `dist/` (JavaScript and type declarations). Inside
this workspace they resolve to their sources through the `source` export
condition.

The `npm` job of [the release workflow](../.github/workflows/release.yml)
publishes them when a release tag is pushed and the repository variable
`PUBLISH_NPM` is `true`. It uses no token: `release.yml` is the trusted
publisher of each package on npmjs.com (OIDC), and npm attaches a provenance
statement to every version. A version that is on npm already is left alone, so
bump `version` in a package's `package.json` (and in the packages that depend
on it) to publish a change.

```sh
npm run check-packages       # build dist/ and check what npm publish would upload
```
