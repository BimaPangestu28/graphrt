# Knowledge connectors: keep a knowledge base in sync

A knowledge base does not have to be filled by uploading files. The `sources:`
list of a knowledge config names the places a team already keeps its
knowledge, and Graphrt keeps the datasets in sync with them:

| Type | Reads | Credential |
| --- | --- | --- |
| `web` | One page, a crawl bounded by depth, page count and domain, or a sitemap.xml | none (public pages) |
| `api` | A REST / JSON API, page by page, with JSONPath to the records and fields | none, bearer token, API key header, basic or OAuth |
| `google_drive` | A folder (and sub folders) or chosen files. Google Docs as Markdown, Sheets as CSV, Slides as text | OAuth, scope `drive.readonly` |
| `notion` | Chosen pages, the rows of chosen databases, or every page shared with the integration | integration token or OAuth |
| `sharepoint` | A SharePoint document library (or a folder in it), a drive by id, or the user's OneDrive | OAuth, scopes `Files.Read.All Sites.Read.All` |

Each sync is incremental. An item listed with the same version as last time
(ETag, revision, modified time, sitemap `lastmod`) is not downloaded again.
Without a version, the content hash decides. Changed items are indexed again
through the dataset's normal pipeline (graphrt-doc extraction, the dataset's
chunking, the embedder), items deleted at the source are removed, and an item
that cannot be read keeps its previous passages.

`graphrt serve` syncs every source on its schedule in the background (runs
never wait for a sync), `graphrt kb sync` syncs them once.

## A first source in two minutes

```yaml
# knowledge.yaml
embedders:
  openai: { type: openai, model: text-embedding-3-small, api_key_env: OPENAI_API_KEY }
stores:
  local: { type: sqlite, path: ./kb.db }
datasets:
  - { id: company, name: Company, store: local, embedder: openai }
sources:
  - id: site
    name: Company web site
    dataset: company
    type: web
    mode: crawl
    urls: ["https://example.com/"]
    max_depth: 2
    max_pages: 200
    schedule: { every: 6h }
```

```sh
graphrt kb --knowledge knowledge.yaml sync
# site: 37 indexed, 0 unchanged, 0 removed, 0 failed (37 documents, 21402 ms)

graphrt serve flows/ --knowledge knowledge.yaml
#   1 knowledge source(s) kept in sync (GET /v1/knowledge/sources)
```

A `knowledge-retrieval` node with `dataset_ids: [company]` now answers from the
web site, and keeps answering from the current version of it.

## Schedules

```yaml
schedule: manual                                  # only on request (the default)
schedule: { every: 6h }                           # 30m, 6h, 1d: at least 5 minutes
schedule: { daily: "06:00", timezone: Asia/Jakarta }
schedule: { cron: "0 */4 * * 1-5", timezone: UTC }
```

A source that never synced syncs when the server starts. A sync missed while
the server was down runs once at the next start, not once per missed slot.
Time zones are `UTC`, fixed offsets (`+07:00`) or IANA names read from the
system zoneinfo (the container image ships it).

## Sources

Every source has `id` (letters, digits, `-`, `_`), optional `name`, the
`dataset` it fills, `type`, `schedule` and `auth`. The other fields are the
connector's own.

### Web pages

```yaml
- id: docs
  dataset: company
  type: web
  urls: ["https://example.com/docs/"]  # start pages (sitemap addresses in sitemap mode)
  mode: crawl                          # page (default), crawl or sitemap
  max_depth: 2                         # crawl: links followed from a start page (at most 5)
  max_pages: 100                       # at most 2000
  same_domain: true                    # only the hosts of the start pages
  include: ["https://example.com/docs/"]   # only addresses starting with one of these
  exclude: ["https://example.com/docs/old/"]
  respect_robots: true
```

robots.txt is honoured for the `graphrt` user agent (its own group, else `*`).
A crawl follows pages and documents only (no videos, images, scripts or
styles), fetches four pages at a time, and every page is limited in size
(25 MB) and time. In `sitemap` mode the `lastmod` of every page is its
version, so unchanged pages are not downloaded at all.

### REST / JSON APIs

```yaml
- id: articles
  dataset: company
  type: api
  url: https://api.example.com/v2/articles
  method: GET                      # or POST with `body`
  query: { lang: en }
  headers: { Accept: application/json }   # no credentials here: see `auth`
  items_path: $.data[*]            # the records in a response (default: the response)
  id_path: $.id                    # default `id`
  title_path: $.title
  text_paths: [$.title, $.summary, $.body]   # default: the whole record as JSON
  updated_path: $.updated_at       # the record's version
  url_path: $.links.html
  pagination: { type: cursor, param: cursor, cursor_path: $.meta.next_cursor }
  max_pages: 50
  auth: { type: bearer, token_env: ARTICLES_TOKEN }
```

Pagination types: `none`, `page` (`param`, `start`, `size_param`, `size`),
`offset` (`param`, `limit_param`, `limit`), `cursor` (`param`, `cursor_path`),
`next_url` (`next_path`) and `link_header` (RFC 8288, as GitHub sends). JSONPath
supports `$`, `.name`, `['name']`, `[0]`, `[-1]`, `[*]` and `..name`.

### Google Drive

