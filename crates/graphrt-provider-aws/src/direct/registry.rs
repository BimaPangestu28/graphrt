//! Reading images from OCI registries (GHCR, Docker Hub, `public.ecr.aws`, a
//! private ECR registry) and writing them to the deployment's ECR repository
//! through the ECR API, so a direct deploy needs no Docker.
//!
//! Lambda only runs images from ECR in the function's own account and region,
//! and only single-platform manifests. A deploy copies the graphrt image of the
//! function's architecture, adds one layer (the Lambda Web Adapter and the
//! agent) and pushes the result. Layers ECR already has are skipped, so a
//! redeploy uploads only the new layer.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::layer::{Layer, sha256_digest};
use super::{DirectError, ErrorCode};
use crate::rest::AwsRestClient;

const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
                              application/vnd.docker.distribution.manifest.list.v2+json, \
                              application/vnd.oci.image.manifest.v1+json, \
                              application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
/// Largest manifest or config read.
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
/// Largest layer pulled (the graphrt image's layers are far smaller).
const MAX_BLOB_BYTES: usize = 512 * 1024 * 1024;
/// Bytes per `UploadLayerPart` call.
const UPLOAD_PART_BYTES: usize = 8 * 1024 * 1024;
/// Retries of a registry answering 429 or 503 (1, 2, 4, 8, 16 s apart unless it says).
const MAX_BUSY_RETRIES: u32 = 5;
const ECR_TARGET: &str = "AmazonEC2ContainerRegistry_V20150921";

/// `registry/repository:tag` or `registry/repository@sha256:...`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    /// Registry host, e.g. `ghcr.io`.
    pub registry: String,
    pub repository: String,
    /// Tag or digest.
    pub reference: String,
}

impl ImageRef {
    /// Parses an image name the way Docker does (`graphrt/graphrt` is Docker Hub, tag `latest`).
    ///
    /// @throws DirectError(InvalidInput) - not an image name
    pub fn parse(image: &str) -> Result<Self, DirectError> {
        let invalid =
            || DirectError::invalid(format!("--image {image:?} is not an image name such as ghcr.io/org/name:tag"));
        if image.is_empty() || image.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid());
        }
        let (name, reference) = match image.split_once('@') {
            Some((name, digest)) => (name, digest.to_owned()),
            None => match image.rsplit_once(':') {
                Some((name, tag)) if !tag.contains('/') => (name, tag.to_owned()),
                _ => (image, "latest".to_owned()),
            },
        };
        let (registry, repository) = match name.split_once('/') {
            Some((host, rest)) if host.contains('.') || host.contains(':') || host == "localhost" => {
                (host.to_owned(), rest.to_owned())
            }
            Some(_) => ("registry-1.docker.io".to_owned(), name.to_owned()),
            None => ("registry-1.docker.io".to_owned(), format!("library/{name}")),
        };
        let registry = if registry == "docker.io" { "registry-1.docker.io".to_owned() } else { registry };
        if repository.is_empty() || reference.is_empty() {
            return Err(invalid());
        }
        Ok(Self { registry, repository, reference })
    }

    /// A private ECR registry (`<account>.dkr.ecr.<region>.amazonaws.com`).
    pub fn is_private_ecr(&self) -> bool {
        self.registry.contains(".dkr.ecr.") && self.registry.ends_with(".amazonaws.com")
    }
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let separator = if self.reference.starts_with("sha256:") { '@' } else { ':' };
        write!(f, "{}/{}{separator}{}", self.registry, self.repository, self.reference)
    }
}

/// One layer of a pulled manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LayerRef {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
}

/// A single-platform image: its manifest, config and layers.
#[derive(Clone, Debug)]
pub(crate) struct PulledImage {
    pub manifest_media_type: String,
    pub config: Value,
    pub layers: Vec<LayerRef>,
    /// `amd64` or `arm64`.
    pub architecture: String,
}

/// Reads images from one registry, anonymously or with ECR basic auth.
pub(crate) struct RegistryReader {
    http: reqwest::Client,
    image: ImageRef,
    /// `Authorization` header value once known.
    authorization: Option<String>,
}

