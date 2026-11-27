- Knowledge connectors (cargo feature `connectors`, new crate
  `graphrt-connect`): the `sources:` of a knowledge config keep datasets in
  sync with web pages (one page, a bounded crawl or a sitemap, robots.txt
  honoured), REST / JSON APIs (pagination, JSONPath), Google Drive, Notion and
  SharePoint or OneDrive. Syncs are incremental (versions and content hashes,
  deleted items removed), run on a schedule (`every`, `daily`, `cron`) in
  `graphrt serve` and once with `graphrt kb sync`, and share a lock so two
  processes never sync the same source at once. `GET /v1/knowledge/sources`
  and `POST /v1/knowledge/sources/{id}/sync` show and start syncs. See
  docs/knowledge-connectors.md.
- `graphrt_core::cron` (feature `cron`): the gateway's cron expressions and
  time zones, shared with the sync schedules.