```yaml
- id: drive
  dataset: company
  type: google_drive
  folder_id: 1AbCdEf...            # from the folder's address; `root` is My Drive
  # file_ids: [1XyZ...]           # or single files
  recursive: true
  auth:
    type: oauth
    token_url: https://oauth2.googleapis.com/token
    client_id: 1234-abc.apps.googleusercontent.com
    client_secret_env: GOOGLE_CLIENT_SECRET     # web and desktop clients have one
    refresh_token_env: GOOGLE_DRIVE_REFRESH_TOKEN
```

Create an OAuth client in the Google Cloud console, enable the Drive API and
sign in once with the scope `https://www.googleapis.com/auth/drive.readonly`
to get the refresh token. Shared drives are included.

### Notion

```yaml
- id: wiki
  dataset: company
  type: notion
  page_ids: ["https://www.notion.so/acme/Handbook-1429989fe8ac4effbc8f57f56486db54"]
  database_ids: []                 # rows become documents, their properties included
  auth: { type: bearer, token_env: NOTION_TOKEN }
```

With neither `page_ids` nor `database_ids`, every page shared with the
integration is indexed. An internal integration (Settings, Connections,
Develop or manage integrations) gives a token that starts with `ntn_` or
`secret_`. Share the pages with it from the page menu (Connections).

### SharePoint and OneDrive

```yaml
- id: policies
  dataset: company
  type: sharepoint
  site_url: https://contoso.sharepoint.com/sites/HR
  folder_path: Policies            # optional: a folder of the library
  # drive_id: b!abc...             # or a library by id
  # onedrive: true                 # or the signed in user's OneDrive
  auth:
    type: oauth
    token_url: https://login.microsoftonline.com/organizations/oauth2/v2.0/token
    client_id: 00000000-0000-0000-0000-000000000000
    refresh_token_env: MS_REFRESH_TOKEN
```

Register an application in Microsoft Entra ID with the delegated permissions
`Files.Read.All` and `Sites.Read.All` (and `offline_access`), and sign in once
to get the refresh token. `cTag` changes only when a file's content changes,
so renamed or re-shared files are not downloaded again.

## Credentials

Configs never hold a secret. They name one (`token_env`, `value_env`,
`password_env`, `client_secret_env`, `refresh_token_env`), and the name is
resolved from the environment and `--env-file`, like `api_key_env` of the
embedders. Credentials are sent only to the source's own service, never
written to the sync state, and never shown in errors or logs.

```yaml
auth: { type: bearer, token_env: NAME }
auth: { type: header, name: X-Api-Key, value_env: NAME }
auth: { type: basic, username: reader, password_env: NAME }
auth: { type: oauth, token_url: ..., client_id: ..., client_secret_env: NAME, refresh_token_env: NAME }
```

OAuth access tokens are refreshed before they expire and once after a 401.
The desktop app keeps refresh tokens in the OS keychain and stores the new one
when a service rotates it.

## Safety limits

- Every request goes through the egress policy of the `http-request` node:
  loopback, private, link-local and cloud metadata addresses are refused
  (checked after DNS resolution, so DNS rebinding cannot reach them). A `web`
  or `api` source on an intranet sets `allow_private: true` explicitly.
- Downloads are limited to 25 MB per item and 8 MB per JSON page, requests to
  60 seconds. Rate limits (429) and server errors are retried three times with
  backoff, honouring `Retry-After`.
- A listing cut short (a page limit, an unreadable sub folder) never removes
  anything, and a failed listing changes nothing in the index.

## Sync state and running in two places

The version and content hash of every synced item, the last status of every
source and a sync lock live in one SQLite file, next to the config by default
(`knowledge.yaml` -> `knowledge.sync.sqlite`) or at `sync_state:`. Two
processes that use the same file (the desktop app and `graphrt serve` on the
same knowledge base) never sync the same source at the same time: the second
one sees the lock, skips, and finds the source up to date afterwards. A lock
of a process that died expires after ten minutes.

## HTTP API

Under the API key, next to the rest of `/v1/knowledge`:

| Request | Answer |
| --- | --- |
| `GET /v1/knowledge/sources` | Every source: type, dataset, schedule, `next_sync_at`, `syncing`, and the last `status` (`outcome`, `report`, `document_count`, `error`, `warnings`, `duration_ms`) |
| `POST /v1/knowledge/sources/{id}/sync` | `202`: the sync started. `409` while it already runs |
| `POST /v1/knowledge/sources/{id}/sync?wait=true` | `200` with the outcome |

Errors have a stable kind (`auth`, `forbidden`, `not_found`, `denied`,
`rate_limited`, `too_large`, `network`, `config`, ...) in `status.error.kind`
and the `x-graphrt-error-kind` header, so a UI can explain them in its own
words.

## More connectors

Connectors are small: list the items of a source with a version, fetch one
item. Everything else (change detection, deletion, locking, schedules) is
shared. The `graphrt-connect` crate builds them from a registry, so a host or
another crate adds a type with `ConnectorRegistry::register` without changing
Graphrt:

```rust
let mut registry = graphrt_connect::ConnectorRegistry::with_builtins();
registry.register("confluence", |source, ctx| {
    let http = ctx.http(source, false, "Confluence")?;
    Ok(Box::new(MyConfluence::new(source, http)?))
});
```

## Build size

The cargo feature is `connectors` of `graphrt-cli` (each connector is a
feature of `graphrt-connect`: `web`, `api`, `google-drive`, `notion`,
`sharepoint`). It is in the default build and the released binaries. The slim
agent builds (`serve,llm,plugins`) have none of this code. No SDK is linked:
every connector uses the HTTP client the runtime already has.
