//! # graphrt-image
//!
//! Image handling for graphrt agents. Every image that reaches a model or a
//! tool goes through the same gate:
//!
//! 1. [`probe`] reads format and pixel size from the header. Only PNG, JPEG,
//!    GIF and WebP pass, judged by magic number, and an image that declares
//!    too many pixels is refused before any decoder runs.
//! 2. [`strip_metadata`] cuts EXIF (GPS position, device, time), XMP, IPTC,
//!    text chunks and comments out of the container. No decoding, no loss.
//! 3. When an image is too large for a model, it is decoded with hard limits,
//!    rotated upright, downscaled and encoded again (feature `process`).
//!
//! | Piece | What it does |
//! | --- | --- |
//! | [`prepare_for_model`] | Steps 1 to 3 for one image |
//! | [`VisionPreparer`] | The same for the `vision` inputs of model nodes ([`graphrt_core::file::ImagePreparer`]) |
//! | `ImageTools` (feature `tools`) | The `image` tools over a `graphrt-files` workspace: `info`, `view`, `resize`, `convert`, `thumbnail`, `generate` |
//! | [`ImageGenerator`] | The contract of an image generation provider. `OpenAiImages` (feature `generate-openai`) is one. |
//!
//! Nothing is on unless a host registers it.

mod error;
mod generate;
#[cfg(feature = "heic")]
mod heic;
mod metadata;
mod prepare;
mod probe;
#[cfg(feature = "process")]
mod process;
#[cfg(feature = "tools")]
mod tools;

pub use error::{ImageError, ImageLimits};
#[cfg(feature = "generate-openai")]
pub use generate::OpenAiImages;
pub use generate::{GenerateRequest, GeneratedImage, ImageGenerator};
pub use metadata::strip_metadata;
pub use prepare::{Change, ModelImage, VisionPreparer, prepare_for_model};
pub use probe::{ImageFormat, ImageInfo, probe, sniff_format};
#[cfg(feature = "process")]
pub use process::{Processed, Transform, transform};
#[cfg(feature = "tools")]
pub use tools::{ImageTools, ImageToolsConfig, PROVIDER};

/// Run CPU-heavy work off the async workers: on Tokio's blocking pool when
/// the `tokio` feature is on and a runtime is present, inline otherwise.
/// `Err` carries the message of a task that panicked or was cancelled.
pub async fn blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    #[cfg(feature = "tokio")]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return handle.spawn_blocking(work).await.map_err(|e| format!("background task failed: {e}"));
    }
    Ok(work())
}
