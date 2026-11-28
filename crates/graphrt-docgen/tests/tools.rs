//! The `documents` tools end to end: what they write is read back by the
//! extractor agents use on uploads, and nothing a model sends can leave the
//! workspace, pass its limits, or put something active into a document.
#![cfg(all(feature = "tools", feature = "office"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::Read as _;

use graphrt_core::tool::{ToolCallContext, ToolOutput, ToolRuntime};
use graphrt_docgen::{DocumentTools, DocumentToolsConfig};
use graphrt_files::{Workspace, WorkspaceLimits, WorkspacePath};
use serde_json::{Value as Json, json};

const REPORT: &str = "# Sales report\n\nRevenue grew **12%** in *Jakarta*.\n\n| Region | Revenue |\n|---|---|\n| Jakarta | 120 |\n| Medan | 41 |\n\n- keep going\n- share it\n";

fn path(raw: &str) -> WorkspacePath {
    WorkspacePath::parse(raw).unwrap()
}

fn scope(name: &str) -> ToolCallContext {
    ToolCallContext { secrets: [("scope".to_owned(), name.to_owned())].into(), ..ToolCallContext::default() }
}

async fn call(tools: &DocumentTools, name: &str, arguments: Json) -> ToolOutput {
    tools.call(name, arguments, ToolCallContext::empty()).await.unwrap()
}

async fn read(workspace: &Workspace, raw: &str) -> Vec<u8> {
    workspace.read_all(&path(raw), 64 * 1024 * 1024).await.unwrap()
}

fn text_of(bytes: &[u8], name: &str) -> String {
    graphrt_doc::extract(bytes, None, Some(name), &graphrt_doc::Options::default()).unwrap()
}

/// Names and contents of the parts of an Office file.
fn parts(bytes: &[u8]) -> Vec<(String, String)> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    (0..archive.len())
        .map(|index| {
            let mut part = archive.by_index(index).unwrap();
            let mut content = String::new();
            part.read_to_string(&mut content).unwrap();
            let name = part.name().unwrap().into_owned();
            (name, content)
        })
        .collect()
}

#[tokio::test]
async fn documents_are_created_and_can_be_read_back() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let tools = DocumentTools::new(workspace.clone());
    let names: Vec<String> = tools.list().await.unwrap().into_iter().map(|spec| spec.name).collect();
    assert_eq!(names, ["create_pdf", "create_docx", "create_xlsx"]);

    let pdf = call(&tools, "create_pdf", json!({ "path": "out/report.pdf", "content": REPORT, "title": "Q3" })).await;
    assert!(!pdf.is_error, "{}", pdf.text);
    assert!(pdf.text.starts_with("Created out/report.pdf (application/pdf, 1 page, "), "{}", pdf.text);
    let text = text_of(&read(&workspace, "out/report.pdf").await, "report.pdf");
    for expected in ["Sales report", "Revenue grew 12% in Jakarta.", "Medan", "41", "share it"] {
        assert!(text.contains(expected), "`{expected}` missing from: {text}");
    }
    // The file comes back as a file value, for the `files` output of the node.
    let files = graphrt_core::tool::collect_files(&pdf.data);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].reference.as_deref(), Some("workspace:out/report.pdf"));
    assert_eq!(files[0].mime_type.as_deref(), Some("application/pdf"));

    let docx = call(&tools, "create_docx", json!({ "path": "out/report.docx", "content": REPORT })).await;
    assert!(!docx.is_error, "{}", docx.text);
    assert!(docx.text.contains("wordprocessingml.document"), "{}", docx.text);
    let text = text_of(&read(&workspace, "out/report.docx").await, "report.docx");
    for expected in ["Sales report", "Revenue grew 12% in Jakarta.", "Medan", "share it"] {
        assert!(text.contains(expected), "`{expected}` missing from: {text}");
    }

    let sheets = json!([
        { "name": "Orders", "rows": [["Region", "Revenue", "Open"], ["Jakarta", 120.5, true], ["Medan", 41, null]] },
        { "rows": [["only"]], "header": false }
    ]);
    let xlsx = call(&tools, "create_xlsx", json!({ "path": "out/orders.xlsx", "sheets": sheets })).await;
    assert!(!xlsx.is_error, "{}", xlsx.text);
    assert!(xlsx.text.contains("spreadsheetml.sheet, 2 sheets, 4 rows"), "{}", xlsx.text);
    let text = text_of(&read(&workspace, "out/orders.xlsx").await, "orders.xlsx");
    for expected in ["Orders", "Jakarta", "120.5", "Medan", "41", "Sheet2", "only"] {
        assert!(text.contains(expected), "`{expected}` missing from: {text}");
    }
}

