//! Getting an image ready for a model.

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::file::{FileError, FileResolver, ImagePreparer, base64_encode};
use graphrt_core::value::File;

use crate::error::{ImageError, ImageLimits};
use crate::metadata::strip_metadata;
use crate::probe::{ImageFormat, ImageInfo, probe};

/// An image a model can take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelImage {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
    /// What was done to the original.
    pub change: Change,
}

/// What [`prepare_for_model`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// The bytes are the original bytes.
    Unchanged,
    /// Metadata was removed. The pixels are untouched.
    MetadataRemoved,
    /// The image was decoded and encoded again (resized, rotated or made smaller).
    Reencoded,
}

impl ModelImage {
    /// `data:<media type>;base64,<data>`.
    pub fn to_data_url(&self) -> String {
        format!("data:{};base64,{}", self.format.mime(), base64_encode(&self.bytes))
    }
}

/// Check `bytes` and make them fit for a model.
///
/// 1. Refuse a file above [`ImageLimits::max_input_bytes`].
/// 2. Read the header: only PNG, JPEG, GIF and WebP pass, and only below the
///    pixel limits. No decoder has run at this point.
/// 3. An upright image within [`ImageLimits::model_max_edge`] and
///    [`ImageLimits::model_max_bytes`] keeps its pixels: metadata is cut out
///    of the container and that is all.
/// 4. Anything else is decoded with hard limits, rotated upright, downscaled
///    and encoded again (feature `process`). Without that feature it is refused.
///
/// CPU work: call it through [`crate::blocking`].
pub fn prepare_for_model(bytes: Vec<u8>, limits: &ImageLimits) -> Result<ModelImage, ImageError> {
    limits.check_bytes(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
    let info = probe(&bytes)?;
    limits.check_dimensions(info.width, info.height)?;

    let fits_edge = info.width.max(info.height) <= limits.model_max_edge;
    if fits_edge && !info.needs_rotation() {
        let (bytes, change) = if limits.strip_metadata {
            // Stripping also cuts off anything appended after the image, so it runs
            // even when the header shows no metadata.
            let stripped = strip_metadata(&bytes)?;
            let change = if stripped.len() == bytes.len() { Change::Unchanged } else { Change::MetadataRemoved };
            (stripped, change)
        } else {
            (bytes.clone(), Change::Unchanged)
        };
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= limits.model_max_bytes {
            return Ok(ModelImage { bytes, format: info.format, width: info.width, height: info.height, change });
        }
    }
    reencode(&bytes, &info, limits)
}

#[cfg(feature = "process")]
fn reencode(bytes: &[u8], info: &ImageInfo, limits: &ImageLimits) -> Result<ModelImage, ImageError> {
    let processed = crate::process::reencode_for_model(bytes, info, limits)?;
    Ok(ModelImage {
        bytes: processed.bytes,
        format: processed.format,
        width: processed.width,
        height: processed.height,
        change: Change::Reencoded,
    })
}

#[cfg(not(feature = "process"))]
fn reencode(bytes: &[u8], info: &ImageInfo, limits: &ImageLimits) -> Result<ModelImage, ImageError> {
    let needed = if info.width.max(info.height) > limits.model_max_edge {
        format!("downscaled from {}x{} to at most {} pixels per side", info.width, info.height, limits.model_max_edge)
    } else if info.needs_rotation() {
        "rotated upright".to_owned()
    } else {
        format!("made smaller than {} bytes (it is {})", limits.model_max_bytes, bytes.len())
    };
    Err(ImageError::ProcessingUnavailable { needed })
}

/// The [`ImagePreparer`] for `vision` inputs: reads each image through a
/// [`FileResolver`], runs [`prepare_for_model`] on the blocking pool and
/// returns a `data:` URL.
///
/// ```
/// use std::sync::Arc;
/// use graphrt_core::file::{ChainResolver, FileResolver, ImagePreparer};
/// use graphrt_image::{ImageLimits, VisionPreparer};
///
/// let files: Arc<dyn FileResolver> = Arc::new(ChainResolver::with_data_urls());
/// let preparer: Arc<dyn ImagePreparer> = Arc::new(VisionPreparer::new(files, ImageLimits::default()));
/// let services = graphrt_core::Services::new().with(preparer);
/// ```
#[derive(Clone, Debug)]
pub struct VisionPreparer {
    resolver: Arc<dyn FileResolver>,
    limits: Arc<ImageLimits>,
    inline_remote: bool,
}

impl VisionPreparer {
    /// A preparer reading through `resolver`.
    ///
    /// Images given as `http(s)` URLs are left to the model provider to fetch
    /// (the runtime never sees their bytes). Use
    /// [`VisionPreparer::inline_remote_images`] to fetch and check them here.
    pub fn new(resolver: Arc<dyn FileResolver>, limits: ImageLimits) -> Self {
        Self { resolver, limits: Arc::new(limits), inline_remote: false }
    }

    /// Also fetch images given as `http(s)` URLs (through the resolver, which
    /// applies the egress policy) and send them inline. Needed for providers
    /// that take no image URLs, and it puts URL images under the same checks
    /// as every other image.
    pub fn inline_remote_images(mut self, inline: bool) -> Self {
        self.inline_remote = inline;
        self
    }

    pub fn limits(&self) -> &ImageLimits {
        &self.limits
    }
}

fn is_remote(file: &File) -> bool {
    file.url.as_deref().is_some_and(|url| {
        let scheme = url.get(..8).unwrap_or(url).to_ascii_lowercase();
        scheme.starts_with("http://") || scheme.starts_with("https://")
    })
}

impl ImagePreparer for VisionPreparer {
    fn prepare<'a>(&'a self, file: &'a File) -> BoxFuture<'a, Result<String, FileError>> {
        Box::pin(async move {
            if is_remote(file) && !self.inline_remote {
                return Err(FileError::Unsupported(file.name.clone()));
            }
            let name = file.name.clone();
            let content = self.resolver.read(file, self.limits.max_input_bytes).await?;
            let limits = Arc::clone(&self.limits);
            // Probe, decode, resize, encode and base64 are all CPU work on large buffers.
            let prepared =
                crate::blocking(move || prepare_for_model(content.bytes, &limits).map(|image| image.to_data_url()))
                    .await;
            match prepared {
                Ok(Ok(url)) => Ok(url),
                Ok(Err(ImageError::TooManyBytes { limit, .. })) => Err(FileError::TooLarge { name, limit }),
                Ok(Err(e)) => Err(FileError::Invalid { name, message: e.to_string() }),
                Err(message) => Err(FileError::Invalid { name, message }),
            }
        })
    }
}
