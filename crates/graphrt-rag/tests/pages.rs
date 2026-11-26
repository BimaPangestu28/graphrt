//! PDF page numbers: from graphrt-doc's page markers to chunk metadata and retrieval hits.

#![cfg(feature = "doc")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;

use graphrt_rag::{
    ChunkerConfig, DocExtractor, Document, HashEmbedder, Ingestor, MemoryStore, PAGE_MARKER_END, PAGE_MARKER_START,
    RetrievalRequest, Retriever, StoreRetriever, StoredChunk, VectorStore,
};

/// A PDF with one page per entry of `pages`, each showing that text (empty: a page without text).
fn pdf_with_pages(pages: &[&str]) -> Vec<u8> {
    let count = pages.len();
    let kids: Vec<String> = (0..count).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count {count} >>", kids.join(" ")).into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    for (i, text) in pages.iter().enumerate() {
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                5 + 2 * i
            )
            .into_bytes(),
        );
        let content = if text.is_empty() { String::new() } else { format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET") };
        let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream.extend_from_slice(content.as_bytes());
        stream.extend_from_slice(b"\nendstream");
        objects.push(stream);
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes(),
    );
    out
}

const HOURS: &str = "Opening hours are nine to five on weekdays";
const FEES: &str = "Delivery fees depend on the distance to the venue";

async fn ingest_pdf(store: &Arc<MemoryStore>, config: ChunkerConfig) {
    let bytes = pdf_with_pages(&[HOURS, "", FEES]);
    let ingestor = Ingestor::new(store.clone(), Arc::new(HashEmbedder::new(64))).chunker(config);
    let doc = Document::new("kb", "menu", String::new()).title("menu.pdf");
    ingestor.ingest_bytes(doc, &bytes, Some("application/pdf"), "menu.pdf", &DocExtractor::new()).await.unwrap();
}

fn has_marker(chunk: &StoredChunk) -> bool {
    chunk.text.contains([PAGE_MARKER_START, PAGE_MARKER_END])
}

#[tokio::test]
async fn pdf_passages_carry_their_page_and_no_markers() {
    let store = Arc::new(MemoryStore::new());
    // One chunk per page.
    ingest_pdf(&store, ChunkerConfig { max_chars: 60, overlap: 0, ..ChunkerConfig::default() }).await;
    let chunks = store.document_chunks("kb", "menu").await.unwrap();
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    assert!(!chunks.iter().any(has_marker), "{chunks:?}");

    let retriever = StoreRetriever::new(store.clone(), Arc::new(HashEmbedder::new(64)));
    let hits =
        retriever.retrieve(&RetrievalRequest::new("delivery fees distance", vec!["kb".into()]).top_k(1)).await.unwrap();
    assert_eq!(hits[0].content.trim(), FEES);
    // Page 2 has no text, so the fees are on page 3.
    assert_eq!(hits[0].metadata.get("page"), Some(&serde_json::json!(3)));
    assert_eq!(hits[0].metadata.get("pages"), None);
    let hits =
        retriever.retrieve(&RetrievalRequest::new("opening hours weekdays", vec!["kb".into()]).top_k(1)).await.unwrap();
    assert_eq!(hits[0].metadata.get("page"), Some(&serde_json::json!(1)));
}

#[tokio::test]
async fn a_passage_spanning_pages_lists_them_all() {
    let store = Arc::new(MemoryStore::new());
    ingest_pdf(&store, ChunkerConfig::default()).await;
    let chunks = store.document_chunks("kb", "menu").await.unwrap();
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    assert!(chunks[0].text.contains(HOURS) && chunks[0].text.contains(FEES));
    assert!(!has_marker(&chunks[0]));
    assert_eq!(chunks[0].metadata.get("page"), Some(&serde_json::json!(1)));
    assert_eq!(chunks[0].metadata.get("pages"), Some(&serde_json::json!([1, 3])));
}

#[tokio::test]
async fn text_without_markers_is_stored_as_before() {
    let store = Arc::new(MemoryStore::new());
    let ingestor = Ingestor::new(store.clone(), Arc::new(HashEmbedder::new(64)));
    ingestor.ingest(&Document::new("kb", "notes", "Plain notes without pages.")).await.unwrap();
    let chunks = store.document_chunks("kb", "notes").await.unwrap();
    assert_eq!(chunks[0].text, "Plain notes without pages.");
    assert_eq!(chunks[0].metadata.get("page"), None);
}