#[tokio::test]
async fn spreadsheet_text_is_never_a_formula() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let tools = DocumentTools::new(workspace.clone());
    // What a model copies out of an e-mail or a web page into a cell.
    let hostile = [
        "=cmd|' /C calc'!A0",
        "+1+1",
        "-2+3",
        "@SUM(A1:A9)",
        "=HYPERLINK(\"http://evil.example\",\"click\")",
        "\t=1+1",
    ];
    let rows: Vec<Json> = hostile.iter().map(|cell| json!([cell])).collect();
    let made =
        call(&tools, "create_xlsx", json!({ "path": "x.xlsx", "sheets": [{ "rows": rows, "header": false }] })).await;
    assert!(!made.is_error, "{}", made.text);
    let bytes = read(&workspace, "x.xlsx").await;
    let all = parts(&bytes);
    let sheet = &all.iter().find(|(name, _)| name == "xl/worksheets/sheet1.xml").unwrap().1;
    assert_eq!(sheet.matches("t=\"inlineStr\"").count(), hostile.len());
    for (_, content) in &all {
        assert!(!content.contains("<f>") && !content.contains("<f "), "a formula was written");
    }
    // A reader shows the text exactly as it was given.
    let text = text_of(&bytes, "x.xlsx");
    assert!(text.contains("=cmd|' /C calc'!A0") && text.contains("@SUM(A1:A9)"), "{text}");
}

#[tokio::test]
async fn text_cannot_inject_markup_into_a_document() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let tools = DocumentTools::new(workspace.clone());
    let hostile = "</w:t></w:r><w:hyperlink r:id=\"rId9\"><w:r><w:t>click</w:t></w:r></w:hyperlink> & <script>x</script> \u{0}\u{1b}";
    let content = format!("# {hostile}\n\n{hostile}\n\n| {hostile} |\n\n- {hostile}\n");
    let docx = call(&tools, "create_docx", json!({ "path": "x.docx", "content": content, "title": hostile })).await;
    assert!(!docx.is_error, "{}", docx.text);
    let bytes = read(&workspace, "x.docx").await;
    for (name, content) in parts(&bytes) {
        assert!(!content.contains("<w:hyperlink") && !content.contains("<script"), "{name}");
        assert!(!content.contains('\u{0}') && !content.contains('\u{1b}'), "{name}");
    }
    assert!(text_of(&bytes, "x.docx").contains("<w:hyperlink r:id=\"rId9\">"));

    let xlsx =
        json!({ "path": "x.xlsx", "sheets": [{ "name": "a\"/><evil", "rows": [["</t></is></c><c><f>1+1</f>"]] }] });
    assert!(!call(&tools, "create_xlsx", xlsx).await.is_error);
    for (name, content) in parts(&read(&workspace, "x.xlsx").await) {
        assert!(!content.contains("<f>") && !content.contains("<evil"), "{name}: {content}");
    }

    // In a PDF, text that looks like operators stays text.
    let pdf_text = "Total) Tj ET /JS (app.alert(1)) \\ ( end";
    let pdf = call(&tools, "create_pdf", json!({ "path": "x.pdf", "content": pdf_text, "title": pdf_text })).await;
    assert!(!pdf.is_error, "{}", pdf.text);
    let bytes = read(&workspace, "x.pdf").await;
    assert!(text_of(&bytes, "x.pdf").contains("Total) Tj ET /JS (app.alert(1)) \\ ( end"));
}

