//! Decoding, resizing and encoding, behind the header check of [`crate::probe`].
//!
//! Everything here is CPU work on buffers that can be large. Callers run it
//! through [`crate::blocking`], never on an async worker.

use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits, RgbImage};

use crate::error::{ImageError, ImageLimits};
use crate::probe::{ImageFormat, ImageInfo, malformed, probe};

/// Extra bytes a decoder may allocate beside the pixels (tables, row buffers).
const DECODER_OVERHEAD_BYTES: u64 = 16 * 1024 * 1024;
/// How often the model image is shrunk further when it does not fit the byte limit.
const MAX_SHRINK_STEPS: u32 = 5;

/// What to do with an image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Transform {
    /// Largest width of the result. The aspect ratio is kept.
    pub max_width: Option<u32>,
    /// Largest height of the result. The aspect ratio is kept.
    pub max_height: Option<u32>,
    /// Format of the result. `None` keeps PNG for images with transparency and uses JPEG otherwise.
    pub format: Option<ImageFormat>,
}

/// An image after [`transform`] or [`crate::prepare_for_model`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Processed {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
}

fn codec(format: ImageFormat) -> image::ImageFormat {
    match format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::WebP => image::ImageFormat::WebP,
        // Never reached: HEIC is decoded by `crate::heic` and never written.
        ImageFormat::Heic => image::ImageFormat::Jpeg,
    }
}

/// Decode `bytes` (already probed as `info`) into upright pixels.
///
/// The decoder runs with hard limits on width and height and a cap on what it
/// may allocate, behind a panic boundary: a decoder bug on a crafted file is
/// an error for that file, not a crash of the runtime. An animation decodes
/// to its first frame.
pub(crate) fn decode(bytes: &[u8], info: &ImageInfo, limits: &ImageLimits) -> Result<DynamicImage, ImageError> {
    limits.check_dimensions(info.width, info.height)?;
    let format = info.format;
    #[cfg(feature = "heic")]
    if format == ImageFormat::Heic {
        return crate::heic::decode_heic(bytes, limits);
    }
    let decoded = catch_unwind(AssertUnwindSafe(|| {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), codec(format));
        let mut bounds = Limits::default();
        bounds.max_image_width = Some(limits.max_dimension);
        bounds.max_image_height = Some(limits.max_dimension);
        bounds.max_alloc = Some(limits.max_pixels.saturating_mul(4).saturating_add(DECODER_OVERHEAD_BYTES));
        reader.limits(bounds);
        let mut decoder = reader.into_decoder()?;
        // The header was already checked, but the decoder is the authority on what it will produce.
        let (width, height) = decoder.dimensions();
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
        let mut image = if u64::from(width) * u64::from(height) > limits.max_pixels {
            return Ok(None);
        } else {
            DynamicImage::from_decoder(decoder)?
        };
        image.apply_orientation(orientation);
        Ok::<_, image::ImageError>(Some(image))
    }));
    match decoded {
        Ok(Ok(Some(image))) => Ok(image),
        Ok(Ok(None)) => Err(ImageError::TooManyPixels {
            width: info.width,
            height: info.height,
            reason: format!("at most {} pixels in total", limits.max_pixels),
        }),
        Ok(Err(e)) => Err(malformed(format, &e.to_string())),
        Err(_) => Err(malformed(format, "the decoder failed on this file")),
    }
}

/// The size that fits `width`x`height` into the given bounds, keeping the
/// aspect ratio. Never larger than the original, never zero.
pub(crate) fn fit(width: u32, height: u32, max_width: Option<u32>, max_height: Option<u32>) -> (u32, u32) {
    let max_width = max_width.unwrap_or(width).clamp(1, width.max(1));
    let max_height = max_height.unwrap_or(height).clamp(1, height.max(1));
    if width <= max_width && height <= max_height {
        return (width, height);
    }
    // Scale by the tighter of the two bounds, in integers.
    let (w, h) = (u64::from(width), u64::from(height));
    let by_width = (u64::from(max_width), (h * u64::from(max_width)).div_ceil(w.max(1)));
    let (new_width, new_height) = if by_width.1 <= u64::from(max_height) {
        by_width
    } else {
        ((w * u64::from(max_height)).div_ceil(h.max(1)), u64::from(max_height))
    };
    let clamp = |value: u64, max: u32| u32::try_from(value).unwrap_or(max).clamp(1, max);
    (clamp(new_width, max_width), clamp(new_height, max_height))
}

/// Downscale to exactly `width`x`height`.
///
/// A reduction to half or less averages the source pixels that fall on each
/// target pixel (one pass over the source, no ringing, nothing aliased). That
/// is several times faster than a bicubic pass, whose kernel grows with the
/// reduction, and it is what a model needs from a 12 megapixel photo. Smaller
/// reductions use bicubic, where averaging would blur.
fn downscale(image: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    let halves = image.width() / width.max(1) >= 2 && image.height() / height.max(1) >= 2;
    if halves {
        return image.thumbnail_exact(width, height);
    }
    image.resize_exact(width, height, FilterType::CatmullRom)
}

