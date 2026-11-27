//! HEIC input (cargo feature `heic`): the photos phones take by default.
//!
//! A HEIC file is never passed on. It is read, checked and decoded here, and
//! what leaves is a JPEG or PNG encoded by this crate, so no provider and no
//! tool downstream has to understand the format.
//!
//! The header is read by a parser of our own before the decoder sees the
//! file: the largest image size any `ispe` property declares is held against
//! the pixel limits, so a small file that declares a huge canvas is refused
//! without decoding. The decoder ([`heifer`], safe Rust) gets the same pixel
//! limit, runs on one thread behind a panic boundary, and its output is
//! checked again.
//!
//! The feature is off by default. HEVC, the codec inside HEIC, is covered by
//! patent pools whatever the licence of the software is, so whether to ship a
//! decoder is a decision for whoever builds the binary.

use std::panic::{AssertUnwindSafe, catch_unwind};

use image::{DynamicImage, RgbaImage};

use crate::error::{ImageError, ImageLimits};
use crate::probe::{ImageFormat, ImageInfo, be_u32, malformed};

/// Boxes looked at per level of the container before giving up.
const MAX_BOXES_PER_LEVEL: usize = 4_096;
/// Brands of files whose images are HEVC coded.
const HEIC_BRANDS: [&[u8; 4]; 8] = [b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs"];

/// Whether `bytes` start like a HEIC file: an `ftyp` box that names an HEVC
/// brand and no AV1 one.
pub(crate) fn is_heic(bytes: &[u8]) -> bool {
    if bytes.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let size = be_u32(bytes, 0).and_then(|size| usize::try_from(size).ok()).unwrap_or(0);
    let ftyp = bytes.get(8..size.clamp(8, bytes.len().min(512))).unwrap_or_default();
    // Major brand, minor version, then compatible brands, four bytes each.
    let brands = || ftyp.chunks_exact(4).enumerate().filter(|(index, _)| *index != 1).map(|(_, brand)| brand);
    let is = |wanted: &[u8; 4]| brands().any(|brand| brand == wanted);
    HEIC_BRANDS.iter().any(|brand| is(brand)) && !is(b"avif") && !is(b"avis")
}

/// The boxes directly inside `bytes`: their type and content.
fn boxes(bytes: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let size = usize::try_from(be_u32(bytes, at)?).ok()?;
        let kind = bytes.get(at.checked_add(4)?..at.checked_add(8)?)?;
        let (header, size) = match size {
            // The box runs to the end of its parent.
            0 => (8, bytes.len().checked_sub(at)?),
            // A 64 bit size follows the type.
            1 => {
                let high = be_u32(bytes, at.checked_add(8)?)?;
                let low = usize::try_from(be_u32(bytes, at.checked_add(12)?)?).ok()?;
                if high != 0 {
                    return None;
                }
                (16, low)
            }
            size => (8, size),
        };
        if size < header {
            return None;
        }
        let end = at.checked_add(size)?;
        let content = bytes.get(at.checked_add(header)?..end)?;
        at = end;
        Some((kind, content))
    })
    .take(MAX_BOXES_PER_LEVEL)
}

fn child<'a>(bytes: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(bytes).find(|(found, _)| found == kind).map(|(_, content)| content)
}

/// What the container says about the image, without decoding anything.
///
/// The size is the largest one any image of the file declares (the photo
/// itself, not its tiles or its thumbnail), which is what the limits must hold
/// against.
pub(crate) fn probe_heic(bytes: &[u8]) -> Result<ImageInfo, ImageError> {
    let cut_short = || malformed(ImageFormat::Heic, "the file is cut short or has no image properties");
    // `meta` is a full box: four bytes of version and flags before its children.
    let meta = child(bytes, b"meta").and_then(|meta| meta.get(4..)).ok_or_else(cut_short)?;
    let properties = child(meta, b"iprp").and_then(|iprp| child(iprp, b"ipco")).ok_or_else(cut_short)?;
    let largest = boxes(properties)
        .filter(|(kind, _)| kind == b"ispe")
        .filter_map(|(_, ispe)| Some((be_u32(ispe, 4)?, be_u32(ispe, 8)?)))
        .max_by_key(|(width, height)| u64::from(*width) * u64::from(*height));
    let (width, height) = largest.ok_or_else(cut_short)?;
    Ok(ImageInfo {
        format: ImageFormat::Heic,
        width,
        height,
        // Phone photos carry EXIF. Nothing of the container survives decoding either way.
        has_metadata: true,
        // Rotation and mirroring are properties the decoder applies.
        orientation: None,
        animated: false,
    })
}

/// Decode the primary image of a HEIC file, upright, as 8 bit pixels.
pub(crate) fn decode_heic(bytes: &[u8], limits: &ImageLimits) -> Result<DynamicImage, ImageError> {
    let options = heifer::Options::new().with_max_threads(1).with_max_pixels(limits.max_pixels.max(1));
    let decoded = catch_unwind(AssertUnwindSafe(|| heifer::decode_srgb_rgba8(bytes, &options)));
    let image = match decoded {
        Ok(Ok(image)) => image,
        Ok(Err(e)) => return Err(malformed(ImageFormat::Heic, &e.to_string())),
        Err(_) => return Err(malformed(ImageFormat::Heic, "the decoder failed on this file")),
    };
    // The decoder is the authority on what it produced: check it like the header.
    limits.check_dimensions(image.width, image.height)?;
    let has_alpha = image.has_alpha;
    let pixels = RgbaImage::from_raw(image.width, image.height, image.data)
        .ok_or_else(|| malformed(ImageFormat::Heic, "the decoder returned an incomplete image"))?;
    let image = DynamicImage::ImageRgba8(pixels);
    Ok(if has_alpha { image } else { DynamicImage::ImageRgb8(image.to_rgb8()) })
}
