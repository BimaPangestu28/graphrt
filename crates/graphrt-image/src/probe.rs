//! Reading what an image is from its header, without decoding it.
//!
//! [`probe`] answers three questions from the first bytes: which format, how
//! many pixels, and whether metadata is attached. It is the gate in front of
//! every decoder: an image that declares a canvas above the limits is refused
//! here, before anything is allocated for it.

use crate::error::ImageError;

/// The image formats every model provider accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
}

impl ImageFormat {
    /// MIME type, for example `image/png`.
    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::WebP => "image/webp",
        }
    }

    /// Usual file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Gif => "gif",
            ImageFormat::WebP => "webp",
        }
    }

    /// The format a name such as `png`, `jpg`, `jpeg`, `gif` or `webp` means.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
            "png" | "image/png" => Some(ImageFormat::Png),
            "jpg" | "jpeg" | "image/jpeg" => Some(ImageFormat::Jpeg),
            "gif" | "image/gif" => Some(ImageFormat::Gif),
            "webp" | "image/webp" => Some(ImageFormat::WebP),
            _ => None,
        }
    }
}

/// What the header of an image says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub format: ImageFormat,
    /// Width in pixels, as stored (before any EXIF rotation).
    pub width: u32,
    /// Height in pixels, as stored.
    pub height: u32,
    /// Whether the file carries metadata that [`crate::strip_metadata`] removes
    /// (EXIF, XMP, IPTC, text chunks, comments).
    pub has_metadata: bool,
    /// EXIF orientation (1 to 8) of a JPEG, when it has one. 1 means upright.
    pub orientation: Option<u8>,
    /// Whether the file is an animation (GIF with several frames is not
    /// detected here; APNG and animated WebP are).
    pub animated: bool,
}

impl ImageInfo {
    /// Width times height.
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Whether the stored pixels must be rotated or mirrored to be upright.
    pub fn needs_rotation(&self) -> bool {
        self.orientation.is_some_and(|orientation| orientation != 1)
    }
}

/// The format of `bytes`, from the magic number alone.
pub fn sniff_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(ImageFormat::WebP)
    } else {
        None
    }
}

/// Read format, dimensions and metadata flags from the header of `bytes`.
///
/// Fails with [`ImageError::UnsupportedFormat`] for anything that is not a
/// PNG, JPEG, GIF or WebP by its magic number (an SVG, an HTML page or a HEIC
/// photo, whatever MIME type it was sent with), and with
/// [`ImageError::Malformed`] when the header is cut short or inconsistent.
///
/// ```
/// use graphrt_image::{ImageFormat, probe};
///
/// // A 1x1 GIF.
/// let gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";
/// let info = probe(gif)?;
/// assert_eq!((info.format, info.width, info.height), (ImageFormat::Gif, 1, 1));
/// assert!(probe(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_err());
/// # Ok::<(), graphrt_image::ImageError>(())
/// ```
pub fn probe(bytes: &[u8]) -> Result<ImageInfo, ImageError> {
    let info = match sniff_format(bytes) {
        Some(ImageFormat::Png) => probe_png(bytes),
        Some(ImageFormat::Jpeg) => probe_jpeg(bytes),
        Some(ImageFormat::Gif) => probe_gif(bytes),
        Some(ImageFormat::WebP) => probe_webp(bytes),
        None => return Err(ImageError::UnsupportedFormat(describe(bytes))),
    }?;
    if info.width == 0 || info.height == 0 {
        return Err(malformed(info.format, "the image has no pixels"));
    }
    Ok(info)
}

