//! Throughput of the file workspace, for the numbers in docs/files-and-images-design.md.
//!
//! ```sh
//! cargo run --release -p graphrt-files --example bench
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::cast_precision_loss, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Instant;

use graphrt_core::tool::{ToolCallContext, ToolRuntime};
use graphrt_files::{DirStore, FileStore, FileTools, Workspace, WorkspaceLimits, WorkspacePath, sniff};
use serde_json::json;

const MIB: usize = 1024 * 1024;

fn path(raw: &str) -> WorkspacePath {
    WorkspacePath::parse(raw).unwrap()
}

/// Median of `runs` timings of `work`, in seconds.
async fn median<F, Fut>(runs: usize, mut work: F) -> f64
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        work().await;
        samples.push(started.elapsed().as_secs_f64());
    }
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn row(what: &str, value: String) {
    println!("| {what} | {value} |");
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let dir = std::env::temp_dir().join(format!("graphrt-files-bench-{}", std::process::id()));
    let store = Arc::new(DirStore::open(&dir).unwrap());
    let limits =
        WorkspaceLimits { max_file_bytes: 256 * MIB as u64, max_total_bytes: 4096 * MIB as u64, ..Default::default() };
    let workspace = Workspace::new(Arc::clone(&store) as Arc<dyn FileStore>, limits);
    let tools = FileTools::new(workspace.clone());

    println!("| What | Result |\n| --- | --- |");

    // Pure CPU: path checks and content sniffing.
    let started = Instant::now();
    let rounds = 1_000_000;
    for index in 0..rounds {
        std::hint::black_box(
            WorkspacePath::parse(std::hint::black_box(if index % 2 == 0 {
                "reports/2026/q3/summary.final.csv"
            } else {
                "a/../../etc/passwd"
            }))
            .is_ok(),
        );
    }
    row("Check one path", format!("{:.0} ns", started.elapsed().as_nanos() as f64 / f64::from(rounds)));

    let text = "plain text with a few words on every line\n".repeat(200);
    let started = Instant::now();
    for _ in 0..rounds / 10 {
        std::hint::black_box(sniff(std::hint::black_box(text.as_bytes()), Some("md")));
    }
    row(
        "Detect the type of a file (8 KiB window)",
        format!("{:.2} µs", started.elapsed().as_micros() as f64 / f64::from(rounds / 10)),
    );

    // Storage: a 64 MiB file.
    let big = vec![0x5au8; 64 * MIB];
    let seconds = median(5, || async {
        workspace.write(&path("big.bin"), big.clone(), true).await.unwrap();
    })
    .await;
    row("Write 64 MiB (temporary file + rename)", format!("{:.0} MiB/s", 64.0 / seconds));
    let seconds = median(5, || async {
        std::hint::black_box(workspace.read_all(&path("big.bin"), u64::MAX).await.unwrap());
    })
    .await;
    row("Read 64 MiB", format!("{:.0} MiB/s", 64.0 / seconds));
    let seconds = median(5, || async {
        std::hint::black_box(workspace.sha256(&path("big.bin")).await.unwrap());
    })
    .await;
    row("SHA-256 of 64 MiB (streamed, 64 KiB buffer)", format!("{:.0} MiB/s", 64.0 / seconds));
    workspace.delete(&path("big.bin")).await.unwrap();

    // Tools: a small write and read, as an agent calls them.
    let seconds = median(200, || async {
        let args = json!({"path": "notes/a.md", "content": "# Title\n\nsome text\n", "overwrite": true});
        tools.call("write", args, ToolCallContext::empty()).await.unwrap();
    })
    .await;
    row("`files/write`, small file", format!("{:.0} µs", seconds * 1e6));
    let seconds = median(200, || async {
        tools.call("read", json!({"path": "notes/a.md"}), ToolCallContext::empty()).await.unwrap();
    })
    .await;
    row("`files/read`, small file", format!("{:.0} µs", seconds * 1e6));

    // A 32 MiB log: the last page, and a search over all of it.
    let line = "2026-11-26T10:00:00Z INFO request served in 12 ms path=/v1/runs status=200\n";
    let lines = 32 * MIB / line.len();
    let mut log = line.repeat(lines);
    log.push_str("2026-11-26T10:00:01Z ERROR needle-in-the-haystack\n");
    workspace.write(&path("logs/app.log"), log.into_bytes(), true).await.unwrap();
    let seconds = median(5, || async {
        let args = json!({"path": "logs/app.log", "offset": lines, "limit": 10});
        let out = tools.call("read", args, ToolCallContext::empty()).await.unwrap();
        assert!(out.text.contains("needle-in-the-haystack"));
    })
    .await;
    row(
        "`files/read`, last 10 lines of a 32 MiB file",
        format!("{:.0} ms ({:.0} MiB/s)", seconds * 1e3, 32.0 / seconds),
    );

    let limits =
        WorkspaceLimits { max_file_bytes: 256 * MIB as u64, max_total_bytes: 4096 * MIB as u64, ..Default::default() };
    let wide = graphrt_files::FileToolsConfig { max_search_file_bytes: 64 * MIB as u64, ..Default::default() };
    let search_tools = FileTools::with_config(Workspace::new(Arc::clone(&store) as Arc<dyn FileStore>, limits), wide);
    let seconds = median(5, || async {
        let out = search_tools
            .call("search", json!({"query": "needle-in-the-haystack"}), ToolCallContext::empty())
            .await
            .unwrap();
        assert!(out.text.contains("logs/app.log:"), "{}", out.text);
    })
    .await;
    row("`files/search`, 32 MiB of text", format!("{:.0} ms ({:.0} MiB/s)", seconds * 1e3, 32.0 / seconds));

    // The same log by pattern: an ordinary one, and one built to be as slow as a pattern can be.
    #[cfg(feature = "regex")]
    {
        let patterns = [
            ("`files/search` by regular expression, 32 MiB", r"ERROR \w+-in-the-\w+$"),
            ("The same with a worst case pattern", r"(.*e){8}needle"),
        ];
        for (what, pattern) in patterns {
            let seconds = median(3, || async {
                let args = json!({"query": pattern, "regex": true});
                let out = search_tools.call("search", args, ToolCallContext::empty()).await.unwrap();
                assert!(!out.is_error, "{}", out.text);
            })
            .await;
            row(what, format!("{:.0} ms ({:.0} MiB/s)", seconds * 1e3, 32.0 / seconds));
        }
        let started = Instant::now();
        let refused = search_tools
            .call("search", json!({"query": r"(\w{1,500}){500}", "regex": true}), ToolCallContext::empty())
            .await
            .unwrap();
        assert!(refused.is_error);
        row(
            "Refuse a pattern that would compile too large",
            format!("{:.1} ms", started.elapsed().as_secs_f64() * 1e3),
        );
    }

    // 2,000 small files: listing and searching a busy workspace.
    for index in 0..2_000 {
        let name = format!("many/d{}/f{index}.md", index % 20);
        workspace.write(&path(&name), format!("# note {index}\nsome text\n").into_bytes(), true).await.unwrap();
    }
    let seconds = median(10, || async {
        std::hint::black_box(workspace.list(Some(&path("many")), 5_000).await.unwrap());
    })
    .await;
    row("List 2,000 files in 20 directories", format!("{:.1} ms", seconds * 1e3));
    let seconds = median(5, || async {
        let args = json!({"query": "note 1999", "path": "many"});
        let out = tools.call("search", args, ToolCallContext::empty()).await.unwrap();
        assert!(out.text.contains("f1999.md"), "{}", out.text);
    })
    .await;
    row("`files/search` across 2,000 small files", format!("{:.0} ms", seconds * 1e3));

    let _ = std::fs::remove_dir_all(&dir);
}
