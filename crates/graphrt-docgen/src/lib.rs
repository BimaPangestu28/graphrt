//! # graphrt-docgen
//!
//! Document creation for graphrt agents: PDF, Word and Excel files.
//!
//! | Piece | What it does |
//! | --- | --- |
//! | [`markdown`] | The document model, read from the Markdown models already write. |
//! | [`pdf`] | A4 pages with the fonts every reader has built in. No font files. |
//! | [`docx`] | A Word document with headings, lists and tables (feature `office`). |
//! | [`xlsx`] | A workbook from rows of cells, text never a formula (feature `office`). |
//! | [`DocumentTools`] | The `documents` tools over a `graphrt-files` workspace (feature `tools`). |
//!
//! The writers are small and in this crate: they produce text, tables and
//! lines, and nothing a reader would run or fetch (no scripts, macros, links,
//! embedded objects or formulas). Nothing here is on unless a host registers
//! the tools.
//!
//! ```
//! use graphrt_docgen::{markdown, pdf};
//!
//! let blocks = markdown::parse("# Report\n\nRevenue grew **12%**.\n\n| Region | Revenue |\n|---|---|\n| Jakarta | 120 |");
//! let document = pdf::render(&blocks, &pdf::PdfOptions::default())?;
//! assert!(document.bytes.starts_with(b"%PDF-1.4"));
//! assert_eq!(document.pages, 1);
//! # Ok::<(), graphrt_docgen::DocError>(())
//! ```

#[cfg(feature = "office")]
pub mod docx;
pub mod markdown;
#[cfg(feature = "office")]
mod package;
pub mod pdf;
#[cfg(feature = "tools")]
mod tools;
#[cfg(feature = "office")]
pub mod xlsx;
#[cfg(feature = "office")]
mod xml;

#[cfg(feature = "tools")]
pub use tools::{DocumentTools, DocumentToolsConfig, PROVIDER};

/// Why a document was not written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DocError {
    #[error("the document has more {what} than the limit of {limit}")]
    TooLong { what: &'static str, limit: usize },
    #[error("{0}")]
    Empty(&'static str),
    #[error("the document could not be written: {0}")]
    Write(String),
}