/// A short name for content that is not a supported image, for error messages.
fn describe(bytes: &[u8]) -> String {
    let head = bytes.get(..bytes.len().min(256)).unwrap_or_default();
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let is_isobmff = head.get(4..8) == Some(b"ftyp");
    let brand = head.get(8..12).unwrap_or_default();
    if text.contains("<svg") {
        "SVG (not accepted: it can carry scripts)".to_owned()
    } else if text.contains("<!doctype html") || text.contains("<html") {
        "HTML".to_owned()
    } else if is_isobmff && matches!(brand, b"heic" | b"heix" | b"hevc" | b"mif1" | b"msf1" | b"heim" | b"heis") {
        "HEIC (convert it to JPEG first)".to_owned()
    } else if is_isobmff && matches!(brand, b"avif" | b"avis") {
        "AVIF (convert it to PNG or JPEG first)".to_owned()
    } else if head.starts_with(b"BM") {
        "BMP (convert it to PNG first)".to_owned()
    } else if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        "TIFF (convert it to PNG or JPEG first)".to_owned()
    } else if head.starts_with(b"%PDF-") {
        "PDF".to_owned()
    } else if bytes.is_empty() {
        "an empty file".to_owned()
    } else {
        "not a PNG, JPEG, GIF or WebP image".to_owned()
    }
}

pub(crate) fn malformed(format: ImageFormat, message: &str) -> ImageError {
    ImageError::Malformed { format, message: message.to_owned() }
}

pub(crate) fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let pair = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([*pair.first()?, *pair.get(1)?]))
}

pub(crate) fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let quad = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([*quad.first()?, *quad.get(1)?, *quad.get(2)?, *quad.get(3)?]))
}

pub(crate) fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let pair = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([*pair.first()?, *pair.get(1)?]))
}

pub(crate) fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let quad = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([*quad.first()?, *quad.get(1)?, *quad.get(2)?, *quad.get(3)?]))
}

fn le_u24(bytes: &[u8], at: usize) -> Option<u32> {
    let triple = bytes.get(at..at.checked_add(3)?)?;
    Some(u32::from_le_bytes([*triple.first()?, *triple.get(1)?, *triple.get(2)?, 0]))
}

/// One PNG chunk: its type and where its data is.
pub(crate) struct PngChunk<'a> {
    pub kind: &'a [u8],
    /// The whole chunk: length, type, data and CRC.
    pub raw: &'a [u8],
}

/// The chunks of a PNG, up to and including `IEND`. `Err` on a chunk that is cut short.
pub(crate) fn png_chunks(bytes: &[u8]) -> impl Iterator<Item = Result<PngChunk<'_>, ImageError>> {
    let mut at = 8usize;
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let cut = || Some(Err(malformed(ImageFormat::Png, "a chunk is cut short")));
        let Some(length) = be_u32(bytes, at) else {
            done = true;
            return cut();
        };
        let Some(end) = usize::try_from(length).ok().and_then(|length| at.checked_add(12)?.checked_add(length)) else {
            done = true;
            return cut();
        };
        let (Some(raw), Some(kind)) = (bytes.get(at..end), bytes.get(at + 4..at + 8)) else {
            done = true;
            return cut();
        };
        at = end;
        if kind == b"IEND" {
            done = true;
        }
        Some(Ok(PngChunk { kind, raw }))
    })
}

/// PNG chunks that describe how to draw the pixels. Everything else is metadata.
pub(crate) fn png_chunk_is_rendering(kind: &[u8]) -> bool {
    const KEEP: [&[u8; 4]; 18] = [
        b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"sBIT", b"bKGD", b"pHYs",
        b"acTL", b"fcTL", b"fdAT", b"cICP", b"sPLT", b"hIST",
    ];
    KEEP.iter().any(|keep| keep.as_slice() == kind)
}

fn probe_png(bytes: &[u8]) -> Result<ImageInfo, ImageError> {
    let format = ImageFormat::Png;
    let header = png_chunks(bytes).next().ok_or_else(|| malformed(format, "no header chunk"))??;
    if header.kind != b"IHDR" || header.raw.len() != 25 {
        return Err(malformed(format, "the first chunk is not a valid IHDR"));
    }
    let (Some(width), Some(height)) = (be_u32(header.raw, 8), be_u32(header.raw, 12)) else {
        return Err(malformed(format, "the header is cut short"));
    };
    // The header only needs the chunks before the pixel data.
    let mut has_metadata = false;
    let mut animated = false;
    for chunk in png_chunks(bytes) {
        let Ok(chunk) = chunk else { break };
        if chunk.kind == b"IDAT" {
            break;
        }
        animated |= chunk.kind == b"acTL";
        has_metadata |= !png_chunk_is_rendering(chunk.kind);
    }
    Ok(ImageInfo { format, width, height, has_metadata, orientation: None, animated })
}

