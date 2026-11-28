//! The S3 bucket of a deployment's store (`graphrt serve --store s3://<bucket>`):
//! created on the first deploy that needs it, private (new buckets block
//! public access and encrypt with S3 managed keys by default), emptied and
//! deleted by `remove` or `--drop-store`.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use super::{DirectError, ErrorCode};
use crate::api::{ProviderError, ProviderErrorKind};
use crate::auth::sigv4::sha256_hex;
use crate::auth::{CredentialChain, xml_text};
use crate::rest::{ApiLimits, AwsRequest, AwsRestClient, ServiceEndpoint};

/// Keys deleted per `DeleteObjects` call (S3's maximum is 1000).
const DELETE_BATCH: usize = 1000;

/// The bucket name of a function's store: `<function>-<account>-<region>`,
/// shortened with a hash past S3's 63 characters.
pub(crate) fn bucket_name(function: &str, account: &str, region: &str) -> String {
    let full = format!("{function}-{account}-{region}");
    if full.len() <= 63 {
        return full;
    }
    let hash = sha256_hex(full.as_bytes());
    let mut short: String = full.chars().take(63 - 9).collect();
    short = short.trim_end_matches('-').to_owned();
    format!("{short}-{}", hash.get(..8).unwrap_or_default())
}

/// The S3 API of one bucket.
#[derive(Debug)]
pub(crate) struct Bucket {
    client: AwsRestClient,
    pub name: String,
    region: String,
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

impl Bucket {
    /// `name` in `region`, at `base_url` (`None`: the public endpoint).
    pub(crate) fn new(
        chain: Arc<CredentialChain>,
        name: &str,
        region: &str,
        base_url: Option<&str>,
        limits: ApiLimits,
    ) -> Result<Self, DirectError> {
        let endpoint = ServiceEndpoint {
            base_url: base_url.map_or_else(|| format!("https://{name}.s3.{region}.amazonaws.com"), str::to_owned),
            region: region.to_owned(),
            service: "s3".to_owned(),
        };
        let client = AwsRestClient::new(chain, endpoint, limits)
            .map_err(|error| DirectError::new(ErrorCode::Internal, error.message))?;
        Ok(Self { client, name: name.to_owned(), region: region.to_owned() })
    }

    async fn call(
        &self,
        method: &str,
        query: &[(&str, &str)],
        body: &[u8],
        extra: &[(&str, &str)],
    ) -> Result<String, ProviderError> {
        let hash = sha256_hex(body);
        let mut headers = vec![("x-amz-content-sha256", hash.as_str())];
        headers.extend_from_slice(extra);
        let request = AwsRequest { method, path: "/", query, headers: &headers, body, idempotent: true };
        let response = self.client.send(&request).await?;
        let bytes =
            response.bytes().await.map_err(|error| ProviderError::transient(format!("S3 response body: {error}")))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Whether the bucket exists (and these credentials may see it).
    pub(crate) async fn exists(&self) -> Result<bool, DirectError> {
        match self.call("HEAD", &[], &[], &[]).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(false),
            Err(error) => Err(DirectError::from_aws(error, "read the store bucket")),
        }
    }

    /// Creates the bucket (tagged) unless this account already has it.
    pub(crate) async fn ensure(&self, deployment: &str) -> Result<(), DirectError> {
        let body = if self.region == "us-east-1" {
            String::new()
        } else {
            format!(
                "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                xml_escape(&self.region)
            )
        };
        match self.call("PUT", &[], body.as_bytes(), &[]).await {
            Ok(_) => {}
            Err(error) if error.message.starts_with("BucketAlreadyOwnedByYou") => return Ok(()),
            Err(error) if error.message.starts_with("BucketAlreadyExists") => {
                return Err(DirectError::new(
                    ErrorCode::AwsApi,
                    format!(
                        "the store bucket name {} is taken by another AWS account: deploy with another --app name",
                        self.name
                    ),
                ));
            }
            Err(error) => return Err(DirectError::from_aws(error, "create the store bucket")),
        }
        let tagging = format!(
            "<Tagging xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><TagSet><Tag><Key>graphrt:deployment</Key>\
             <Value>{}</Value></Tag></TagSet></Tagging>",
            xml_escape(deployment)
        );
        let checksum = STANDARD.encode(sha2_digest(tagging.as_bytes()));
        let headers = [("x-amz-checksum-sha256", checksum.as_str()), ("x-amz-sdk-checksum-algorithm", "SHA256")];
        if let Err(error) = self.call("PUT", &[("tagging", "")], tagging.as_bytes(), &headers).await {
            // A missing tag never stops a deploy.
            tracing::warn!(bucket = %self.name, error = %error.message, "could not tag the store bucket");
        }
        Ok(())
    }