/// Pixels without transparency, composed over white (JPEG has no alpha).
fn flatten(image: &DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    let mut rgb = RgbImage::new(rgba.width(), rgba.height());
    for (target, source) in rgb.pixels_mut().zip(rgba.pixels()) {
        let alpha = u32::from(source.0[3]);
        let blend = |channel: u8| {
            let value = (u32::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255;
            u8::try_from(value).unwrap_or(u8::MAX)
        };
        target.0 = [blend(source.0[0]), blend(source.0[1]), blend(source.0[2])];
    }
    rgb
}

/// Encode pixels. The result never carries metadata: the encoders write none.
pub(crate) fn encode(image: &DynamicImage, format: ImageFormat, jpeg_quality: u8) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    let failed = |e: image::ImageError| ImageError::Encode(e.to_string());
    match format {
        ImageFormat::Heic => return Err(ImageError::Encode("HEIC images are read, not written".to_owned())),
        ImageFormat::Jpeg => {
            let rgb = flatten(image);
            JpegEncoder::new_with_quality(&mut out, jpeg_quality.clamp(1, 100)).encode_image(&rgb).map_err(failed)?;
        }
        ImageFormat::Png => {
            // 16-bit and float pixels become 8-bit: models and browsers gain nothing from more.
            let encoder = PngEncoder::new_with_quality(&mut out, CompressionType::Fast, PngFilter::Adaptive);
            eight_bit(image).write_with_encoder(encoder).map_err(failed)?;
        }
        ImageFormat::WebP => {
            eight_bit(image).write_with_encoder(WebPEncoder::new_lossless(&mut out)).map_err(failed)?;
        }
        ImageFormat::Gif => {
            let mut cursor = Cursor::new(&mut out);
            DynamicImage::ImageRgba8(image.to_rgba8())
                .write_to(&mut cursor, image::ImageFormat::Gif)
                .map_err(failed)?;
        }
    }
    Ok(out)
}

/// The image as 8-bit RGB or RGBA.
fn eight_bit(image: &DynamicImage) -> DynamicImage {
    if image.color().has_alpha() {
        DynamicImage::ImageRgba8(image.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
    }
}

/// Decode `bytes`, fit them into the bounds of `transform` and encode the result.
///
/// The output is always written by this crate's encoders, so it has no
/// metadata and nothing after its end, whatever the input carried.
pub fn transform(bytes: &[u8], limits: &ImageLimits, transform: Transform) -> Result<Processed, ImageError> {
    limits.check_bytes(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
    let info = probe(bytes)?;
    let image = decode(bytes, &info, limits)?;
    let (width, height) = fit(image.width(), image.height(), transform.max_width, transform.max_height);
    let image =
        if (width, height) == (image.width(), image.height()) { image } else { downscale(&image, width, height) };
    let format =
        transform.format.unwrap_or(if image.color().has_alpha() { ImageFormat::Png } else { ImageFormat::Jpeg });
    let bytes = encode(&image, format, limits.jpeg_quality)?;
    Ok(Processed { bytes, format, width, height })
}

/// Re-encode an image for a model: upright, at most `model_max_edge` on the
/// long side and at most `model_max_bytes`.
pub(crate) fn reencode_for_model(
    bytes: &[u8],
    info: &ImageInfo,
    limits: &ImageLimits,
) -> Result<Processed, ImageError> {
    let image = decode(bytes, info, limits)?;
    let mut edge = limits.model_max_edge.max(1);
    let has_alpha = image.color().has_alpha();
    for _ in 0..=MAX_SHRINK_STEPS {
        let (width, height) = fit(image.width(), image.height(), Some(edge), Some(edge));
        let sized = if (width, height) == (image.width(), image.height()) {
            None
        } else {
            Some(downscale(&image, width, height))
        };
        let sized = sized.as_ref().unwrap_or(&image);
        // Transparency needs PNG. When that is too large, a JPEG over white is the next best thing.
        let formats: &[ImageFormat] =
            if has_alpha { &[ImageFormat::Png, ImageFormat::Jpeg] } else { &[ImageFormat::Jpeg] };
        for format in formats {
            let encoded = encode(sized, *format, limits.jpeg_quality)?;
            if u64::try_from(encoded.len()).unwrap_or(u64::MAX) <= limits.model_max_bytes {
                return Ok(Processed { bytes: encoded, format: *format, width, height });
            }
        }
        edge = (edge / 4 * 3).max(1);
    }
    Err(ImageError::Encode(format!(
        "the image does not fit in {} bytes even after shrinking it",
        limits.model_max_bytes
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_the_aspect_ratio_and_never_enlarges() {
        assert_eq!(fit(4000, 3000, Some(1568), Some(1568)), (1568, 1176));
        assert_eq!(fit(3000, 4000, Some(1568), Some(1568)), (1176, 1568));
        assert_eq!(fit(800, 600, Some(1568), Some(1568)), (800, 600));
        assert_eq!(fit(800, 600, None, None), (800, 600));
        assert_eq!(fit(800, 600, Some(400), None), (400, 300));
        assert_eq!(fit(800, 600, None, Some(60)), (80, 60));
        // Extreme shapes never collapse to zero.
        assert_eq!(fit(10_000, 1, Some(100), Some(100)), (100, 1));
        assert_eq!(fit(1, 10_000, Some(100), Some(100)), (1, 100));
        assert_eq!(fit(800, 600, Some(0), Some(0)), (1, 1));
        assert_eq!(fit(800, 600, Some(5000), Some(5000)), (800, 600));
    }
}
