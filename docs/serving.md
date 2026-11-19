# Serving workflows

`graphrt serve <file-or-dir>` exposes workflows over HTTP. This page covers
durable runs: starting runs in the background, following them, and resuming
paused runs by id — without the client keeping snapshots.

```sh
graphrt serve examples/ --store ./runs --store-ttl-days 30
```

## Run store

| Option | Effect |
| --- | --- |
| `--store <dir>` (or `$GRAPHRT_STORE`) | Durable directory store. Paused runs survive restarts. |
| *(neither)* | In-memory store. Runs are lost when the server stops. |
| `--store-ttl-days <n>` | Hourly cleanup of runs (any status except running) not updated for `n` days. |
| `--max-runs <n>` | In-memory store only: keep at most `n` runs (default 1000, `0` = unlimited). The oldest finished runs are dropped first; running and paused runs are always kept. |

### Small devices

A finished run with its events takes roughly 10–20 KiB in the in-memory store,
so memory grows with `--max-runs`, not with uptime. Measured with a static
musl build (`--no-default-features --features llm,http,wasm,serve`, 6.3 MiB)
on Linux arm64 under cgroup limits, serving a 3-node workflow for 37,000
requests:

| CPU | Memory limit | `--max-runs` | Steady RSS | Requests/s | p50 / p99 |
| --- | --- | --- | --- | --- | --- |
| 0.1 | 16 MiB | 100 | ~3 MiB | ~100 | 100 / 200 ms |
| 0.25 | 16 MiB | 100 | ~3.4 MiB | ~600 | 4 / 89 ms |
| 1.0 | 64 MiB | 1000 (default build) | ~21 MiB | ~4,600 | 2 / 4 ms |

At 0.1 CPU the latency is the kernel's CPU quota (10 ms per 100 ms period),
not graphrt. On devices with little memory, lower `--max-runs` (or use
`--store` on flash storage) and build only the features you need.

Directory layout:

```text
runs/
  .lock                          OS lock held around every store operation
  runs/<run_id>/record.json      status, outputs, answer, pause reasons, usage
  runs/<run_id>/events.jsonl     one event per line (capped at 5 000 per run)
  runs/<run_id>/snapshot.json    snapshot of a paused run
```

Records and snapshots are replaced atomically (temporary file, fsync, rename,
directory fsync). The lock is an OS file lock, so several server processes on
one machine may share a directory; it is not meant for network file systems.
Listing reads every record, which suits thousands of runs, not millions.

Nothing secret is written: event values are serialized with secrets masked, and
snapshots never contain secret plaintext. Secrets other than `env.*` must be
passed again on resume (`secrets` in the resume body).

### Restarts