/// One JPEG segment before the pixel data.
pub(crate) struct JpegSegment<'a> {
    pub marker: u8,
    /// The segment data, after the two length bytes.
    pub data: &'a [u8],
    /// The whole segment: marker, length and data.
    pub raw: &'a [u8],
}

/// The segments of a JPEG from the start up to (not including) the first
/// start of scan. The second value is the offset of that start of scan.
pub(crate) fn jpeg_header_segments(bytes: &[u8]) -> Result<(Vec<JpegSegment<'_>>, usize), ImageError> {
    let format = ImageFormat::Jpeg;
    let mut segments = Vec::new();
    let mut at = 2usize;
    loop {
        // Fill bytes (0xFF) may precede a marker.
        while bytes.get(at) == Some(&0xff) && bytes.get(at + 1) == Some(&0xff) {
            at += 1;
        }
        let (Some(0xff), Some(&marker)) = (bytes.get(at).copied(), bytes.get(at + 1)) else {
            return Err(malformed(format, "no image data after the header"));
        };
        match marker {
            // Start of scan: the pixel data follows.
            0xda => return Ok((segments, at)),
            0xd9 => return Err(malformed(format, "the image ends before its pixel data")),
            // Markers without a length.
            0x01 | 0xd0..=0xd7 => at += 2,
            _ => {
                let length =
                    usize::from(be_u16(bytes, at + 2).ok_or_else(|| malformed(format, "a segment is cut short"))?);
                let end = at + 2 + length;
                let (true, Some(raw), Some(data)) = (length >= 2, bytes.get(at..end), bytes.get(at + 4..end)) else {
                    return Err(malformed(format, "a segment is cut short"));
                };
                segments.push(JpegSegment { marker, data, raw });
                at = end;
            }
        }
    }
}

/// JPEG segments that only carry metadata: EXIF and XMP (APP1), IPTC and
/// Photoshop data (APP13), and comments.
pub(crate) fn jpeg_segment_is_metadata(marker: u8) -> bool {
    matches!(marker, 0xe1 | 0xed | 0xfe)
}

fn probe_jpeg(bytes: &[u8]) -> Result<ImageInfo, ImageError> {
    let format = ImageFormat::Jpeg;
    let (segments, _) = jpeg_header_segments(bytes)?;
    let mut size = None;
    let mut orientation = None;
    let mut has_metadata = false;
    for segment in &segments {
        has_metadata |= jpeg_segment_is_metadata(segment.marker);
        // Start of frame markers (baseline, progressive...) hold the size.
        let is_frame = matches!(segment.marker, 0xc0..=0xcf) && !matches!(segment.marker, 0xc4 | 0xc8 | 0xcc);
        if is_frame && size.is_none() {
            let (Some(height), Some(width)) = (be_u16(segment.data, 1), be_u16(segment.data, 3)) else {
                return Err(malformed(format, "the frame header is cut short"));
            };
            size = Some((u32::from(width), u32::from(height)));
        }
        if segment.marker == 0xe1 && orientation.is_none() {
            orientation = exif_orientation(segment.data);
        }
    }
    let (width, height) = size.ok_or_else(|| malformed(format, "no frame header"))?;
    Ok(ImageInfo { format, width, height, has_metadata, orientation, animated: false })
}

/// The orientation tag (0x0112) of an EXIF block, when it holds a valid value.
fn exif_orientation(app1: &[u8]) -> Option<u8> {
    let tiff = app1.strip_prefix(b"Exif\0\0")?;
    let big_endian = match tiff.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let u16_at = |at: usize| if big_endian { be_u16(tiff, at) } else { le_u16(tiff, at) };
    let u32_at = |at: usize| if big_endian { be_u32(tiff, at) } else { le_u32(tiff, at) };
    let directory = usize::try_from(u32_at(4)?).ok()?;
    let entries = usize::from(u16_at(directory)?);
    // An IFD entry is 12 bytes: tag, type, count, value.
    for index in 0..entries.min(256) {
        let entry = directory.checked_add(2)?.checked_add(index.checked_mul(12)?)?;
        if u16_at(entry)? == 0x0112 {
            let value = u16_at(entry.checked_add(8)?)?;
            return u8::try_from(value).ok().filter(|value| (1..=8).contains(value));
        }
    }
    None
}

