//! Presigned STS `GetCallerIdentity` tokens for the EKS API server (`k8s-aws-v1.`).
//!
//! The same token `aws eks get-token` / aws-iam-authenticator produce: a SigV4
//! query-signed `GET https://sts.<region>.amazonaws.com/?Action=GetCallerIdentity`
//! with the cluster name bound through the signed `x-k8s-aws-id` header, valid
//! 60 s on the STS side (EKS accepts it for up to 15 minutes), base64url without
//! padding. Used by the Kubernetes provider for EKS clusters outside the cluster.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::sigv4::{AwsCredentials, PresignRequest, amz_date, presign_query};

/// Token for the EKS cluster `cluster` in `region`, signed at `unix_seconds`.
///
/// `sts_host` is normally `sts.<region>.amazonaws.com`.
///
/// @example
/// // let token = eks_token(&creds, "eu-west-1", "sts.eu-west-1.amazonaws.com", "prod", now);
/// // Authorization: Bearer {token}
pub fn eks_token(
    credentials: &AwsCredentials,
    region: &str,
    sts_host: &str,
    cluster: &str,
    unix_seconds: u64,
) -> String {
    let date = amz_date(unix_seconds);
    let headers = [("host", sts_host), ("x-k8s-aws-id", cluster)];
    let query = presign_query(
        credentials,
        &PresignRequest {
            method: "GET",
            path: "/",
            query: &[("Action", "GetCallerIdentity"), ("Version", "2011-06-15")],
            headers: &headers,
            region,
            service: "sts",
            amz_date: &date,
            expires_secs: 60,
        },
    );
    let url = format!("https://{sts_host}/?{query}");
    format!("k8s-aws-v1.{}", URL_SAFE_NO_PAD.encode(url.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_decodes_to_presigned_sts_url() {
        let creds = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: Some("tok/en".into()),
        };
        let token = eks_token(&creds, "us-east-1", "sts.us-east-1.amazonaws.com", "prod", 1_440_938_160);
        let encoded = token.strip_prefix("k8s-aws-v1.").unwrap();
        assert!(!encoded.contains('='));
        let url = String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
        assert!(url.starts_with("https://sts.us-east-1.amazonaws.com/?Action=GetCallerIdentity&Version=2011-06-15&"));
        assert!(url.contains("X-Amz-Credential=AKIDEXAMPLE%2F20150830%2Fus-east-1%2Fsts%2Faws4_request"), "{url}");
        assert!(url.contains("X-Amz-Expires=60"));
        assert!(url.contains("X-Amz-Security-Token=tok%2Fen"));
        assert!(url.contains("X-Amz-SignedHeaders=host%3Bx-k8s-aws-id"));
        assert!(url.rsplit('&').next().unwrap().starts_with("X-Amz-Signature="));
        // Same inputs, same token (deterministic).
        assert_eq!(token, eks_token(&creds, "us-east-1", "sts.us-east-1.amazonaws.com", "prod", 1_440_938_160));
    }
}