    /// Every key in the bucket.
    async fn keys(&self) -> Result<Vec<String>, DirectError> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2")];
            if let Some(token) = &token {
                query.push(("continuation-token", token.as_str()));
            }
            let page = self
                .call("GET", &query, &[], &[])
                .await
                .map_err(|error| DirectError::from_aws(error, "list the store bucket"))?;
            let mut rest = page.as_str();
            while let Some(start) = rest.find("<Key>") {
                let after = rest.get(start + 5..).unwrap_or_default();
                let Some(end) = after.find("</Key>") else { break };
                keys.push(unescape(after.get(..end).unwrap_or_default()));
                rest = after.get(end..).unwrap_or_default();
            }
            token = (xml_text(&page, "IsTruncated").as_deref() == Some("true"))
                .then(|| xml_text(&page, "NextContinuationToken"))
                .flatten();
            if token.is_none() {
                return Ok(keys);
            }
        }
    }

    /// Deletes every object, then the bucket. `false` when there was none.
    pub(crate) async fn delete(&self) -> Result<bool, DirectError> {
        if !self.exists().await? {
            return Ok(false);
        }
        let keys = self.keys().await?;
        for batch in keys.chunks(DELETE_BATCH) {
            let objects: String =
                batch.iter().map(|key| format!("<Object><Key>{}</Key></Object>", xml_escape(key))).collect();
            let body = format!("<Delete><Quiet>true</Quiet>{objects}</Delete>");
            let checksum = STANDARD.encode(sha2_digest(body.as_bytes()));
            let headers = [("x-amz-checksum-sha256", checksum.as_str()), ("x-amz-sdk-checksum-algorithm", "SHA256")];
            self.call("POST", &[("delete", "")], body.as_bytes(), &headers)
                .await
                .map_err(|error| DirectError::from_aws(error, "empty the store bucket"))?;
        }
        match self.call("DELETE", &[], &[], &[]).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(false),
            Err(error) => Err(DirectError::from_aws(error, "delete the store bucket")),
        }
    }
}

fn sha2_digest(bytes: &[u8]) -> Vec<u8> {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).to_vec()
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The inline policy that lets the function's role use its bucket.
pub(crate) fn role_policy(bucket: &str) -> String {
    let document = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            { "Effect": "Allow", "Action": ["s3:ListBucket"], "Resource": format!("arn:aws:s3:::{bucket}") },
            {
                "Effect": "Allow",
                "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
                "Resource": format!("arn:aws:s3:::{bucket}/*"),
            },
        ],
    });
    Value::to_string(&document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_names_fit_s3() {
        assert_eq!(
            bucket_name("graphrt-support", "123456789012", "ap-southeast-1"),
            "graphrt-support-123456789012-ap-southeast-1"
        );
        let long = bucket_name(&format!("graphrt-{}", "a".repeat(50)), "123456789012", "ap-southeast-1");
        assert!(long.len() <= 63 && !long.contains("--"), "{long}");
        assert_eq!(long, bucket_name(&format!("graphrt-{}", "a".repeat(50)), "123456789012", "ap-southeast-1"));
    }

    #[test]
    fn the_role_may_only_use_its_bucket() {
        let policy = role_policy("graphrt-a-1-ap-southeast-1");
        assert!(policy.contains("arn:aws:s3:::graphrt-a-1-ap-southeast-1/*") && !policy.contains("s3:*"));
    }
}
