# Browser package

What a web page needs to use an agent served by `graphrt serve`, published to
npm as one package, [`@graphrt-dev/graphrt`](packages/graphrt). The guide is
[docs/web.md](../docs/web.md).

| Import | Sources | What |
| --- | --- | --- |
| `@graphrt-dev/graphrt/client` | [`src/client`](packages/graphrt/src/client) | A typed client: blocking, streamed and background runs, events with replay, abort, resume, feedback, conversations |
| `@graphrt-dev/graphrt/ui` | [`src/ui`](packages/graphrt/src/ui) | Web components that draw UI blocks: Adaptive Cards, charts, tables, stat tiles, diagrams |
| `@graphrt-dev/graphrt/chat` | [`src/chat`](packages/graphrt/src/chat) | The chat component and the widget, built on the two above |
| `@graphrt-dev/graphrt/monitor` | [`src/monitor`](packages/graphrt/src/monitor) | The monitoring view: runs, run details, the approvals inbox, push notifications |
| `@graphrt-dev/graphrt/console` | [`src/console`](packages/graphrt/src/console) | The admin console of `graphrt serve --console` |

The parts import each other by relative path, so the built `dist/` keeps them
apart: importing `@graphrt-dev/graphrt/client` loads no web component and no Mermaid loader,
and nothing runs on import (`"sideEffects": false`). Tests live in
`packages/graphrt/test/<part>`.

`graphrt serve --chat` serves the chat page and the widget from
`crates/graphrt-cli/assets/chat`, and `graphrt serve --monitor` the monitoring
page from `crates/graphrt-cli/assets/monitor`, and `graphrt serve --console`
the console page from `crates/graphrt-console/assets/console`. `npm run build`
writes them from the sources (`src/chat/page.ts`, `src/chat/embed.ts`,
`src/monitor/page.ts`, `src/monitor/sw.ts`, `src/console/page.ts`; these page scripts are not part of the npm package). The
result is committed, so building graphrt needs no Node.js.

```sh
cargo build -p graphrt-cli --features console   # the tests start a real graphrt serve ($GRAPHRT_BIN to use another binary)
npm ci
npm run check                # types, lint, tests, that the embedded pages are up to date and the package publishes cleanly
npm run build                # after a change: rebuild the embedded pages, then commit them
npm run pack-package         # write .pack/graphrt-dev-graphrt-X.Y.Z.tgz, the file npm publish would upload
```

## Publishing

`packages/graphrt/package.json` is private and has no version, so it cannot be
published by accident. `scripts/publish.mjs` builds `dist/` and stages the
published copy in `.pack/node_modules/@graphrt-dev/graphrt`: public, with the version of the
graphrt release (`[workspace.package] version` of the root `Cargo.toml`), the
repository's `LICENSE`, and exports without the `source` condition. Then it
checks the copy as a user gets it: only `dist/`, `README.md`, `LICENSE` and
`package.json` are uploaded, every entry point loads in Node.js by its package
name, and esbuild leaves out every part a page does not import.

The `npm` job of [the release workflow](../.github/workflows/release.yml)
publishes `@graphrt-dev/graphrt` when a release tag is pushed and the repository variable
`PUBLISH_NPM` is `true`, refusing a tag that is not the version of `Cargo.toml`.
It uses no token: `release.yml` (environment `npm`) is the trusted publisher of
`@graphrt-dev/graphrt` on npmjs.com (OIDC), and npm attaches a provenance statement to every
version. A version that is on npm already is left alone.

The package is scoped because npm refused the unscoped name `graphrt` as too
close to `graphql`. The scope is the npm organization `graphrt-dev`.

npm configures a trusted publisher only for a package that exists, so the very
first version is published once by hand, by an owner of the npm organization
`graphrt-dev`:

```sh
web/scripts/first-publish.sh --dry-run vX.Y.Z   # build and check the tarball, publish nothing
web/scripts/first-publish.sh vX.Y.Z             # needs `npm login` first, builds from a clean clone of the tag
```

It refuses when `@graphrt-dev/graphrt` is on npm already, builds and checks the
package with the tag's own `scripts/publish.mjs`, installs the tarball in a
scratch project, imports every entry point by name in Node.js and esbuild, and
publishes that tarball. npm confirms the publish in the browser
(`--auth-type=web`).

Tags up to `v0.6.3` still call the package `graphrt`. `--rename` publishes such
a tag from its own source: after the tag's checks pass, it rewrites only `name`
in the staged `package.json` and the import paths in its `README.md`, then runs
the checks above under the new name:

```sh
web/scripts/first-publish.sh --rename v0.6.3
```

Then on npmjs.com, package `@graphrt-dev/graphrt`, Settings, Trusted publishing: GitHub
Actions, organization `Graphrt`, repository `graphrt`, workflow `release.yml`,
environment `npm`. Under Publishing access, choose "Require two-factor
authentication and disallow tokens". From then on set `PUBLISH_NPM` to `true`
and every release tag publishes.
