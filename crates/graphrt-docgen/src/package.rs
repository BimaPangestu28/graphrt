//! The zip container of DOCX and XLSX files.

use std::io::{Cursor, Write as _};

use crate::DocError;

/// Write `parts` (name, content) as a zip archive. Names are fixed by this
/// crate, never taken from a caller, and the time stamps are constant so the
/// same input gives the same file.
pub(crate) fn zip_parts(parts: &[(&str, String)]) -> Result<Vec<u8>, DocError> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, content) in parts {
        writer.start_file(*name, options).map_err(|e| DocError::Write(e.to_string()))?;
        writer.write_all(content.as_bytes()).map_err(|e| DocError::Write(e.to_string()))?;
    }
    let cursor = writer.finish().map_err(|e| DocError::Write(e.to_string()))?;
    Ok(cursor.into_inner())
}
