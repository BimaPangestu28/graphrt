- `graphrt serve --workspace-api key|users`: the agents' workspace over HTTP
  under `/v1/files` (list, info, upload, download, delete), through the same
  workspace as the `files` tools, so paths, quotas, type checks and user
  directories are shared. Off unless the flag is given. `users` also admits
  verified end users (`--verify-jwt`), each inside the directory of their own
  `sub` (`--workspace-per-user`). Uploads are raw request bodies limited by
  `--max-upload-bytes`, counted while they arrive. Downloads are streamed and
  always carry `nosniff` and a sandboxing `Content-Security-Policy`. HTML,
  SVG and programs are sent as `application/octet-stream` attachments.
  `?ref=` takes the `reference` of a file value, so a client can offer the
  files a run returned as downloads. See `docs/files-and-images.md`.
- Run requests take `files: ["workspace:..."]`: uploaded workspace files
  attached to the run. They become `sys.files` as file values, checked
  against what the caller may reach, for `vision` and `document-extractor`.
