//! Removing metadata from an image without decoding it.
//!
//! A photo from a phone carries where it was taken (GPS in EXIF), on which
//! device and when, often a thumbnail of the uncropped picture, and sometimes
//! free text. None of that helps a model see the image, and all of it would
//! leave the machine. [`strip_metadata`] rewrites the container without those
//! parts. The pixel data is copied as it is: no decoding, no quality loss.
//!
//! It also drops whatever follows the end of the image, so a file that is a
//! valid picture with something else appended (a "polyglot") comes out as
//! just the picture.

use crate::error::ImageError;
use crate::probe::{
    ImageFormat, be_u16, gif_layout, jpeg_header_segments, jpeg_segment_is_metadata, malformed, png_chunk_is_rendering,
    png_chunks, sniff_format, webp_chunk_is_metadata, webp_chunks,
};

/// `bytes` without metadata: EXIF, XMP, IPTC and comments of a JPEG, text and
/// EXIF chunks of a PNG, EXIF and XMP chunks of a WebP, comments and XMP of a
/// GIF, and anything after the end of the image. Colour profiles stay.
///
/// The EXIF orientation of a JPEG goes too, so the picture may display
/// rotated: rotate the pixels first when [`crate::ImageInfo::needs_rotation`].
pub fn strip_metadata(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    match sniff_format(bytes) {
        Some(ImageFormat::Png) => strip_png(bytes),
        Some(ImageFormat::Jpeg) => strip_jpeg(bytes),
        Some(ImageFormat::Gif) => strip_gif(bytes),
        Some(ImageFormat::WebP) => strip_webp(bytes),
        None => Err(ImageError::UnsupportedFormat("not a PNG, JPEG, GIF or WebP image".to_owned())),
    }
}

fn strip_png(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(bytes.get(..8).unwrap_or_default());
    let mut ended = false;
    for chunk in png_chunks(bytes) {
        let chunk = chunk?;
        if png_chunk_is_rendering(chunk.kind) {
            out.extend_from_slice(chunk.raw);
        }
        ended = chunk.kind == b"IEND";
    }
    if !ended {
        return Err(malformed(ImageFormat::Png, "the image has no end chunk"));
    }
    Ok(out)
}

fn strip_jpeg(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let format = ImageFormat::Jpeg;
    let (segments, scan_start) = jpeg_header_segments(bytes)?;
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&[0xff, 0xd8]);
    for segment in segments.iter().filter(|segment| !jpeg_segment_is_metadata(segment.marker)) {
        out.extend_from_slice(segment.raw);
    }
    // From the first scan on: entropy-coded data with markers in between
    // (more scans and tables in a progressive JPEG), up to the end of image.
    let mut at = scan_start;
    loop {
        let (Some(0xff), Some(&marker)) = (bytes.get(at).copied(), bytes.get(at + 1)) else {
            return Err(malformed(format, "the pixel data is cut short"));
        };
        match marker {
            0xd9 => {
                out.extend_from_slice(&[0xff, 0xd9]);
                return Ok(out);
            }
            0xff => at += 1,
            0x01 | 0xd0..=0xd7 => {
                out.extend_from_slice(&[0xff, marker]);
                at += 2;
            }
            _ => {
                let length =
                    usize::from(be_u16(bytes, at + 2).ok_or_else(|| malformed(format, "a segment is cut short"))?);
                let end = at + 2 + length;
                let (true, Some(raw)) = (length >= 2, bytes.get(at..end)) else {
                    return Err(malformed(format, "a segment is cut short"));
                };
                if !jpeg_segment_is_metadata(marker) {
                    out.extend_from_slice(raw);
                }
                at = end;
                if marker == 0xda {
                    at = copy_scan_data(bytes, at, &mut out);
                }
            }
        }
    }
}

/// Copy entropy-coded data from `at` up to the next real marker, and return
/// its position. Inside scan data, `FF 00` is a literal 0xFF and `FF D0..D7`
/// are restart markers: both belong to the data.
fn copy_scan_data(bytes: &[u8], start: usize, out: &mut Vec<u8>) -> usize {
    let mut at = start;
    while let Some(&byte) = bytes.get(at) {
        if byte == 0xff && !matches!(bytes.get(at + 1), Some(0x00 | 0xd0..=0xd7)) {
            break;
        }
        at += if byte == 0xff { 2 } else { 1 };
    }
    let end = at.min(bytes.len());
    out.extend_from_slice(bytes.get(start..end).unwrap_or_default());
    end
}

fn strip_webp(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(b"RIFF\0\0\0\0WEBP");
    for chunk in webp_chunks(bytes) {
        let chunk = chunk?;
        if webp_chunk_is_metadata(chunk.kind) {
            continue;
        }
        let start = out.len();
        out.extend_from_slice(chunk.raw);
        if chunk.raw.len() % 2 == 1 {
            out.push(0);
        }
        if chunk.kind == b"VP8X"
            && let Some(flags) = out.get_mut(start + 8)
        {
            // Clear the "has EXIF" (0x08) and "has XMP" (0x04) bits.
            *flags &= !0x0c;
        }
    }
    let riff_size = u32::try_from(out.len() - 8)
        .map_err(|_| malformed(ImageFormat::WebP, "the image is larger than a RIFF file can be"))?;
    if let Some(size) = out.get_mut(4..8) {
        size.copy_from_slice(&riff_size.to_le_bytes());
    }
    if out.len() <= 12 {
        return Err(malformed(ImageFormat::WebP, "no image data"));
    }
    Ok(out)
}

/// A GIF is a header followed by blocks. Comment extensions and application
/// extensions other than the animation loop count are dropped, and so is
/// anything after the trailer.
fn strip_gif(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let layout = gif_layout(bytes)?;
    if !layout.complete {
        return Err(malformed(ImageFormat::Gif, "the image is cut short or holds an unknown block"));
    }
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(layout.header);
    for block in layout.blocks.iter().filter(|block| block.rendering) {
        out.extend_from_slice(block.raw);
    }
    out.push(0x3b);
    Ok(out)
}
