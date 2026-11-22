//! Decompression budget for PDF streams.
//!
//! The PDF parser (lopdf, behind pdf-extract) decodes streams without any size
//! limit: object streams while the document loads, page contents and fonts
//! while text is extracted. This module decodes each stream the way lopdf 0.42
//! does, into a counting sink, and rejects the document once the decoded total
//! passes [`crate::Options::max_decompressed_bytes`]:
//!
//! - `FlateDecode`, including lopdf's raw-deflate retry when the zlib header
//!   is unreadable;
//! - `LZWDecode` (with `EarlyChange`);
//! - `ASCII85Decode`;
//! - chains of these, in order;
//! - the PNG predictor's row buffers, which lopdf allocates from `/Columns`,
//!   `/Colors` and `/BitsPerComponent` alone.
//!
//! [`check_raw_streams`] runs on the raw bytes before loading. It finds streams
//! by their keywords, so it is a heuristic, but it is the only check that can
//! run before lopdf inflates object streams. [`check_loaded_streams`] runs on
//! the parsed document before any page is extracted and sees every stream
//! exactly (including decrypted ones).

use std::io::{self, Write};

use crate::DocError;

/// A decoding step lopdf applies to a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamFilter {
    Flate,
    Lzw,
    Ascii85,
}

impl StreamFilter {
    /// The filter for a `/Filter` name, `None` for names lopdf does not decode
    /// (it stops decoding the chain there).
    fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"FlateDecode" => Some(Self::Flate),
            b"LZWDecode" => Some(Self::Lzw),
            b"ASCII85Decode" => Some(Self::Ascii85),
            _ => None,
        }
    }
}

/// How lopdf would decode one stream.
#[derive(Debug, Default)]
struct DecodeRecipe {
    filters: Vec<StreamFilter>,
    /// `/DecodeParms /EarlyChange` (LZW); lopdf defaults to `true`.
    early_change: bool,
    /// Bytes per row of the PNG predictor buffers, when a PNG predictor is set.
    predictor_row_bytes: Option<u64>,
}

/// Running total of decoded bytes for one document.
pub(crate) struct DecodeBudget {
    limit: u64,
    remaining: u64,
}

impl DecodeBudget {
    pub(crate) fn new(limit: u64) -> Self {
        Self { limit, remaining: limit }
    }

    fn exceeded(&self) -> DocError {
        DocError::LimitExceeded(format!("PDF streams decompress to more than {} bytes", self.limit))
    }

    /// Decode `data` with `recipe`, charging the output of the last step.
    fn charge(&mut self, data: &[u8], recipe: &DecodeRecipe) -> Result<(), DocError> {
        if recipe.filters.is_empty() {
            return Ok(());
        }
        // lopdf allocates two zeroed rows per predictor pass, whatever the data.
        if recipe.predictor_row_bytes.is_some_and(|row| row.saturating_mul(2) > self.remaining) {
            return Err(self.exceeded());
        }
        // Output of the previous step; only steps followed by another keep theirs.
        let mut intermediate = Vec::new();
        for (step, filter) in recipe.filters.iter().enumerate() {
            let last = step + 1 == recipe.filters.len();
            let input = if step == 0 { data } else { intermediate.as_slice() };
            let mut output = CappedSink::new(self.remaining, !last);
            decode_step(*filter, recipe.early_change, input, &mut output);
            if output.overflowed {
                return Err(self.exceeded());
            }
            if last {
                self.remaining -= output.written;
            }
            intermediate = output.kept;
        }
        Ok(())
    }
}

/// Writer that counts bytes, optionally keeps them, and refuses to pass `limit`.
struct CappedSink {
    limit: u64,
    written: u64,
    keep: bool,
    kept: Vec<u8>,
    overflowed: bool,
}

impl CappedSink {
    fn new(limit: u64, keep: bool) -> Self {
        Self { limit, written: 0, keep, kept: Vec::new(), overflowed: false }
    }
}

