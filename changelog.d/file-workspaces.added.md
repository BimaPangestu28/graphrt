- File workspaces for agents: the new `graphrt-files` crate and
  `--workspace DIR` on `run`, `serve` and `eval`. Agents get the `files`
  tools (`files/list`, `read`, `search`, `write`, `info`, `delete`) inside one
  directory and nowhere else. `read` returns a window of lines of text files
  and of the extracted text of PDF, Office, EPUB and e-mail files; `search`
  finds lines containing a text. Paths with `..`, absolute paths, links,
  hidden names and Windows device names are refused, programs are not stored,
  file types come from content, and sizes and counts are bounded (32 MiB per
  file, 512 MiB and 5,000 files per workspace). `--workspace-read-only`
  removes the write tools, and `--workspace-per-user` gives every user a
  private directory chosen by `tool_context: { files: { scope: ... } }`,
  which the model cannot see or change. Storage is behind the `FileStore`
  trait (`DirStore`, `MemoryStore`). Off unless the flag is given; the slim
  agent builds do not include it. See `docs/files-and-images.md` and the
  design and threat model in `docs/files-and-images-design.md`.
- `agent` nodes have a `files` output and `tool` nodes fill theirs: the files
  a native tool returned in its structured result
  (`graphrt_core::tool::collect_files`), for example what `files/write`
  created. Files are never sent to the model, and JSON from plugins, MCP
  servers and models still cannot create file values. Behind the `tool-files`
  feature of `graphrt-core` and `graphrt-llm` (on by default, enabled by the
  CLI's `files` feature): the slim agent builds have no native file tools,
  leave it out and stay exactly as large as before.
