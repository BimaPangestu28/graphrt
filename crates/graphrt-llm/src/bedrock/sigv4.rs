//! AWS Signature Version 4 request signing (header-based).

use std::fmt;

use sha2::{Digest, Sha256};

/// Long-term or temporary AWS credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Session token of temporary credentials (sent as `x-amz-security-token`).
    pub session_token: Option<String>,
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("session_token", &self.session_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Everything that goes into a signature.
#[derive(Debug)]
pub(crate) struct SigningRequest<'a> {
    pub method: &'a str,
    /// Path exactly as sent on the wire (already percent-encoded once).
    pub path: &'a str,
    /// Unencoded query parameters.
    pub query: &'a [(&'a str, &'a str)],
    /// Headers to sign (names are lower-cased while signing). Must include `host` and `x-amz-date`.
    pub headers: &'a [(&'a str, &'a str)],
    /// Hex SHA-256 of the body.
    pub payload_hash: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
    /// Encode each path segment a second time for the canonical URI (all services except S3).
    pub double_encode_path: bool,
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// HMAC-SHA256 (RFC 2104).
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        for (slot, b) in block.iter_mut().zip(digest.iter()) {
            *slot = *b;
        }
    } else {
        for (slot, b) in block.iter_mut().zip(key.iter()) {
            *slot = *b;
        }
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// SigV4 URI encoding: everything except unreserved characters (and `/` when `keep_slash`).
pub(crate) fn uri_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || (keep_slash && b == b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The signing key for a date (`YYYYMMDD`), region and service.
pub(crate) fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn canonical_request(req: &SigningRequest<'_>) -> (String, String) {
    let path = if req.path.is_empty() { "/" } else { req.path };
    let canonical_uri = if req.double_encode_path { uri_encode(path, true) } else { path.to_owned() };

    let mut query: Vec<(String, String)> =
        req.query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    query.sort();
    let canonical_query: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();

    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    headers.sort();
    let mut canonical_headers = String::new();
    for (k, v) in &headers {
        canonical_headers.push_str(k);
        canonical_headers.push(':');
        canonical_headers.push_str(v);
        canonical_headers.push('\n');
    }
    let signed: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        canonical_uri,
        canonical_query.join("&"),
        canonical_headers,
        signed,
        req.payload_hash
    );
    (canonical, signed)
}

/// The `Authorization` header value for `req`.
pub(crate) fn authorization(creds: &AwsCredentials, req: &SigningRequest<'_>) -> String {
    let date = req.amz_date.get(..8).unwrap_or(req.amz_date);
    let scope = format!("{date}/{}/{}/aws4_request", req.region, req.service);
    let (canonical, signed) = canonical_request(req);
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", req.amz_date, sha256_hex(canonical.as_bytes()));
    let key = signing_key(&creds.secret_access_key, date, req.region, req.service);
    let signature = hex(&hmac_sha256(&key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        creds.access_key_id
    )
}

/// Format Unix seconds as `YYYYMMDDTHHMMSSZ`.
pub(crate) fn amz_date(unix_seconds: u64) -> String {
    let days = i64::try_from(unix_seconds / 86_400).unwrap_or(0);
    let secs = unix_seconds % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", secs / 3600, (secs % 3600) / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn example_creds() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    #[test]
    fn hmac_rfc4231_case_2() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn derived_signing_key_matches_aws_docs() {
        let key = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20120215", "us-east-1", "iam");
        assert_eq!(hex(&key), "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d");
    }

    fn vanilla(method: &str) -> String {
        let headers = [("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z")];
        let req = SigningRequest {
            method,
            path: "/",
            query: &[],
            headers: &headers,
            payload_hash: EMPTY_HASH,
            region: "us-east-1",
            service: "service",
            amz_date: "20150830T123600Z",
            double_encode_path: true,
        };
        authorization(&example_creds(), &req)
    }

    #[test]
    fn aws_test_suite_get_vanilla() {
        assert_eq!(
            vanilla("GET"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn aws_test_suite_post_vanilla() {
        assert!(
            vanilla("POST").ends_with("Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b")
        );
    }

    #[test]
    fn canonical_uri_is_double_encoded() {
        let headers = [("host", "h"), ("x-amz-date", "20150830T123600Z")];
        let req = SigningRequest {
            method: "POST",
            path: "/model/anthropic.claude-v2%3A1/converse-stream",
            query: &[("b", "2 x"), ("a", "1")],
            headers: &headers,
            payload_hash: EMPTY_HASH,
            region: "us-east-1",
            service: "bedrock",
            amz_date: "20150830T123600Z",
            double_encode_path: true,
        };
        let (canonical, signed) = canonical_request(&req);
        assert!(canonical.contains("/model/anthropic.claude-v2%253A1/converse-stream\na=1&b=2%20x\n"), "{canonical}");
        assert_eq!(signed, "host;x-amz-date");
    }

    #[test]
    fn date_formatting() {
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(1_709_251_199), "20240229T235959Z");
        assert_eq!(amz_date(0), "19700101T000000Z");
    }

    #[test]
    fn debug_redacts_secrets() {
        let mut c = example_creds();
        c.session_token = Some("tok-secret".into());
        let text = format!("{c:?}");
        assert!(!text.contains("EXAMPLEKEY") && !text.contains("tok-secret"));
    }
}