impl Write for CappedSink {
    fn write(&mut self, chunk: &[u8]) -> io::Result<usize> {
        let len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        if self.written.saturating_add(len) > self.limit {
            self.overflowed = true;
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.written += len;
        if self.keep {
            self.kept.extend_from_slice(chunk);
        }
        Ok(chunk.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Decode one step into `output`. Corrupt input stops early, as in lopdf, which
/// keeps whatever was decoded so far; only the size matters here.
fn decode_step(filter: StreamFilter, early_change: bool, input: &[u8], output: &mut CappedSink) {
    match filter {
        StreamFilter::Flate => inflate_like_lopdf(input, output),
        StreamFilter::Lzw => {
            use weezl::{BitOrder, decode::Decoder};
            let mut decoder = if early_change {
                Decoder::with_tiff_size_switch(BitOrder::Msb, 8)
            } else {
                Decoder::new(BitOrder::Msb, 8)
            };
            // Errors leave the partial output, which is what lopdf keeps too.
            let _ = decoder.into_stream(&mut *output).decode_all(input);
        }
        StreamFilter::Ascii85 => decode_ascii85(input, output),
    }
}

/// Zlib, then raw deflate after the two header bytes when zlib produced nothing.
fn inflate_like_lopdf(input: &[u8], output: &mut CappedSink) {
    let zlib = io::copy(&mut flate2::read::ZlibDecoder::new(input), output);
    if zlib.is_ok() || output.written > 0 || output.overflowed {
        return;
    }
    if let Some(raw) = input.get(2..) {
        let _ = io::copy(&mut flate2::read::DeflateDecoder::new(raw), output);
    }
}

/// ASCII85 as lopdf decodes it: `z` is four zero bytes, other characters come
/// in groups of five, decoding stops at the first character outside `!`..`u`.
fn decode_ascii85(input: &[u8], output: &mut CappedSink) {
    let input = input.strip_suffix(b"~>").unwrap_or(input);
    let mut group: u32 = 0;
    let mut count = 0usize;
    for &byte in input {
        if byte.is_ascii_whitespace() {
            continue;
        }
        let written = match byte {
            b'z' if count == 0 => output.write_all(&[0; 4]),
            b'!'..=b'u' => {
                let Some(next) = group.checked_mul(85).and_then(|g| g.checked_add(u32::from(byte - b'!'))) else {
                    return;
                };
                group = next;
                count += 1;
                if count < 5 {
                    continue;
                }
                let bytes = group.to_be_bytes();
                (group, count) = (0, 0);
                output.write_all(&bytes)
            }
            _ => return,
        };
        if written.is_err() {
            return;
        }
    }
    // A final partial group of n characters is padded with `u` and decodes to
    // n - 1 bytes.
    if count > 0 {
        let padded = (count..5).try_fold(group, |g, _| g.checked_mul(85).and_then(|g| g.checked_add(84)));
        if let Some(padded) = padded {
            let _ = output.write_all(padded.to_be_bytes().get(..count - 1).unwrap_or_default());
        }
    }
}

/// Check the streams of a parsed document (exact dictionaries and data).
pub(crate) fn check_loaded_streams(document: &pdf_extract::Document, limit: u64) -> Result<(), DocError> {
    let mut budget = DecodeBudget::new(limit);
    for object in document.objects.values() {
        if let pdf_extract::Object::Stream(stream) = object {
            budget.charge(&stream.content, &loaded_recipe(&stream.dict))?;
        }
    }
    Ok(())
}

fn loaded_recipe(dict: &pdf_extract::Dictionary) -> DecodeRecipe {
    use pdf_extract::Object;
    let names: Vec<&[u8]> = match dict.get(b"Filter") {
        Ok(Object::Name(name)) => vec![name.as_slice()],
        Ok(Object::Array(items)) => items.iter().filter_map(|item| item.as_name().ok()).collect(),
        _ => Vec::new(),
    };
    let params = dict.get(b"DecodeParms").and_then(Object::as_dict).ok();
    let param = |key: &[u8]| params.and_then(|p| p.get(key).and_then(Object::as_i64).ok());
    DecodeRecipe {
        filters: names.into_iter().map_while(StreamFilter::from_name).collect(),
        early_change: param(b"EarlyChange").is_none_or(|v| v != 0),
        predictor_row_bytes: predictor_row_bytes(
            param(b"Predictor"),
            param(b"Columns"),
            param(b"Colors"),
            param(b"BitsPerComponent"),
        ),
    }
}

/// Row size of lopdf's PNG predictor buffers: `max(1, columns) * max(1, colors) *
/// max(8, bits) / 8`, for predictors 10 to 15 only.
fn predictor_row_bytes(
    predictor: Option<i64>,
    columns: Option<i64>,
    colors: Option<i64>,
    bits: Option<i64>,
) -> Option<u64> {
    if !(10..=15).contains(&predictor.unwrap_or(1)) {
        return None;
    }
    let at_least = |value: Option<i64>, floor: u64| value.map_or(floor, |v| u64::try_from(v).unwrap_or(0).max(floor));
    Some(at_least(columns, 1).saturating_mul(at_least(colors, 1)).saturating_mul(at_least(bits, 8)) / 8)
}

/// Check the streams found in the raw bytes, before the document is loaded.
pub(crate) fn check_raw_streams(bytes: &[u8], limit: u64) -> Result<(), DocError> {
    let mut budget = DecodeBudget::new(limit);
    let mut pos = 0usize;
    // Start of the text that may hold the current stream's dictionary: the end
    // of the previous stream, so the dictionaries' windows never overlap.
    let mut dict_from = 0usize;
    while let Some(offset) = find(bytes.get(pos..).unwrap_or_default(), b"stream") {
        let keyword = pos + offset;
        let after_keyword = keyword + b"stream".len();
        if !follows_dictionary(bytes, keyword) {
            pos = after_keyword;
            continue;
        }
        let data_start = skip_eol(bytes, after_keyword);
        let Some(end_offset) = find(bytes.get(data_start..).unwrap_or_default(), b"endstream") else {
            break;
        };
        let data_end = data_start + end_offset;
        let dict = bytes.get(dict_from..keyword).unwrap_or_default();
        budget.charge(bytes.get(data_start..data_end).unwrap_or_default(), &raw_recipe(dict))?;
        pos = data_end + b"endstream".len();
        dict_from = pos;
    }
    Ok(())
}

/// Whether `stream` at `keyword` follows a dictionary (`>>`, then whitespace),
/// and is not the tail of `endstream`.
fn follows_dictionary(bytes: &[u8], keyword: usize) -> bool {
    let before = bytes.get(..keyword).unwrap_or_default();
    let trimmed = before.trim_ascii_end();
    trimmed.ends_with(b">>") && !before.ends_with(b"end")
}

/// Position after the end-of-line that follows the `stream` keyword.
fn skip_eol(bytes: &[u8], at: usize) -> usize {
    match bytes.get(at..at + 2) {
        Some(b"\r\n") => at + 2,
        Some([b'\n' | b'\r', _]) => at + 1,
        _ => at,
    }
}

/// Recipe from the text before a stream. The last `/Filter` in the text belongs
/// to the stream's own dictionary, which ends right before the keyword.
fn raw_recipe(text: &[u8]) -> DecodeRecipe {
    let Some(filter_at) = rfind(text, b"/Filter") else { return DecodeRecipe::default() };
    let value = text.get(filter_at + b"/Filter".len()..).unwrap_or_default().trim_ascii_start();
    let names: Vec<&[u8]> = match value.strip_prefix(b"[") {
        Some(array) => {
            let end = array.iter().position(|&b| b == b']').unwrap_or(array.len());
            array.get(..end).unwrap_or_default().split(|&b| b == b'/').skip(1).map(<[u8]>::trim_ascii).collect()
        }
        None => value.strip_prefix(b"/").map(name_token).into_iter().collect(),
    };
    let param =
        |key: &[u8]| rfind(text, key).and_then(|at| integer_after(text.get(at + key.len()..).unwrap_or_default()));
    DecodeRecipe {
        filters: names.into_iter().map_while(StreamFilter::from_name).collect(),
        early_change: param(b"/EarlyChange").is_none_or(|v| v != 0),
        predictor_row_bytes: predictor_row_bytes(
            param(b"/Predictor"),
            param(b"/Columns"),
            param(b"/Colors"),
            param(b"/BitsPerComponent"),
        ),
    }
}

/// A PDF name's characters, up to the next whitespace or delimiter.
fn name_token(text: &[u8]) -> &[u8] {
    let end = text.iter().position(|b| b.is_ascii_whitespace() || b"/<>[]()%".contains(b)).unwrap_or(text.len());
    text.get(..end).unwrap_or_default()
}

/// The integer at the start of `text`, after optional whitespace.
fn integer_after(text: &[u8]) -> Option<i64> {
    let text = text.trim_ascii_start();
    let sign_len = usize::from(matches!(text.first(), Some(b'+' | b'-')));
    let digits = text.get(sign_len..)?.iter().take_while(|b| b.is_ascii_digit()).count();
    let number = std::str::from_utf8(text.get(..sign_len + digits)?).ok()?;
    // Values too large for i64 are as large as it gets.
    number.parse().ok().or(if digits > 0 && !number.starts_with('-') { Some(i64::MAX) } else { None })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_recipes_read_names_arrays_and_nested_parameters() {
        let recipe = raw_recipe(b"<< /Filter [/FlateDecode /LZWDecode] /DecodeParms << /EarlyChange 0 >> >>");
        assert_eq!(recipe.filters, [StreamFilter::Flate, StreamFilter::Lzw]);
        assert!(!recipe.early_change);
        let recipe = raw_recipe(b"<</Filter/LZWDecode/DecodeParms<</Predictor 12/Columns 99999999999999999999>>>>");
        assert_eq!(recipe.filters, [StreamFilter::Lzw]);
        assert!(recipe.early_change);
        assert!(recipe.predictor_row_bytes.is_some_and(|row| row > 1 << 60));
        assert!(raw_recipe(b"<< /Filter /DCTDecode >>").filters.is_empty());
        assert!(raw_recipe(b"<< /Length 5 >>").filters.is_empty());
    }

    #[test]
    fn ascii85_matches_the_reference_decoding() {
        let mut sink = CappedSink::new(100, true);
        decode_ascii85(b"87cURD]i,\"Ebo80~>", &mut sink);
        assert_eq!(sink.kept, b"Hello World!");
        let mut sink = CappedSink::new(100, true);
        decode_ascii85(b"zz", &mut sink);
        assert_eq!(sink.kept, [0; 8]);
    }

    #[test]
    fn the_budget_counts_only_final_output() {
        let mut budget = DecodeBudget::new(10);
        let recipe =
            DecodeRecipe { filters: vec![StreamFilter::Ascii85], early_change: true, predictor_row_bytes: None };
        assert!(budget.charge(b"zz", &recipe).is_ok());
        assert_eq!(budget.remaining, 2);
        assert!(budget.charge(b"z", &recipe).is_err());
    }
}
