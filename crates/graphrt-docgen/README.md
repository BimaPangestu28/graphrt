# graphrt-docgen

Document creation for graphrt agents: PDF, Word (DOCX) and Excel (XLSX) files written from Markdown and rows, and the `documents` tools over a file workspace.

Part of [graphrt](https://github.com/Graphrt/graphrt) — **run your agent everywhere**: one small runtime for
agentic AI workflows on servers, containers, the edge and phones.

```toml
[dependencies]
graphrt-docgen = "0.6"
```

```rust
use std::sync::Arc;
use graphrt_core::tool::ToolRegistry;
use graphrt_docgen::DocumentTools;
use graphrt_files::{Workspace, WorkspaceLimits};

let workspace = Workspace::dir("./workspace", WorkspaceLimits::default())?;
let mut tools = ToolRegistry::new();
tools.add("documents", Arc::new(DocumentTools::new(workspace))); // agent nodes use `documents/*`
```

An agent writes a report as Markdown and gets a PDF or a Word document, or sends rows and gets a
workbook. The writers are in this crate and small: text, tables and lines, and nothing a reader
would run or fetch (no scripts, macros, links, embedded objects or formulas).

- Documentation: <https://docs.rs/graphrt-docgen>
- Guide: [Files and images](https://github.com/Graphrt/graphrt/blob/main/docs/files-and-images.md#creating-documents)
- Design and threat model: [files-and-images-design.md](https://github.com/Graphrt/graphrt/blob/main/docs/files-and-images-design.md)
- Project README: <https://github.com/Graphrt/graphrt#readme>
