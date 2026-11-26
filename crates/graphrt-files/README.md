# graphrt-files

File workspaces for graphrt agents: a sandboxed store with quotas, content sniffing and the `files` tools (list, read, search, write).

Part of [graphrt](https://github.com/Graphrt/graphrt) — **run your agent everywhere**: one small runtime for
agentic AI workflows on servers, containers, the edge and phones.

```toml
[dependencies]
graphrt-files = "0.6"
```

```rust
use std::sync::Arc;
use graphrt_core::tool::ToolRegistry;
use graphrt_files::{FileTools, Workspace, WorkspaceLimits};

let workspace = Workspace::dir("./workspace", WorkspaceLimits::default())?;
let mut tools = ToolRegistry::new();
tools.add("files", Arc::new(FileTools::new(workspace))); // agent nodes use `files/*`
```

An agent can list, read (text and documents), search and write files inside the workspace and
nowhere else: no `..`, no absolute paths, no links, no programs. File types come from content,
sizes and counts are bounded, and every user can get a private directory.

- Documentation: <https://docs.rs/graphrt-files>
- Guide: [Files and images](https://github.com/Graphrt/graphrt/blob/main/docs/files-and-images.md)
- Design and threat model: [files-and-images-design.md](https://github.com/Graphrt/graphrt/blob/main/docs/files-and-images-design.md)
- Project README: <https://github.com/Graphrt/graphrt#readme>

Licensed under the Apache License, Version 2.0.
