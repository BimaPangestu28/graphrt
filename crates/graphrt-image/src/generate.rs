//! Image generation behind a trait, so the provider can be swapped.

use std::fmt;

use futures::future::BoxFuture;

use crate::error::ImageError;

/// What to generate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenerateRequest {
    /// Description of the image.
    pub prompt: String,
    /// Size as `WIDTHxHEIGHT` (for example `1024x1024`), when the caller cares.
    pub size: Option<String>,
}

impl GenerateRequest {
    /// Longest prompt accepted, in characters.
    pub const MAX_PROMPT_CHARS: usize = 4_000;

    /// Check the request before it is sent anywhere.
    pub fn validate(&self) -> Result<(), ImageError> {
        let chars = self.prompt.trim().chars().count();
        if chars == 0 {
            return Err(ImageError::Invalid("the prompt is empty".to_owned()));
        }
        if chars > Self::MAX_PROMPT_CHARS {
            return Err(ImageError::Invalid(format!(
                "the prompt is longer than {} characters",
                Self::MAX_PROMPT_CHARS
            )));
        }
        if let Some(size) = &self.size {
            let valid = size.split_once('x').is_some_and(|(width, height)| {
                let side = |text: &str| text.parse::<u32>().is_ok_and(|pixels| (16..=8192).contains(&pixels));
                side(width) && side(height)
            });
            if !valid {
                return Err(ImageError::Invalid(format!("`{size}` is not a size such as 1024x1024")));
            }
        }
        Ok(())
    }
}

/// What a generator returned: bytes that claim to be an image.
///
/// They come from a remote service, so they are untrusted like any upload:
/// callers probe them and apply their limits before storing or showing them
/// (the `image/generate` tool does).
#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
}

impl fmt::Debug for GeneratedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedImage").field("bytes", &self.bytes.len()).finish()
    }
}

/// A service that turns a prompt into an image.
pub trait ImageGenerator: Send + Sync + fmt::Debug {
    /// Generate one image.
    fn generate<'a>(&'a self, request: &'a GenerateRequest) -> BoxFuture<'a, Result<GeneratedImage, ImageError>>;
}

#[cfg(feature = "generate-gemini")]
pub use gemini::GeminiImages;
#[cfg(feature = "generate-openai")]
pub use openai::OpenAiImages;

/// What the generators share: one bounded, keyed HTTP request.
#[cfg(any(feature = "generate-openai", feature = "generate-gemini"))]
mod transport {
    use std::sync::OnceLock;
    use std::time::Duration;

    use crate::error::ImageError;

    /// Largest response body read (base64 of the image plus a little JSON).
    const MAX_RESPONSE_BYTES: usize = 48 * 1024 * 1024;
    /// How much of an error body ends up in an error message.
    const MAX_ERROR_CHARS: usize = 400;

    /// A provider endpoint and its key.
    ///
    /// The base URL and the key come from the operator, never from a workflow
    /// or a model. Redirects are not followed, so the key and the prompt
    /// cannot be carried to another host.
    pub(super) struct Transport {
        pub(super) base_url: String,
        api_key: String,
        pub(super) timeout: Duration,
        client: OnceLock<Result<reqwest::Client, String>>,
    }

    impl Transport {
        pub(super) fn new(base_url: String, api_key: String) -> Self {
            Self {
                base_url: base_url.trim_end_matches('/').to_owned(),
                api_key,
                timeout: Duration::from_secs(180),
                client: OnceLock::new(),
            }
        }

        pub(super) fn api_key(&self) -> &str {
            &self.api_key
        }

        /// The HTTP client, built on first use (loading the trust store costs
        /// start-up time an agent that never generates should not pay).
        pub(super) fn client(&self) -> Result<&reqwest::Client, ImageError> {
            self.client
                .get_or_init(|| {
                    // An error only means a provider is installed already.
                    let _already_installed = rustls::crypto::ring::default_provider().install_default();
                    reqwest::Client::builder()
                        .connect_timeout(Duration::from_secs(10))
                        .redirect(reqwest::redirect::Policy::none())
                        .build()
                        .map_err(|e| e.to_string())
                })
                .as_ref()
                .map_err(|message| ImageError::Generation(format!("cannot build the HTTP client: {message}")))
        }