#[tokio::test]
async fn documents_hold_nothing_a_reader_would_run_or_fetch() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let tools = DocumentTools::new(workspace.clone());
    // Links, images and HTML in the Markdown are written as text, not as objects.
    let content = "# Title\n\n[click](https://evil.example/x) ![img](https://evil.example/p.png) <a href=\"javascript:x\">a</a>\n\n<script>alert(1)</script>\n";
    assert!(!call(&tools, "create_pdf", json!({ "path": "a.pdf", "content": content })).await.is_error);
    assert!(!call(&tools, "create_docx", json!({ "path": "a.docx", "content": content })).await.is_error);
    assert!(
        !call(&tools, "create_xlsx", json!({ "path": "a.xlsx", "sheets": [{ "rows": [[content]] }] })).await.is_error
    );

    let pdf = read(&workspace, "a.pdf").await;
    let raw = String::from_utf8_lossy(&pdf);
    for active in [
        "/JavaScript",
        "/JS",
        "/OpenAction",
        "/AA",
        "/URI",
        "/Launch",
        "/EmbeddedFile",
        "/AcroForm",
        "/GoToR",
        "/RichMedia",
    ] {
        assert!(!raw.contains(active), "the PDF holds {active}");
    }
    let docx_parts: Vec<String> = parts(&read(&workspace, "a.docx").await).into_iter().map(|(name, _)| name).collect();
    assert_eq!(
        docx_parts,
        [
            "[Content_Types].xml",
            "_rels/.rels",
            "docProps/core.xml",
            "word/_rels/document.xml.rels",
            "word/styles.xml",
            "word/document.xml"
        ]
    );
    let xlsx_parts: Vec<String> = parts(&read(&workspace, "a.xlsx").await).into_iter().map(|(name, _)| name).collect();
    assert_eq!(
        xlsx_parts,
        [
            "[Content_Types].xml",
            "_rels/.rels",
            "xl/workbook.xml",
            "xl/_rels/workbook.xml.rels",
            "xl/styles.xml",
            "xl/worksheets/sheet1.xml"
        ]
    );
    for file in ["a.docx", "a.xlsx"] {
        for (name, content) in parts(&read(&workspace, file).await) {
            for active in [
                "TargetMode=\"External\"",
                "vbaProject",
                "oleObject",
                "externalLink",
                "<w:fldSimple",
                "<w:instrText",
                "DDEAUTO",
            ] {
                assert!(!content.contains(active), "{file} {name} holds {active}");
            }
        }
    }
}

#[tokio::test]
async fn a_path_cannot_leave_the_workspace_and_the_name_must_say_what_the_file_is() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let tools = DocumentTools::new(workspace.clone());
    let escapes = [
        "../report.pdf",
        "/tmp/report.pdf",
        "a/../../report.pdf",
        "C:\\report.pdf",
        ".hidden.pdf",
        "con.pdf",
        "a\u{202e}fdp.pdf",
    ];
    for escape in escapes {
        let refused = call(&tools, "create_pdf", json!({ "path": escape, "content": "x" })).await;
        assert!(refused.is_error && refused.text.contains("invalid path"), "{escape}: {}", refused.text);
    }
    for (tool, wrong) in [
        ("create_pdf", "report.exe"),
        ("create_pdf", "report.pdf.html"),
        ("create_pdf", "report"),
        ("create_docx", "report.pdf"),
    ] {
        let refused = call(&tools, tool, json!({ "path": wrong, "content": "x" })).await;
        assert!(refused.is_error && refused.text.contains("must end in"), "{wrong}: {}", refused.text);
    }
    let refused = call(&tools, "create_xlsx", json!({ "path": "a.xls", "sheets": [{ "rows": [[1]] }] })).await;
    assert!(refused.is_error && refused.text.contains("must end in `.xlsx`"), "{}", refused.text);
    // Arguments the tools do not know are refused, not ignored.
    let refused = call(&tools, "create_pdf", json!({ "path": "a.pdf", "content": "x", "scope": "other-user" })).await;
    assert!(refused.is_error && refused.text.contains("invalid arguments"), "{}", refused.text);
    assert!(workspace.list(None, 10).await.unwrap().entries.is_empty());
    assert!(tools.call("create_pptx", json!({}), ToolCallContext::empty()).await.is_err());
}

#[tokio::test]
async fn limits_come_back_as_errors_the_model_can_act_on() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let config = DocumentToolsConfig {
        max_content_chars: 2_000,
        max_pdf_pages: 2,
        max_sheets: 2,
        max_cells: 6,
        ..Default::default()
    };
    let tools = DocumentTools::new(workspace.clone()).with_config(config);
    let cases = [
        ("create_pdf", json!({ "path": "a.pdf", "content": "x".repeat(2_001) }), "above the limit of 2000"),
        ("create_docx", json!({ "path": "a.docx", "content": "   \n" }), "`content` is empty"),
        ("create_pdf", json!({ "path": "a.pdf", "content": "line\n\n".repeat(250) }), "more pages than the limit of 2"),
        (
            "create_pdf",
            json!({ "path": "a.pdf", "content": format!("|{}", "c|".repeat(13)) }),
            "more table columns than the limit of 12",
        ),
        ("create_xlsx", json!({ "path": "a.xlsx", "sheets": [] }), "at least one sheet"),
        (
            "create_xlsx",
            json!({ "path": "a.xlsx", "sheets": [{ "rows": [] }, { "rows": [] }, { "rows": [] }] }),
            "more sheets than the limit of 2",
        ),
        (
            "create_xlsx",
            json!({ "path": "a.xlsx", "sheets": [{ "rows": [[1, 2, 3, 4], [5, 6, 7]] }] }),
            "more cells than the limit of 6",
        ),
    ];
    for (tool, arguments, expected) in cases {
        let refused = call(&tools, tool, arguments).await;
        assert!(refused.is_error && refused.text.contains(expected), "{tool}: {}", refused.text);
    }
    assert!(workspace.list(None, 10).await.unwrap().entries.is_empty());
}

