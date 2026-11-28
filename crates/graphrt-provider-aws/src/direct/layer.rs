//! The one image layer a direct deploy adds on top of the graphrt image: the
//! Lambda Web Adapter extension and the agent file, as a gzip-compressed tar.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

const BLOCK: usize = 512;
/// Largest file pulled out of a layer (the adapter is about 3 MiB).
const MAX_EXTRACTED_BYTES: usize = 64 * 1024 * 1024;
/// Largest uncompressed layer read while looking for a file.
const MAX_LAYER_TAR_BYTES: u64 = 256 * 1024 * 1024;

/// One file or directory of a new layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Path without a leading slash, e.g. `opt/extensions/lambda-adapter`.
    pub path: String,
    pub mode: u32,
    /// `None` for a directory.
    pub contents: Option<Vec<u8>>,
}

impl Entry {
    pub(crate) fn dir(path: &str) -> Self {
        Self { path: path.to_owned(), mode: 0o755, contents: None }
    }

    pub(crate) fn file(path: &str, mode: u32, contents: Vec<u8>) -> Self {
        Self { path: path.to_owned(), mode, contents: Some(contents) }
    }
}

/// A finished layer: the compressed bytes and both digests the image needs.
#[derive(Clone)]
pub(crate) struct Layer {
    pub gzip: Vec<u8>,
    /// `sha256:<hex>` of the compressed bytes (the blob digest).
    pub digest: String,
    /// `sha256:<hex>` of the uncompressed tar (the config's `rootfs.diff_ids`).
    pub diff_id: String,
}

impl std::fmt::Debug for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Layer").field("bytes", &self.gzip.len()).field("digest", &self.digest).finish()
    }
}

pub(crate) fn sha256_digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    let bytes = text.as_bytes();
    let start = bytes.len().saturating_sub(width);
    for (slot, byte) in field.iter_mut().zip(bytes.iter().skip(start)) {
        *slot = *byte;
    }
    if let Some(last) = field.last_mut() {
        *last = 0;
    }
}

/// The bytes `range` of a header block (always inside the 512 bytes here).
fn span(block: &mut [u8; BLOCK], range: std::ops::Range<usize>) -> &mut [u8] {
    block.get_mut(range).unwrap_or_default()
}

fn put(field: &mut [u8], text: &[u8]) {
    for (slot, byte) in field.iter_mut().zip(text) {
        *slot = *byte;
    }
}

/// A ustar header for `entry` (paths up to 100 bytes, which every path here is).
fn header(entry: &Entry) -> Result<[u8; BLOCK], String> {
    let mut name = entry.path.clone();
    if entry.contents.is_none() && !name.ends_with('/') {
        name.push('/');
    }
    if name.len() > 100 {
        return Err(format!("layer path {name:?} is longer than 100 bytes"));
    }
    let mut block = [0u8; BLOCK];
    let size = entry.contents.as_ref().map_or(0, Vec::len);
    put(span(&mut block, 0..100), name.as_bytes());
    octal(span(&mut block, 100..108), u64::from(entry.mode));
    octal(span(&mut block, 108..116), 0);
    octal(span(&mut block, 116..124), 0);
    octal(span(&mut block, 124..136), u64::try_from(size).unwrap_or(u64::MAX));
    octal(span(&mut block, 136..148), 0);
    put(span(&mut block, 156..157), if entry.contents.is_some() { b"0" } else { b"5" });
    put(span(&mut block, 257..263), b"ustar\0");
    put(span(&mut block, 263..265), b"00");
    // The checksum is computed with its own field as eight spaces.
    put(span(&mut block, 148..156), b"        ");
    let sum: u64 = block.iter().map(|byte| u64::from(*byte)).sum();
    let text = format!("{sum:06o}\0 ");
    put(span(&mut block, 148..156), text.as_bytes());
    Ok(block)
}

/// The uncompressed tar of `entries`.
pub(crate) fn tar(entries: &[Entry]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(&header(entry)?);
        if let Some(contents) = &entry.contents {
            out.extend_from_slice(contents);
            let padding = (BLOCK - contents.len() % BLOCK) % BLOCK;
            out.extend(std::iter::repeat_n(0u8, padding));
        }
    }
    out.extend(std::iter::repeat_n(0u8, BLOCK * 2));
    Ok(out)
}