        /// `text` without the key, cut to what an error message may hold.
        pub(super) fn scrub(&self, text: &str) -> String {
            let scrubbed =
                if self.api_key.len() >= 4 { text.replace(&self.api_key, "******") } else { text.to_owned() };
            scrubbed.chars().take(MAX_ERROR_CHARS).collect()
        }

        /// Send a prepared request and parse the JSON answer of a success.
        ///
        /// @throws ImageError::Generation - A connection failure, an answer above the size limit, an error
        ///   status (with the scrubbed body) or an answer that is not JSON.
        pub(super) async fn send(&self, request: reqwest::RequestBuilder) -> Result<serde_json::Value, ImageError> {
            let failed = |message: String| ImageError::Generation(message);
            let mut response = request.send().await.map_err(|e| failed(self.scrub(&e.without_url().to_string())))?;
            let status = response.status();
            let mut raw = Vec::new();
            while let Some(chunk) =
                response.chunk().await.map_err(|e| failed(self.scrub(&e.without_url().to_string())))?
            {
                if raw.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(failed(format!("the response is larger than {MAX_RESPONSE_BYTES} bytes")));
                }
                raw.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                let text = String::from_utf8_lossy(&raw);
                return Err(failed(format!("HTTP {}: {}", status.as_u16(), self.scrub(&text))));
            }
            serde_json::from_slice(&raw).map_err(|e| failed(format!("the response is not JSON: {e}")))
        }

        /// Run `work` within the time limit of one generation.
        pub(super) async fn within_timeout<T>(
            &self,
            work: impl Future<Output = Result<T, ImageError>>,
        ) -> Result<T, ImageError> {
            match tokio::time::timeout(self.timeout, work).await {
                Ok(result) => result,
                Err(_) => Err(ImageError::Generation(format!("no answer within {} s", self.timeout.as_secs()))),
            }
        }
    }
}

#[cfg(feature = "generate-openai")]
mod openai {
    use std::time::Duration;

    use futures::future::BoxFuture;
    use graphrt_core::file::base64_decode;

    use super::transport::Transport;
    use super::{GenerateRequest, GeneratedImage, ImageGenerator};
    use crate::error::ImageError;

    /// [`ImageGenerator`] for the OpenAI images API (`POST {base_url}/images/generations`)
    /// and the servers that implement it.
    ///
    /// The base URL and the key come from the operator, never from a workflow
    /// or a model. Redirects are not followed, so the key and the prompt
    /// cannot be carried to another host. The answer must hold the image as
    /// base64 (`b64_json`, which GPT image models always return): an answer
    /// that only holds a URL is refused rather than fetched.
    pub struct OpenAiImages {
        transport: Transport,
        model: String,
    }

    impl std::fmt::Debug for OpenAiImages {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // The key is never printed.
            f.debug_struct("OpenAiImages")
                .field("base_url", &self.transport.base_url)
                .field("model", &self.model)
                .finish()
        }
    }

    impl OpenAiImages {
        /// A generator for `model` (for example `gpt-image-2`) at `base_url`
        /// (for example `https://api.openai.com/v1`).
        pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
            Self { transport: Transport::new(base_url.into(), api_key.into()), model: model.into() }
        }

        /// Longest time one generation may take (default 180 s).
        pub fn with_timeout(mut self, timeout: Duration) -> Self {
            self.transport.timeout = timeout;
            self
        }

        async fn request(&self, request: &GenerateRequest) -> Result<GeneratedImage, ImageError> {
            let failed = |message: String| ImageError::Generation(message);
            let mut body = serde_json::json!({ "model": self.model, "prompt": request.prompt, "n": 1 });
            if let (Some(size), Some(fields)) = (&request.size, body.as_object_mut()) {
                fields.insert("size".to_owned(), serde_json::Value::String(size.clone()));
            }
            let url = format!("{}/images/generations", self.transport.base_url);
            let call = self.transport.client()?.post(&url).bearer_auth(self.transport.api_key()).json(&body);
            let json = self.transport.send(call).await?;
            let first = json.pointer("/data/0").ok_or_else(|| failed("the response holds no image".to_owned()))?;
            let Some(encoded) = first.get("b64_json").and_then(serde_json::Value::as_str) else {
                return Err(failed(
                    "the provider answered with a URL instead of image data: use a model that returns base64"
                        .to_owned(),
                ));
            };
            let bytes =
                base64_decode(encoded).ok_or_else(|| failed("the image data is not valid base64".to_owned()))?;
            Ok(GeneratedImage { bytes })
        }
    }

    impl ImageGenerator for OpenAiImages {
        fn generate<'a>(&'a self, request: &'a GenerateRequest) -> BoxFuture<'a, Result<GeneratedImage, ImageError>> {
            Box::pin(async move {
                request.validate()?;
                self.transport.within_timeout(self.request(request)).await
            })
        }
    }
}

