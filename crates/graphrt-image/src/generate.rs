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

#[cfg(feature = "generate-openai")]
pub use openai::OpenAiImages;

#[cfg(feature = "generate-openai")]
mod openai {
    use std::sync::OnceLock;
    use std::time::Duration;

    use futures::future::BoxFuture;
    use graphrt_core::file::base64_decode;

    use super::{GenerateRequest, GeneratedImage, ImageGenerator};
    use crate::error::ImageError;

    /// Largest response body read (base64 of the image plus a little JSON).
    const MAX_RESPONSE_BYTES: usize = 48 * 1024 * 1024;
    /// How much of an error body ends up in an error message.
    const MAX_ERROR_CHARS: usize = 400;

    /// [`ImageGenerator`] for the OpenAI images API (`POST {base_url}/images/generations`)
    /// and the servers that implement it.
    ///
    /// The base URL and the key come from the operator, never from a workflow
    /// or a model. Redirects are not followed, so the key and the prompt
    /// cannot be carried to another host. The answer must hold the image as
    /// base64 (`b64_json`, which GPT image models always return): an answer
    /// that only holds a URL is refused rather than fetched.
    pub struct OpenAiImages {
        base_url: String,
        api_key: String,
        model: String,
        timeout: Duration,
        client: OnceLock<Result<reqwest::Client, String>>,
    }

    impl std::fmt::Debug for OpenAiImages {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // The key is never printed.
            f.debug_struct("OpenAiImages").field("base_url", &self.base_url).field("model", &self.model).finish()
        }
    }

    impl OpenAiImages {
        /// A generator for `model` (for example `gpt-image-2`) at `base_url`
        /// (for example `https://api.openai.com/v1`).
        pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
            Self {
                base_url: base_url.into().trim_end_matches('/').to_owned(),
                api_key: api_key.into(),
                model: model.into(),
                timeout: Duration::from_secs(180),
                client: OnceLock::new(),
            }
        }

        /// Longest time one generation may take (default 180 s).
        pub fn with_timeout(mut self, timeout: Duration) -> Self {
            self.timeout = timeout;
            self
        }

        /// The HTTP client, built on first use (loading the trust store costs
        /// start-up time an agent that never generates should not pay).
        fn client(&self) -> Result<&reqwest::Client, ImageError> {
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

        fn scrub(&self, text: &str) -> String {
            let scrubbed =
                if self.api_key.len() >= 4 { text.replace(&self.api_key, "******") } else { text.to_owned() };
            scrubbed.chars().take(MAX_ERROR_CHARS).collect()
        }

        async fn request(&self, request: &GenerateRequest) -> Result<GeneratedImage, ImageError> {
            let failed = |message: String| ImageError::Generation(message);
            let mut body = serde_json::json!({ "model": self.model, "prompt": request.prompt, "n": 1 });
            if let (Some(size), Some(fields)) = (&request.size, body.as_object_mut()) {
                fields.insert("size".to_owned(), serde_json::Value::String(size.clone()));
            }
            let url = format!("{}/images/generations", self.base_url);
            let mut response = self
                .client()?
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| failed(self.scrub(&e.without_url().to_string())))?;
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
            let json: serde_json::Value =
                serde_json::from_slice(&raw).map_err(|e| failed(format!("the response is not JSON: {e}")))?;
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
                match tokio::time::timeout(self.timeout, self.request(request)).await {
                    Ok(result) => result,
                    Err(_) => Err(ImageError::Generation(format!("no answer within {} s", self.timeout.as_secs()))),
                }
            })
        }
    }
}
