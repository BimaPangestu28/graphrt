//! The files of the pages `graphrt serve` serves itself (the chat page, the
//! monitoring page): each with a strong validator, so a browser that has a
//! file is not sent it again.

use axum::body::Bytes;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_NONE_MATCH, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

pub(super) const SCRIPT_TYPE: &str = "text/javascript; charset=utf-8";
pub(super) const HTML_TYPE: &str = "text/html; charset=utf-8";

/// One file of the page with its headers.
#[derive(Debug)]
pub(super) struct Asset {
    bytes: Bytes,
    content_type: &'static str,
    /// A strong validator: the page changes only with the binary.
    etag: HeaderValue,
}

impl Asset {
    pub(super) fn new(bytes: impl Into<Bytes>, content_type: &'static str) -> Self {
        let bytes = bytes.into();
        let digest = Sha256::digest(&bytes);
        let tag = format!("\"{}\"", crate::hex::to_hex(digest.get(..12).unwrap_or_default()));
        let etag = HeaderValue::from_str(&tag).unwrap_or_else(|_| HeaderValue::from_static("\"0\""));
        Self { bytes, content_type, etag }
    }

    /// The file, or `304` when the client already has it.
    pub(super) fn response(&self, request: &HeaderMap) -> Response {
        let headers = [
            (CONTENT_TYPE, HeaderValue::from_static(self.content_type)),
            (ETAG, self.etag.clone()),
            // Revalidated on every load: an upgraded server serves its own page at once.
            (CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        ];
        if request.get(IF_NONE_MATCH) == Some(&self.etag) {
            return (StatusCode::NOT_MODIFIED, headers).into_response();
        }
        (headers, self.bytes.clone()).into_response()
    }

    /// The file as a page: with its `Content-Security-Policy` and without a referrer.
    pub(super) fn page_response(&self, request: &HeaderMap, policy: &HeaderValue) -> Response {
        let mut response = self.response(request);
        let headers = response.headers_mut();
        headers.insert(CONTENT_SECURITY_POLICY, policy.clone());
        headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        response
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_asset_is_not_sent_again_to_a_client_that_has_it() {
        let asset = Asset::new(&b"console.log(1)"[..], SCRIPT_TYPE);
        let first = asset.response(&HeaderMap::new());
        assert_eq!(first.status(), StatusCode::OK);
        let mut request = HeaderMap::new();
        request.insert(IF_NONE_MATCH, first.headers().get(ETAG).unwrap().clone());
        assert_eq!(asset.response(&request).status(), StatusCode::NOT_MODIFIED);
    }
}
