//! Errors and limits of image handling.

use crate::probe::ImageFormat;

/// Why an image was refused or could not be processed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImageError {
    /// The content is not a PNG, JPEG, GIF or WebP image.
    #[error("unsupported image: {0}")]
    UnsupportedFormat(String),
    /// The header or the pixel data is damaged.
    #[error("malformed {} image: {message}", .format.mime())]
    Malformed { format: ImageFormat, message: String },
    /// The file is larger than [`ImageLimits::max_input_bytes`].
    #[error("the image is {size} bytes, above the limit of {limit} bytes")]
    TooManyBytes { size: u64, limit: u64 },
    /// The canvas is larger than [`ImageLimits::max_pixels`] or [`ImageLimits::max_dimension`].
    #[error("the image is {width}x{height} pixels, above the limit ({reason})")]
    TooManyPixels { width: u32, height: u32, reason: String },
    /// The image must be resized or re-encoded and this build cannot do it.
    #[error("the image needs to be {needed}, and this build has no image processing (feature `process`)")]
    ProcessingUnavailable { needed: String },
    /// Encoding the result failed, or it cannot be made small enough.
    #[error("cannot encode the image: {0}")]
    Encode(String),
    /// An invalid request (bad size, unknown format name...).
    #[error("{0}")]
    Invalid(String),
    /// The image generator failed.
    #[error("image generation failed: {0}")]
    Generation(String),
}

/// Bounds on the images the runtime accepts and on what it sends to a model.
///
/// The defaults fit every built-in provider: Anthropic takes 10 MB of base64
/// per image on its API and 5 MB on Bedrock, and downscales above 1,568 px on
/// the long edge anyway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageLimits {
    /// Largest image file that is read at all.
    pub max_input_bytes: u64,
    /// Most pixels (width times height) an image may declare. Checked from the
    /// header, before any decoder runs. Decoding needs about 4 bytes per pixel.
    pub max_pixels: u64,
    /// Longest side an image may declare, in pixels.
    pub max_dimension: u32,
    /// Longest side of an image sent to a model. Larger images are downscaled.
    pub model_max_edge: u32,
    /// Largest image sent to a model, in bytes before base64. Larger images
    /// are encoded again (and downscaled further if needed).
    pub model_max_bytes: u64,
    /// Remove EXIF, XMP, IPTC, text chunks and comments from images before
    /// they leave the runtime.
    pub strip_metadata: bool,
    /// JPEG quality (1 to 100) for images this crate encodes.
    pub jpeg_quality: u8,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 20 * 1024 * 1024,
            max_pixels: 25_000_000,
            max_dimension: 12_000,
            model_max_edge: 1_568,
            // 3.7 MB is under 5 MB once base64 encoded (Bedrock, tool result images).
            model_max_bytes: 3_700_000,
            strip_metadata: true,
            jpeg_quality: 85,
        }
    }
}

impl ImageLimits {
    /// Refuse a file that is too large to read.
    pub fn check_bytes(&self, size: u64) -> Result<(), ImageError> {
        if size > self.max_input_bytes {
            return Err(ImageError::TooManyBytes { size, limit: self.max_input_bytes });
        }
        Ok(())
    }

    /// Refuse a canvas that is too large to decode.
    pub fn check_dimensions(&self, width: u32, height: u32) -> Result<(), ImageError> {
        let too_many = |reason: String| ImageError::TooManyPixels { width, height, reason };
        if width > self.max_dimension || height > self.max_dimension {
            return Err(too_many(format!("at most {} pixels per side", self.max_dimension)));
        }
        if u64::from(width) * u64::from(height) > self.max_pixels {
            return Err(too_many(format!("at most {} pixels in total", self.max_pixels)));
        }
        Ok(())
    }
}