/// Builds the layer of `entries`.
///
/// @throws String - a path too long for a tar header, or compression failing
pub(crate) fn build(entries: &[Entry]) -> Result<Layer, String> {
    let tar = tar(entries)?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tar).map_err(|error| format!("cannot compress the layer: {error}"))?;
    let gzip = encoder.finish().map_err(|error| format!("cannot compress the layer: {error}"))?;
    Ok(Layer { digest: sha256_digest(&gzip), diff_id: sha256_digest(&tar), gzip })
}

fn parse_octal(field: &[u8]) -> Option<u64> {
    let text: String = field.iter().take_while(|byte| **byte != 0).map(|byte| char::from(*byte)).collect::<String>();
    let text = text.trim();
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

/// The contents of the regular file `path` inside a gzip-compressed tar layer.
///
/// @throws String - the layer is not a readable tar or has no such file
pub(crate) fn extract_file(gzip: &[u8], path: &str) -> Result<Vec<u8>, String> {
    let mut tar = Vec::new();
    GzDecoder::new(gzip)
        .take(MAX_LAYER_TAR_BYTES)
        .read_to_end(&mut tar)
        .map_err(|error| format!("the layer is not gzip data: {error}"))?;
    let wanted = path.trim_start_matches('/');
    let mut offset = 0usize;
    while let Some(block) = tar.get(offset..offset + BLOCK) {
        if block.iter().all(|byte| *byte == 0) {
            break;
        }
        let name_field = block.get(0..100).unwrap_or_default();
        let prefix_field = block.get(345..500).unwrap_or_default();
        let field_text = |field: &[u8]| -> String {
            field.iter().take_while(|byte| **byte != 0).map(|byte| char::from(*byte)).collect()
        };
        let prefix = field_text(prefix_field);
        let name = field_text(name_field);
        let full = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let size = parse_octal(block.get(124..136).unwrap_or_default())
            .and_then(|size| usize::try_from(size).ok())
            .ok_or_else(|| "a tar header has a bad size".to_owned())?;
        let kind = block.get(156).copied().unwrap_or(b'0');
        let data_start = offset + BLOCK;
        if (kind == b'0' || kind == 0) && full.trim_start_matches("./").trim_start_matches('/') == wanted {
            if size > MAX_EXTRACTED_BYTES {
                return Err(format!("{wanted} is larger than {MAX_EXTRACTED_BYTES} bytes"));
            }
            return tar
                .get(data_start..data_start + size)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| format!("{wanted} is cut short in the layer"));
        }
        offset = data_start + size.div_ceil(BLOCK) * BLOCK;
    }
    Err(format!("the layer has no file {wanted}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_layer_reads_back_and_its_digests_match_its_bytes() {
        let layer = build(&[
            Entry::dir("opt"),
            Entry::dir("opt/extensions"),
            Entry::file("opt/extensions/lambda-adapter", 0o755, vec![7u8; 1500]),
            Entry::file("workflows/support.yaml", 0o444, b"name: support\n".to_vec()),
        ])
        .unwrap();
        assert_eq!(layer.digest, sha256_digest(&layer.gzip));
        assert_eq!(extract_file(&layer.gzip, "/workflows/support.yaml").unwrap(), b"name: support\n");
        assert_eq!(extract_file(&layer.gzip, "opt/extensions/lambda-adapter").unwrap(), vec![7u8; 1500]);
        assert!(extract_file(&layer.gzip, "missing").unwrap_err().contains("no file missing"));
    }

    #[test]
    fn headers_carry_a_valid_checksum_and_type() {
        let block = header(&Entry::file("a/b", 0o644, vec![1, 2, 3])).unwrap();
        let stored = parse_octal(&block[148..155]).unwrap();
        let mut copy = block;
        copy[148..156].copy_from_slice(b"        ");
        assert_eq!(stored, copy.iter().map(|byte| u64::from(*byte)).sum::<u64>());
        assert_eq!(block[156], b'0');
        assert_eq!(parse_octal(&block[124..136]), Some(3));
        assert_eq!(header(&Entry::dir("opt")).unwrap()[156], b'5');
        assert!(header(&Entry::file(&"x".repeat(101), 0o644, Vec::new())).is_err());
    }
}