/// One block of a GIF after its header.
pub(crate) struct GifBlock<'a> {
    /// The whole block, as stored.
    pub raw: &'a [u8],
    /// Whether the block is needed to draw the image (pixels, timing, the loop
    /// count). Comments and application data such as XMP are not.
    pub rendering: bool,
}

/// The parts of a GIF.
pub(crate) struct GifLayout<'a> {
    /// Signature, logical screen descriptor and global colour table.
    pub header: &'a [u8],
    pub blocks: Vec<GifBlock<'a>>,
    /// Whether the trailer was reached. `false` for a file that is cut short.
    pub complete: bool,
}

/// Walk the blocks of a GIF. A file that is cut short gives the blocks before
/// the cut and `complete: false`. `Err` when even the header is missing.
pub(crate) fn gif_layout(bytes: &[u8]) -> Result<GifLayout<'_>, ImageError> {
    let cut = || malformed(ImageFormat::Gif, "the header is cut short");
    let flags = *bytes.get(10).ok_or_else(cut)?;
    let mut at = 13 + gif_color_table_len(flags);
    let header = bytes.get(..at).ok_or_else(cut)?;
    let mut blocks = Vec::new();
    let complete = loop {
        let (end, rendering) = match bytes.get(at) {
            // Trailer.
            Some(0x3b) => break true,
            // Extension: introducer, label, then data sub-blocks.
            Some(0x21) => {
                let (Some(&label), Some(end)) = (bytes.get(at + 1), skip_gif_sub_blocks(bytes, at + 2)) else {
                    break false;
                };
                let name = bytes.get(at + 3..at + 14);
                let keeps_animation = label == 0xff && matches!(name, Some(b"NETSCAPE2.0" | b"ANIMEXTS1.0"));
                // 0xF9 is the graphic control extension (timing, transparency).
                (end, label == 0xf9 || keeps_animation)
            }
            // Image descriptor, optional colour table, LZW code size, data sub-blocks.
            Some(0x2c) => {
                let Some(&image_flags) = bytes.get(at + 9) else { break false };
                let data_start = at + 10 + gif_color_table_len(image_flags) + 1;
                let Some(end) = skip_gif_sub_blocks(bytes, data_start) else { break false };
                (end, true)
            }
            _ => break false,
        };
        let Some(raw) = bytes.get(at..end) else { break false };
        blocks.push(GifBlock { raw, rendering });
        at = end;
    };
    Ok(GifLayout { header, blocks, complete })
}

/// Bytes of a GIF colour table announced by a flags byte (0 when absent).
fn gif_color_table_len(flags: u8) -> usize {
    if flags & 0x80 == 0 { 0 } else { 3 * (1usize << ((flags & 0x07) + 1)) }
}

/// The position after a run of GIF data sub-blocks that starts at `at`.
fn skip_gif_sub_blocks(bytes: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let size = usize::from(*bytes.get(at)?);
        at = at.checked_add(1 + size)?;
        if size == 0 {
            return Some(at);
        }
    }
}

fn probe_gif(bytes: &[u8]) -> Result<ImageInfo, ImageError> {
    let format = ImageFormat::Gif;
    let (Some(width), Some(height)) = (le_u16(bytes, 6), le_u16(bytes, 8)) else {
        return Err(malformed(format, "the header is cut short"));
    };
    // Comment and application extensions can hold text and XMP. A file given
    // only in part (a header window) is judged by the blocks that are there.
    let has_metadata = gif_layout(bytes).is_ok_and(|layout| layout.blocks.iter().any(|block| !block.rendering));
    Ok(ImageInfo {
        format,
        width: u32::from(width),
        height: u32::from(height),
        has_metadata,
        orientation: None,
        animated: false,
    })
}

