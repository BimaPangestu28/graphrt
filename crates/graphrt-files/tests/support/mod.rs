//! Helpers shared by the integration tests (each test binary uses a subset).

#![allow(dead_code, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use graphrt_files::WorkspacePath;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory under the system temp directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let unique =
            format!("graphrt-files-{label}-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn path(raw: &str) -> WorkspacePath {
    WorkspacePath::parse(raw).unwrap()
}

/// A one-page PDF showing `text`, with a correct xref table.
pub fn pdf(text: &str) -> Vec<u8> {
    let content = format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET");
    let stream = format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len());
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_owned(),
        stream,
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    let mut out = "%PDF-1.4\n".to_owned();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{body}\nendobj\n", index + 1));
    }
    let xref = out.len();
    out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
    for offset in offsets {
        out.push_str(&format!("{offset:010} 00000 n \n"));
    }
    out.push_str(&format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1));
    out.into_bytes()
}