#[cfg(feature = "generate-gemini")]
mod gemini {
    use std::time::Duration;

    use futures::future::BoxFuture;
    use graphrt_core::file::base64_decode;

    use super::transport::Transport;
    use super::{GenerateRequest, GeneratedImage, ImageGenerator};
    use crate::error::ImageError;

    /// Aspect ratios the Gemini image models accept, as `(width, height)`.
    const ASPECT_RATIOS: [(u32, u32); 14] = [
        (1, 1),
        (1, 4),
        (1, 8),
        (2, 3),
        (3, 2),
        (3, 4),
        (4, 1),
        (4, 3),
        (4, 5),
        (5, 4),
        (8, 1),
        (9, 16),
        (16, 9),
        (21, 9),
    ];

    /// [`ImageGenerator`] for the image models of the Gemini API
    /// (`POST {base_url}/models/{model}:generateContent`).
    ///
    /// The base URL and the key come from the operator. The key travels in
    /// the `x-goog-api-key` header, never in the URL, and redirects are not
    /// followed. The answer must hold the image inline as base64: an answer
    /// that points to a file or a URL is refused rather than fetched. Images
    /// from these models carry Google's SynthID watermark.
    pub struct GeminiImages {
        transport: Transport,
        model: String,
    }

    impl std::fmt::Debug for GeminiImages {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // The key is never printed.
            f.debug_struct("GeminiImages")
                .field("base_url", &self.transport.base_url)
                .field("model", &self.model)
                .finish()
        }
    }

    impl GeminiImages {
        /// A generator for `model` (for example `gemini-3.1-flash-image`) at
        /// `base_url` (for example `https://generativelanguage.googleapis.com/v1beta`).
        pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
            let model: String = model.into();
            let model = model.trim().trim_start_matches("models/").to_owned();
            Self { transport: Transport::new(base_url.into(), api_key.into()), model }
        }

        /// Longest time one generation may take (default 180 s).
        pub fn with_timeout(mut self, timeout: Duration) -> Self {
            self.transport.timeout = timeout;
            self
        }

        async fn request(&self, request: &GenerateRequest) -> Result<GeneratedImage, ImageError> {
            let failed = |message: String| ImageError::Generation(message);
            // The model name becomes part of the URL path: only a plain name may.
            let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
            if self.model.is_empty() || !self.model.chars().all(plain) {
                return Err(failed(format!("`{}` is not a model name", self.transport.scrub(&self.model))));
            }
            let mut config = serde_json::json!({ "responseModalities": ["TEXT", "IMAGE"] });
            if let (Some(size), Some(fields)) = (request.size.as_deref().and_then(image_config), config.as_object_mut())
            {
                fields.insert("imageConfig".to_owned(), size);
            }
            let body = serde_json::json!({
                "contents": [{ "parts": [{ "text": request.prompt }] }],
                "generationConfig": config,
            });
            let url = format!("{}/models/{}:generateContent", self.transport.base_url, self.model);
            let call =
                self.transport.client()?.post(&url).header("x-goog-api-key", self.transport.api_key()).json(&body);
            let json = self.transport.send(call).await?;
            let parts = json.pointer("/candidates/0/content/parts").and_then(serde_json::Value::as_array);
            let field = |part: &serde_json::Value, camel: &str, snake: &str| {
                part.get(camel).or_else(|| part.get(snake)).cloned()
            };
            let inline = parts.into_iter().flatten().find_map(|part| field(part, "inlineData", "inline_data"));
            let Some(inline) = inline else {
                let points_elsewhere =
                    parts.into_iter().flatten().any(|part| field(part, "fileData", "file_data").is_some());
                if points_elsewhere {
                    return Err(failed(
                        "the provider answered with a file reference instead of image data, which is not fetched"
                            .to_owned(),
                    ));
                }
                // Say why when the provider does: a refusal is not a broken answer.
                let reason = ["/candidates/0/finishReason", "/promptFeedback/blockReason"]
                    .iter()
                    .find_map(|pointer| json.pointer(pointer).and_then(serde_json::Value::as_str))
                    .map(|reason| format!(" ({})", self.transport.scrub(reason)))
                    .unwrap_or_default();
                return Err(failed(format!("the response holds no image{reason}")));
            };
            let encoded = inline.get("data").and_then(serde_json::Value::as_str).unwrap_or_default();
            let bytes =
                base64_decode(encoded).ok_or_else(|| failed("the image data is not valid base64".to_owned()))?;
            Ok(GeneratedImage { bytes })
        }
    }

    /// `imageConfig` for a size given as `WIDTHxHEIGHT`: the closest aspect
    /// ratio the API knows, and the smallest of its sizes that holds the
    /// longer side.
    fn image_config(size: &str) -> Option<serde_json::Value> {
        let (width, height) = size.split_once('x')?;
        let (width, height): (u32, u32) = (width.parse().ok()?, height.parse().ok()?);
        if width == 0 || height == 0 {
            return None;
        }
        // Compare ratios by cross multiplication: no floating point, no rounding.
        let distance = |(w, h): (u32, u32)| {
            let (asked, offered) = (u64::from(width) * u64::from(h), u64::from(w) * u64::from(height));
            // Relative to the size of both, so tall and wide ratios are judged alike.
            asked.abs_diff(offered) * 10_000 / asked.max(offered).max(1)
        };
        let (ratio_width, ratio_height) = ASPECT_RATIOS.iter().copied().min_by_key(|ratio| distance(*ratio))?;
        let image_size = match width.max(height) {
            0..=1024 => "1K",
            1025..=2048 => "2K",
            _ => "4K",
        };
        Some(serde_json::json!({ "aspectRatio": format!("{ratio_width}:{ratio_height}"), "imageSize": image_size }))
    }

    impl ImageGenerator for GeminiImages {
        fn generate<'a>(&'a self, request: &'a GenerateRequest) -> BoxFuture<'a, Result<GeneratedImage, ImageError>> {
            Box::pin(async move {
                request.validate()?;
                self.transport.within_timeout(self.request(request)).await
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::image_config;

        #[test]
        fn a_pixel_size_becomes_the_closest_ratio_and_size_the_api_knows() {
            let config = |size: &str| {
                let config = image_config(size).unwrap();
                format!("{} {}", config["aspectRatio"].as_str().unwrap(), config["imageSize"].as_str().unwrap())
            };
            assert_eq!(config("1024x1024"), "1:1 1K");
            assert_eq!(config("1920x1080"), "16:9 2K");
            assert_eq!(config("1080x1920"), "9:16 2K");
            assert_eq!(config("1536x1024"), "3:2 2K");
            assert_eq!(config("1024x1536"), "2:3 2K");
            assert_eq!(config("4096x1024"), "4:1 4K");
            assert_eq!(config("512x4096"), "1:8 4K");
            assert_eq!(config("2520x1080"), "21:9 4K");
            assert_eq!(config("800x600"), "4:3 1K");
            assert_eq!(config("16x8192"), "1:8 4K");
            for odd in ["", "x", "0x10", "10x0", "axb", "1024"] {
                assert!(image_config(odd).is_none(), "{odd}");
            }
        }
    }
}