/// One RIFF chunk of a WebP file.
pub(crate) struct WebpChunk<'a> {
    pub kind: &'a [u8],
    pub data: &'a [u8],
    /// The whole chunk: type, size, data and the padding byte if any.
    pub raw: &'a [u8],
}

/// The chunks of a WebP file. `Err` on a chunk that is cut short.
pub(crate) fn webp_chunks(bytes: &[u8]) -> impl Iterator<Item = Result<WebpChunk<'_>, ImageError>> {
    // Chunks live inside the RIFF size, which may be smaller than the file.
    let declared =
        le_u32(bytes, 4).and_then(|size| usize::try_from(size).ok()).map_or(0, |size| size.saturating_add(8));
    let limit = declared.min(bytes.len());
    let mut at = 12usize;
    let mut done = false;
    std::iter::from_fn(move || {
        if done || at >= limit {
            return None;
        }
        let cut = || Some(Err(malformed(ImageFormat::WebP, "a chunk is cut short")));
        let Some(size) = le_u32(bytes, at + 4).and_then(|size| usize::try_from(size).ok()) else {
            done = true;
            return cut();
        };
        let Some(data_end) = at.checked_add(8).and_then(|start| start.checked_add(size)) else {
            done = true;
            return cut();
        };
        if data_end > limit {
            done = true;
            return cut();
        }
        // Chunks are padded to an even size (the last one may lack its padding byte).
        let end = (data_end + (size & 1)).min(limit);
        let (Some(kind), Some(data), Some(raw)) =
            (bytes.get(at..at + 4), bytes.get(at + 8..data_end), bytes.get(at..end))
        else {
            done = true;
            return cut();
        };
        at = end;
        Some(Ok(WebpChunk { kind, data, raw }))
    })
}

/// WebP chunks that carry metadata.
pub(crate) fn webp_chunk_is_metadata(kind: &[u8]) -> bool {
    !matches!(kind, b"VP8X" | b"VP8 " | b"VP8L" | b"ALPH" | b"ANIM" | b"ANMF" | b"ICCP")
}

fn probe_webp(bytes: &[u8]) -> Result<ImageInfo, ImageError> {
    let format = ImageFormat::WebP;
    let mut size = None;
    let mut has_metadata = false;
    let mut animated = false;
    for chunk in webp_chunks(bytes) {
        let chunk = chunk?;
        has_metadata |= webp_chunk_is_metadata(chunk.kind);
        let found = match chunk.kind {
            b"VP8X" => {
                let flags = chunk.data.first().copied().unwrap_or(0);
                animated |= flags & 0x02 != 0;
                has_metadata |= flags & 0x0c != 0;
                match (le_u24(chunk.data, 4), le_u24(chunk.data, 7)) {
                    (Some(width), Some(height)) => Some((width + 1, height + 1)),
                    _ => return Err(malformed(format, "the extended header is cut short")),
                }
            }
            b"VP8 " => {
                // Frame tag (3 bytes), start code 9D 01 2A, then 14-bit width and height.
                if chunk.data.get(3..6) != Some(&[0x9d, 0x01, 0x2a]) {
                    return Err(malformed(format, "the lossy frame has no start code"));
                }
                match (le_u16(chunk.data, 6), le_u16(chunk.data, 8)) {
                    (Some(width), Some(height)) => Some((u32::from(width & 0x3fff), u32::from(height & 0x3fff))),
                    _ => return Err(malformed(format, "the lossy frame header is cut short")),
                }
            }
            b"VP8L" => {
                // Signature 0x2F, then 14 bits of width - 1 and 14 bits of height - 1.
                if chunk.data.first() != Some(&0x2f) {
                    return Err(malformed(format, "the lossless frame has no signature"));
                }
                let bits =
                    le_u32(chunk.data, 1).ok_or_else(|| malformed(format, "the lossless header is cut short"))?;
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            _ => None,
        };
        // The first size wins: VP8X is the canvas, and comes first when present.
        size = size.or(found);
    }
    let (width, height) = size.ok_or_else(|| malformed(format, "no image data"))?;
    Ok(ImageInfo { format, width, height, has_metadata, orientation: None, animated })
}