impl std::fmt::Debug for RegistryReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryReader").field("image", &self.image).finish_non_exhaustive()
    }
}

fn registry_error(image: &ImageRef, message: impl std::fmt::Display) -> DirectError {
    DirectError::new(ErrorCode::ImageUnavailable, format!("cannot read the image {image}: {message}"))
}

/// The `key="value"` pairs of a `WWW-Authenticate: Bearer ...` challenge.
fn challenge_parameters(header: &str) -> Vec<(String, String)> {
    let rest = header.trim().strip_prefix("Bearer").or_else(|| header.trim().strip_prefix("bearer")).unwrap_or("");
    let mut parameters = Vec::new();
    let mut remaining = rest.trim();
    while let Some((key, after)) = remaining.split_once('=') {
        let key = key.trim().trim_start_matches(',').trim().to_owned();
        let after = after.trim_start();
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((value, next)) => (value.to_owned(), next),
                None => (quoted.to_owned(), ""),
            }
        } else {
            match after.split_once(',') {
                Some((value, next)) => (value.trim().to_owned(), next),
                None => (after.trim().to_owned(), ""),
            }
        };
        parameters.push((key, value));
        remaining = next.trim_start_matches(',').trim();
    }
    parameters
}

impl RegistryReader {
    /// A reader of `image`. `ecr_authorization` is the `Basic ...` value of an
    /// ECR authorization token, for an image in a private ECR registry.
    pub(crate) fn new(image: ImageRef, ecr_authorization: Option<String>) -> Result<Self, DirectError> {
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|error| DirectError::new(ErrorCode::Internal, format!("cannot build an HTTP client: {error}")))?;
        Ok(Self { http, image, authorization: ecr_authorization })
    }

    fn url(&self, path: &str) -> String {
        let scheme = if self.image.registry.starts_with("localhost") { "http" } else { "https" };
        format!("{scheme}://{}/v2/{}/{path}", self.image.registry, self.image.repository)
    }

    /// Fetches an anonymous pull token for the challenge of a 401.
    async fn authenticate(&mut self, challenge: &str) -> Result<(), DirectError> {
        let parameters = challenge_parameters(challenge);
        let value = |name: &str| parameters.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        let realm = value("realm").ok_or_else(|| registry_error(&self.image, "the registry asks for a login"))?;
        let scope = value("scope").unwrap_or_else(|| format!("repository:{}:pull", self.image.repository));
        let mut query = vec![("scope", scope)];
        if let Some(service) = value("service") {
            query.push(("service", service));
        }
        let encoded: Vec<String> = query
            .iter()
            .map(|(key, value)| format!("{key}={}", crate::auth::sigv4::uri_encode(value, false)))
            .collect();
        let separator = if realm.contains('?') { '&' } else { '?' };
        let response = self
            .http
            .get(format!("{realm}{separator}{}", encoded.join("&")))
            .send()
            .await
            .map_err(|error| registry_error(&self.image, error))?;
        if !response.status().is_success() {
            return Err(registry_error(
                &self.image,
                format!("the registry refused an anonymous pull ({}): is the image public?", response.status()),
            ));
        }
        let bytes = response.bytes().await.map_err(|error| registry_error(&self.image, error))?;
        let body: Value = serde_json::from_slice(&bytes).map_err(|error| registry_error(&self.image, error))?;
        let token = body
            .get("token")
            .or_else(|| body.get("access_token"))
            .and_then(Value::as_str)
            .ok_or_else(|| registry_error(&self.image, "the registry gave no pull token"))?;
        self.authorization = Some(format!("Bearer {token}"));
        Ok(())
    }

    /// `GET` with authentication, retried once after a token challenge.
    async fn get(
        &mut self,
        url: &str,
        accept: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<u8>, Option<String>), DirectError> {
        let mut authenticated = false;
        let mut busy_retries = 0u32;
        loop {
            let mut request = self.http.get(url);
            if let Some(accept) = accept {
                request = request.header("accept", accept);
            }
            if let Some(authorization) = &self.authorization {
                request = request.header("authorization", authorization);
            }
            let response = request.send().await.map_err(|error| registry_error(&self.image, error))?;
            let status = response.status();
            // Anonymous pulls are rate limited (public.ecr.aws allows about one request a second).
            if (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status == reqwest::StatusCode::SERVICE_UNAVAILABLE)
                && busy_retries < MAX_BUSY_RETRIES
            {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map_or_else(|| Duration::from_secs(1 << busy_retries), Duration::from_secs)
                    .min(Duration::from_secs(30));
                busy_retries += 1;
                tokio::time::sleep(wait).await;
                continue;
            }
            if status == reqwest::StatusCode::UNAUTHORIZED && !authenticated {
                authenticated = true;
                let challenge = response
                    .headers()
                    .get("www-authenticate")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                self.authenticate(&challenge).await?;
                continue;
            }
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(registry_error(&self.image, "no such image or tag (check --image)"));
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(registry_error(&self.image, "the registry keeps asking for a login: is the image public?"));
            }
            if !status.is_success() {
                return Err(registry_error(&self.image, format!("HTTP {status}")));
            }
            let content_type =
                response.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_owned);
            let bytes = response.bytes().await.map_err(|error| registry_error(&self.image, error))?;
            if bytes.len() > limit {
                return Err(registry_error(&self.image, format!("a blob is larger than {limit} bytes")));
            }
            return Ok((bytes.to_vec(), content_type));
        }
    }

    async fn manifest(&mut self, reference: &str) -> Result<(Value, String), DirectError> {
        let url = self.url(&format!("manifests/{reference}"));
        let (bytes, content_type) = self.get(&url, Some(MANIFEST_TYPES), MAX_JSON_BYTES).await?;
        let manifest: Value = serde_json::from_slice(&bytes)
            .map_err(|error| registry_error(&self.image, format!("bad manifest: {error}")))?;
        let media_type = manifest
            .get("mediaType")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(content_type)
            .unwrap_or_else(|| DOCKER_MANIFEST.to_owned());
        Ok((manifest, media_type))
    }

    /// The image for Linux on one of `architectures` (first match in that order).
    ///
    /// @throws DirectError(ImageUnavailable) - unreadable, or no such platform
    pub(crate) async fn pull(&mut self, architectures: &[&str]) -> Result<PulledImage, DirectError> {
        let reference = self.image.reference.clone();
        let (mut manifest, mut media_type) = self.manifest(&reference).await?;
        if let Some(entries) = manifest.get("manifests").and_then(Value::as_array) {
            let platform_of = |entry: &Value| {
                let platform = entry.get("platform")?;
                let os = platform.get("os").and_then(Value::as_str)?;
                let architecture = platform.get("architecture").and_then(Value::as_str)?;
                (os == "linux").then(|| architecture.to_owned())
            };
            let chosen = architectures.iter().find_map(|wanted| {
                entries
                    .iter()
                    .find(|entry| platform_of(entry).as_deref() == Some(*wanted))
                    .and_then(|entry| entry.get("digest").and_then(Value::as_str).map(str::to_owned))
            });
            let digest = chosen.ok_or_else(|| {
                registry_error(
                    &self.image,
                    format!("the image has no linux/{} variant", architectures.join(" or linux/")),
                )
            })?;
            (manifest, media_type) = self.manifest(&digest).await?;
        }
        let config_digest = manifest
            .get("config")
            .and_then(|config| config.get("digest"))
            .and_then(Value::as_str)
            .ok_or_else(|| registry_error(&self.image, "the manifest has no config"))?
            .to_owned();
        let config_bytes = self.blob(&config_digest, MAX_JSON_BYTES).await?;
        let config: Value = serde_json::from_slice(&config_bytes)
            .map_err(|error| registry_error(&self.image, format!("bad image config: {error}")))?;
        let architecture = config.get("architecture").and_then(Value::as_str).unwrap_or("amd64").to_owned();
        if !architectures.contains(&architecture.as_str()) {
            return Err(registry_error(
                &self.image,
                format!("the image is for {architecture}, Lambda needs amd64 or arm64"),
            ));
        }
        let layers = manifest
            .get("layers")
            .and_then(Value::as_array)
            .ok_or_else(|| registry_error(&self.image, "the manifest has no layers"))?
            .iter()
            .map(|layer| {
                Some(LayerRef {
                    media_type: layer.get("mediaType").and_then(Value::as_str)?.to_owned(),
                    digest: layer.get("digest").and_then(Value::as_str)?.to_owned(),
                    size: layer.get("size").and_then(Value::as_u64)?,
                })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| registry_error(&self.image, "a layer entry is incomplete"))?;
        if layers.iter().any(|layer| layer.media_type.contains("zstd")) {
            return Err(registry_error(&self.image, "zstd layers are not supported by Lambda: use a gzip image"));
        }
        Ok(PulledImage { manifest_media_type: media_type, config, layers, architecture })
    }

    /// A blob's bytes, checked against its digest.
    pub(crate) async fn blob(&mut self, digest: &str, limit: usize) -> Result<Vec<u8>, DirectError> {
        let url = self.url(&format!("blobs/{digest}"));
        let (bytes, _) = self.get(&url, None, limit).await?;
        if sha256_digest(&bytes) != digest {
            return Err(registry_error(&self.image, format!("blob {digest} does not match its digest")));
        }
        Ok(bytes)
    }

    pub(crate) async fn layer_blob(&mut self, digest: &str) -> Result<Vec<u8>, DirectError> {
        self.blob(digest, MAX_BLOB_BYTES).await
    }
}