#[tokio::test]
async fn the_rules_of_the_workspace_hold_for_documents() {
    // The quota: a document that does not fit is not stored.
    let small = WorkspaceLimits { max_total_bytes: 1_500, ..WorkspaceLimits::default() };
    let workspace = Workspace::in_memory(small);
    let tools = DocumentTools::new(workspace.clone());
    assert!(!call(&tools, "create_pdf", json!({ "path": "a.pdf", "content": "short" })).await.is_error);
    let full = call(&tools, "create_pdf", json!({ "path": "b.pdf", "content": "short" })).await;
    assert!(full.is_error && full.text.contains("workspace is full"), "{}", full.text);

    // An existing file is kept unless the model says to replace it.
    let kept = call(&tools, "create_pdf", json!({ "path": "a.pdf", "content": "other" })).await;
    assert!(kept.is_error && kept.text.contains("already exists"), "{}", kept.text);
    let before = read(&workspace, "a.pdf").await;
    assert!(
        !call(&tools, "create_pdf", json!({ "path": "a.pdf", "content": "other text", "overwrite": true }))
            .await
            .is_error
    );
    assert_ne!(read(&workspace, "a.pdf").await, before);

    // A read only workspace offers no tool at all.
    let read_only = Workspace::in_memory(WorkspaceLimits { read_only: true, ..WorkspaceLimits::default() });
    let tools = DocumentTools::new(read_only);
    assert!(tools.list().await.unwrap().is_empty());
    assert!(
        tools.call("create_pdf", json!({ "path": "a.pdf", "content": "x" }), ToolCallContext::empty()).await.is_err()
    );
}

#[tokio::test]
async fn users_only_write_into_their_own_directory() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let tools = DocumentTools::new(workspace.clone()).scoped_by_context();
    assert!(tools.accepts_secrets());
    let arguments = json!({ "path": "report.pdf", "content": "for ana" });
    let made = tools.call("create_pdf", arguments.clone(), &scope("ana")).await.unwrap();
    assert!(!made.is_error, "{}", made.text);
    assert!(workspace.scoped("ana").stat(&path("report.pdf")).await.unwrap().is_some());
    assert!(workspace.scoped("budi").stat(&path("report.pdf")).await.unwrap().is_none());
    assert!(workspace.stat(&path("report.pdf")).await.unwrap().is_none());
    // The same name is free for another user, and a call without a scope fails closed.
    assert!(!tools.call("create_pdf", arguments.clone(), &scope("budi")).await.unwrap().is_error);
    for context in [ToolCallContext::empty().clone(), scope(""), scope("  ")] {
        let refused = tools.call("create_pdf", arguments.clone(), &context).await.unwrap();
        assert!(refused.is_error && refused.text.contains("per user"), "{}", refused.text);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_large_document_does_not_block_the_async_worker() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let config = DocumentToolsConfig { max_content_chars: 1_000_000, max_pdf_pages: 2_000, ..Default::default() };
    let tools = std::sync::Arc::new(DocumentTools::new(workspace).with_config(config));
    let content = "A paragraph with enough words to fill a line of the page and then some more.\n\n".repeat(4_000);
    let writer = {
        let tools = std::sync::Arc::clone(&tools);
        tokio::spawn(async move { call(&tools, "create_pdf", json!({ "path": "big.pdf", "content": content })).await })
    };
    // With one worker thread, this timer only keeps ticking if the layout runs elsewhere.
    let mut longest = std::time::Duration::ZERO;
    while !writer.is_finished() {
        let started = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        longest = longest.max(started.elapsed());
    }
    let made = writer.await.unwrap();
    assert!(!made.is_error, "{}", made.text);
    assert!(longest < std::time::Duration::from_millis(250), "the worker stalled for {longest:?}");
}