On startup the server marks runs that were `running` as `failed` with the error
`interrupted by restart` (a run's in-flight work cannot be recovered). Paused
runs stay paused and can be resumed.

## Starting runs

`POST /v1/workflows/{name}/runs`

| Body field | Meaning |
| --- | --- |
| `inputs`, `sys`, `conversation` | Run input |
| `stream: true` | Respond with server-sent events |
| `async: true` | Respond `202 {run_id, run, events}` immediately; the run continues in the background |
| *(neither)* | Wait and respond with the final JSON result |

For synchronous calls (blocking or streaming), the run is aborted when the
client disconnects before it finishes. Async runs are never tied to a
connection.

## Following runs

| Request | Response |
| --- | --- |
| `GET /v1/runs?workflow=&status=&limit=&cursor=` | `{runs, next_cursor}`, newest first (max 500 per page) |
| `GET /v1/runs/{id}` | The run record, plus `active` and, for paused runs, `resume_inputs` |
| `GET /v1/runs/{id}/events` | SSE replay of stored events, then a live tail while the run executes |
| `GET /v1/runs/{id}/events?stream=false&from=N` | Stored events as a JSON array, starting at index `N` |
| `POST /v1/runs/{id}/abort` | `202` when the run is executing |
| `DELETE /v1/runs/{id}` | `204`; `409` while the run executes |

`resume_inputs` lists what each paused node waits for:

```json
{
  "status": "paused",
  "resume_inputs": [
    { "key": "review", "node_id": "review", "kind": "human_input",
      "data": { "prompt": "Approve an expense of 50 for: Lunch?", "fields": [], "actions": [] } }
  ]
}
```

`key` is what to use in the resume `inputs`: the node id, or `<frame>/<node_id>`
for a node paused inside an iteration or loop.

A listener that falls behind the live tail receives a `lagged` event with the
number of skipped events; the complete history stays available from the store.

## Resuming

`POST /v1/runs/{id}/resume`

```json
{ "inputs": { "review": { "action": "approve", "values": { "note": "ok" } } } }
```

The server loads the snapshot it stored when the run paused. Exactly one resume
of a pause succeeds: a concurrent or repeated resume gets `409 Conflict`.
`stream` and `async` work as for new runs. Clients that keep snapshots
themselves may still send `snapshot` in the body.

## Knowledge base API

When the server is started with `--knowledge <config>`, the knowledge base the
workflows' `knowledge-retrieval` nodes use is also exposed under
`/v1/knowledge`, behind the same API key. Documents added here are searchable by
running workflows immediately. It is meant as the backend of a knowledge base
UI (datasets, documents, chunks, retrieval testing).

| Endpoint | Purpose |
| --- | --- |
| `GET /v1/knowledge/datasets` | `{datasets: [{id, name, description, managed, chunking}]}`. `managed` is false for datasets served by an external retriever (read-only); `chunking` is the dataset's `{max_chars, overlap, markdown, parent_max_chars}`. |
| `GET /v1/knowledge/datasets/{dataset}/documents?limit=&after=` | Documents ordered by id: `{documents: [{document_id, title, chunks, characters, enabled, metadata}], next}`. Pass `next` as `after` for the following page; `next` is null at the end. `limit` defaults to 50, at most 500. |
| `POST /v1/knowledge/datasets/{dataset}/documents` | Add or replace a document from text: `{document_id, text, title?, metadata?}`. |
| `PUT /v1/knowledge/datasets/{dataset}/documents/{document}/file?name=&title=` | Add or replace a document from a raw file body (PDF, Office, HTML, Markdown, EPUB, CSV, e-mail, ...). `name` (e.g. `report.pdf`) and `Content-Type` select the format. Limited by `--max-upload-bytes` (default 16 MiB). |
| `GET /v1/knowledge/datasets/{dataset}/documents/{document}` | The document's chunks in order: `{document_id, chunks: [{id, title, text, characters, metadata}]}` (vectors are never sent). |
| `PATCH /v1/knowledge/datasets/{dataset}/documents/{document}` | `{enabled: false}` disables a document: it is kept (and stays disabled when re-uploaded) but no search returns it; `{enabled: true}` restores it. Listings show `enabled`. |
| `DELETE /v1/knowledge/datasets/{dataset}/documents/{document}` | `{deleted_chunks}`; 404 when the document does not exist. |
| `POST /v1/knowledge/retrieve` | Retrieval testing: `{query, datasets, top_k?, score_threshold?, filter?, weights?, rerank?}` returns `{hits, elapsed_ms}`, exactly as the node would retrieve. |

Chunking is set per dataset in the knowledge config. With `parent_max_chars`
the dataset uses parent-child retrieval: documents are split into parents of
that size and each parent into small children (`max_chars`, no overlap); the
children are embedded and searched, and a hit returns its whole parent once,
however many of its children matched. Parents are rebuilt from their children,
so nothing extra is stored.

```yaml
datasets:
  - { id: handbook, store: local, embedder: local,
      chunking: { max_chars: 400, parent_max_chars: 1600 } }
```

Adding a document with an existing id replaces it: unchanged chunks keep their
vectors (nothing is re-embedded), changed ones are embedded, and chunks the new
version no longer contains are removed. The response reports
`{document_id, report: {stored, skipped, removed}}`.

To stay light on small devices, ingestion (text extraction and embedding) runs
one request at a time (others wait their turn), and file extraction runs off
the request workers. Errors are JSON `{error}` with a matching status (400
invalid input, 404 unknown dataset or document, 409 operation not supported by
the dataset, 413 upload too large, 502 provider failure) and an
`x-graphrt-error-kind` header.

## Tenancy

The API is single-tenant: anyone holding the server's API key can see and
resume every run. Run one server (or one store directory) per tenant, or put an
authenticating proxy in front that restricts which run ids a caller may use.