/// The image config of `base` with `extra` as its last layer.
pub(crate) fn config_with_layer(base: &Value, extra: &Layer, created_by: &str) -> Value {
    let mut config = base.clone();
    if let Some(object) = config.as_object_mut() {
        let rootfs = object.entry("rootfs").or_insert_with(|| json!({ "type": "layers", "diff_ids": [] }));
        if let Some(diff_ids) = rootfs.get_mut("diff_ids").and_then(Value::as_array_mut) {
            diff_ids.push(Value::from(extra.diff_id.clone()));
        }
        let history = object.entry("history").or_insert_with(|| json!([]));
        if let Some(history) = history.as_array_mut() {
            history.push(json!({ "created_by": created_by, "comment": "graphrt deploy --direct" }));
        }
    }
    config
}

/// The manifest of the composed image: `base` layers, then `extra`.
pub(crate) fn manifest_with_layer(base: &PulledImage, config_bytes: &[u8], extra: &Layer) -> Value {
    let oci = base.manifest_media_type.contains("oci");
    let (manifest_type, config_type, layer_type) = if oci {
        ("application/vnd.oci.image.manifest.v1+json", "application/vnd.oci.image.config.v1+json", OCI_LAYER)
    } else {
        (DOCKER_MANIFEST, "application/vnd.docker.container.image.v1+json", DOCKER_LAYER)
    };
    let mut layers: Vec<Value> = base
        .layers
        .iter()
        .map(|layer| json!({ "mediaType": layer.media_type, "size": layer.size, "digest": layer.digest }))
        .collect();
    layers.push(json!({ "mediaType": layer_type, "size": extra.gzip.len(), "digest": extra.digest }));
    json!({
        "schemaVersion": 2,
        "mediaType": manifest_type,
        "config": { "mediaType": config_type, "size": config_bytes.len(), "digest": sha256_digest(config_bytes) },
        "layers": layers,
    })
}

/// The ECR API of the deployment's account and region.
#[derive(Debug, Clone)]
pub(crate) struct Ecr {
    pub client: AwsRestClient,
}

impl Ecr {
    async fn call(&self, action: &str, body: &Value) -> Result<Value, crate::api::ProviderError> {
        self.client.json_11(&format!("{ECR_TARGET}.{action}"), body).await
    }

    /// Creates the repository (tagged) unless it exists.
    pub(crate) async fn ensure_repository(&self, name: &str, tags: &[(&str, &str)]) -> Result<(), DirectError> {
        let tags: Vec<Value> = tags.iter().map(|(key, value)| json!({ "Key": key, "Value": value })).collect();
        let body = json!({ "repositoryName": name, "imageTagMutability": "MUTABLE", "tags": tags });
        match self.call("CreateRepository", &body).await {
            Ok(_) => Ok(()),
            Err(error) if error.message.starts_with("RepositoryAlreadyExistsException") => Ok(()),
            Err(error) => Err(DirectError::from_aws(error, "create the ECR repository")),
        }
    }

    /// The `Authorization` value for this account's registry (`Basic ...`).
    pub(crate) async fn authorization(&self) -> Result<String, DirectError> {
        let response = self
            .call("GetAuthorizationToken", &json!({}))
            .await
            .map_err(|error| DirectError::from_aws(error, "read an ECR login token"))?;
        response
            .get("authorizationData")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("authorizationToken"))
            .and_then(Value::as_str)
            .map(|token| format!("Basic {token}"))
            .ok_or_else(|| DirectError::new(ErrorCode::AwsApi, "ECR gave no login token"))
    }

    /// The digests among `digests` that the repository does not have yet.
    pub(crate) async fn missing_layers(
        &self,
        repository: &str,
        digests: &[String],
    ) -> Result<Vec<String>, DirectError> {
        let mut missing = Vec::new();
        for chunk in digests.chunks(100) {
            let body = json!({ "repositoryName": repository, "layerDigests": chunk });
            let response = self
                .call("BatchCheckLayerAvailability", &body)
                .await
                .map_err(|error| DirectError::from_aws(error, "check the ECR layers"))?;
            let available: Vec<&str> = response
                .get("layers")
                .and_then(Value::as_array)
                .map(|layers| {
                    layers
                        .iter()
                        .filter(|layer| layer.get("layerAvailability").and_then(Value::as_str) == Some("AVAILABLE"))
                        .filter_map(|layer| layer.get("layerDigest").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            missing.extend(chunk.iter().filter(|digest| !available.contains(&digest.as_str())).cloned());
        }
        Ok(missing)
    }

    /// Uploads one blob (layer or config).
    pub(crate) async fn upload(&self, repository: &str, digest: &str, bytes: &[u8]) -> Result<(), DirectError> {
        let context = "upload the image to ECR";
        let started = self
            .call("InitiateLayerUpload", &json!({ "repositoryName": repository }))
            .await
            .map_err(|error| DirectError::from_aws(error, context))?;
        let upload_id = started
            .get("uploadId")
            .and_then(Value::as_str)
            .ok_or_else(|| DirectError::new(ErrorCode::AwsApi, "ECR gave no upload id"))?
            .to_owned();
        let mut first = 0usize;
        for part in bytes.chunks(UPLOAD_PART_BYTES) {
            let last = first + part.len().saturating_sub(1);
            let body = json!({
                "repositoryName": repository, "uploadId": upload_id,
                "partFirstByte": first, "partLastByte": last, "layerPartBlob": STANDARD.encode(part),
            });
            self.call("UploadLayerPart", &body).await.map_err(|error| DirectError::from_aws(error, context))?;
            first += part.len();
        }
        let body = json!({ "repositoryName": repository, "uploadId": upload_id, "layerDigests": [digest] });
        match self.call("CompleteLayerUpload", &body).await {
            Ok(_) => Ok(()),
            Err(error) if error.message.starts_with("LayerAlreadyExistsException") => Ok(()),
            Err(error) => Err(DirectError::from_aws(error, context)),
        }
    }

    /// Tags `manifest` as `tag`; returns the image digest.
    pub(crate) async fn put_image(
        &self,
        repository: &str,
        tag: &str,
        manifest: &str,
        media_type: &str,
    ) -> Result<String, DirectError> {
        let body = json!({
            "repositoryName": repository, "imageTag": tag,
            "imageManifest": manifest, "imageManifestMediaType": media_type,
        });
        match self.call("PutImage", &body).await {
            Ok(_) => {}
            Err(error) if error.message.starts_with("ImageAlreadyExistsException") => {}
            Err(error) => return Err(DirectError::from_aws(error, "tag the image in ECR")),
        }
        Ok(sha256_digest(manifest.as_bytes()))
    }

    /// Deletes the images tagged `tags` (missing ones are ignored).
    pub(crate) async fn delete_tags(&self, repository: &str, tags: &[String]) -> Result<(), DirectError> {
        if tags.is_empty() {
            return Ok(());
        }
        let ids: Vec<Value> = tags.iter().map(|tag| json!({ "imageTag": tag })).collect();
        self.call("BatchDeleteImage", &json!({ "repositoryName": repository, "imageIds": ids }))
            .await
            .map(|_| ())
            .map_err(|error| DirectError::from_aws(error, "delete old images in ECR"))
    }

    /// Deletes the repository with its images; `false` when it did not exist.
    pub(crate) async fn delete_repository(&self, repository: &str) -> Result<bool, DirectError> {
        match self.call("DeleteRepository", &json!({ "repositoryName": repository, "force": true })).await {
            Ok(_) => Ok(true),
            Err(error) if error.message.starts_with("RepositoryNotFoundException") => Ok(false),
            Err(error) => Err(DirectError::from_aws(error, "delete the ECR repository")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct::layer::{Entry, build};

    #[test]
    fn image_names_parse_like_docker() {
        let ghcr = ImageRef::parse("ghcr.io/graphrt/graphrt:0.6.3").unwrap();
        assert_eq!(
            (ghcr.registry.as_str(), ghcr.repository.as_str(), ghcr.reference.as_str()),
            ("ghcr.io", "graphrt/graphrt", "0.6.3")
        );
        let hub = ImageRef::parse("graphrt/graphrt").unwrap();
        assert_eq!((hub.registry.as_str(), hub.reference.as_str()), ("registry-1.docker.io", "latest"));
        assert_eq!(ImageRef::parse("alpine").unwrap().repository, "library/alpine");
        let pinned = ImageRef::parse("localhost:5000/a/b@sha256:abc").unwrap();
        assert_eq!((pinned.registry.as_str(), pinned.reference.as_str()), ("localhost:5000", "sha256:abc"));
        assert_eq!(pinned.to_string(), "localhost:5000/a/b@sha256:abc");
        assert!(
            ImageRef::parse("123456789012.dkr.ecr.ap-southeast-1.amazonaws.com/graphrt:1").unwrap().is_private_ecr()
        );
        assert!(ImageRef::parse("has space").is_err());
    }

    #[test]
    fn bearer_challenges_are_read() {
        let parameters = challenge_parameters(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:graphrt/graphrt:pull""#,
        );
        assert_eq!(parameters[0], ("realm".to_owned(), "https://ghcr.io/token".to_owned()));
        assert_eq!(parameters[2].1, "repository:graphrt/graphrt:pull");
    }

    #[test]
    fn the_composed_image_lists_the_new_layer_last_in_manifest_and_config() {
        let extra = build(&[Entry::file("a", 0o644, b"x".to_vec())]).unwrap();
        let base = PulledImage {
            manifest_media_type: DOCKER_MANIFEST.to_owned(),
            config: json!({ "architecture": "arm64", "rootfs": { "type": "layers", "diff_ids": ["sha256:base"] } }),
            layers: vec![LayerRef { media_type: DOCKER_LAYER.to_owned(), digest: "sha256:l1".to_owned(), size: 10 }],
            architecture: "arm64".to_owned(),
        };
        let config = config_with_layer(&base.config, &extra, "agent");
        assert_eq!(config["rootfs"]["diff_ids"][1], Value::from(extra.diff_id.clone()));
        let config_bytes = serde_json::to_vec(&config).unwrap();
        let manifest = manifest_with_layer(&base, &config_bytes, &extra);
        assert_eq!(manifest["layers"][1]["digest"], Value::from(extra.digest.clone()));
        assert_eq!(manifest["layers"][1]["mediaType"], DOCKER_LAYER);
        assert_eq!(manifest["config"]["digest"], Value::from(sha256_digest(&config_bytes)));
    }
}
