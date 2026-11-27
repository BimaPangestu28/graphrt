//! `GeminiImages` against a local server that speaks just enough HTTP.

#![cfg(feature = "generate-gemini")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

#[path = "support/server.rs"]
mod server;

use std::time::Duration;

use graphrt_core::file::base64_encode;
use graphrt_image::{GeminiImages, GenerateRequest, ImageError, ImageGenerator};
use server::{ok_json, serve};

const KEY: &str = "AIza-test-very-secret-key";
const MODEL: &str = "gemini-3.1-flash-image";

fn request(prompt: &str, size: Option<&str>) -> GenerateRequest {
    GenerateRequest { prompt: prompt.to_owned(), size: size.map(str::to_owned) }
}

fn answer(parts: &str) -> String {
    ok_json(&format!(r#"{{"candidates":[{{"content":{{"parts":[{parts}]}},"finishReason":"STOP"}}]}}"#))
}

#[tokio::test]
async fn an_inline_answer_becomes_image_bytes() {
    let image = b"\x89PNG\r\n\x1a\nnot-really-pixels";
    // The model may talk before it draws: the first inline image is the answer.
    let parts = format!(
        r#"{{"text":"Here is your picture."}},{{"inlineData":{{"mimeType":"image/png","data":"{}"}}}}"#,
        base64_encode(image)
    );
    let (base_url, seen) = serve(vec![answer(&parts)]).await;
    let generator = GeminiImages::new(base_url, KEY, format!("models/{MODEL}"));
    let generated = generator.generate(&request("a red bicycle", Some("1920x1080"))).await.unwrap();
    assert_eq!(generated.bytes, image);

    let sent = seen.lock().unwrap()[0].clone();
    assert!(sent.starts_with(&format!("POST /v1/models/{MODEL}:generateContent HTTP/1.1")), "{sent}");
    // The key travels in a header and nowhere in the URL.
    assert!(sent.to_ascii_lowercase().contains(&format!("x-goog-api-key: {}", KEY.to_ascii_lowercase())));
    assert!(!sent.lines().next().unwrap().contains(KEY));
    let body: serde_json::Value = serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "contents": [{ "parts": [{ "text": "a red bicycle" }] }],
            "generationConfig": {
                "responseModalities": ["TEXT", "IMAGE"],
                "imageConfig": { "aspectRatio": "16:9", "imageSize": "2K" }
            }
        })
    );
    assert!(!format!("{generator:?}").contains(KEY));

    // Without a size nothing is said about one, and the older field spelling is understood too.
    let parts = format!(r#"{{"inline_data":{{"mime_type":"image/png","data":"{}"}}}}"#, base64_encode(image));
    let (base_url, seen) = serve(vec![answer(&parts)]).await;
    let generated = GeminiImages::new(base_url, KEY, MODEL).generate(&request("x", None)).await.unwrap();
    assert_eq!(generated.bytes, image);
    let sent = seen.lock().unwrap()[0].clone();
    let body: serde_json::Value = serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["generationConfig"], serde_json::json!({ "responseModalities": ["TEXT", "IMAGE"] }));
}

/// A file reference in the answer would make the runtime fetch an address a
/// remote service chose. It is refused, not fetched.
#[tokio::test]
async fn an_answer_that_points_to_a_file_is_refused_not_fetched() {
    let parts = r#"{"fileData":{"mimeType":"image/png","fileUri":"http://169.254.169.254/latest/meta-data/iam"}}"#;
    let (base_url, seen) = serve(vec![answer(parts)]).await;
    let refused = GeminiImages::new(base_url, KEY, MODEL).generate(&request("x", None)).await.unwrap_err();
    assert!(matches!(&refused, ImageError::Generation(m) if m.contains("file reference")), "{refused:?}");
    assert_eq!(seen.lock().unwrap().len(), 1, "exactly one request: nothing was fetched");
}

#[tokio::test]
async fn errors_and_refusals_are_reported_without_the_key() {
    let error =
        format!(r#"{{"error":{{"code":400,"message":"API key not valid: {KEY}","status":"INVALID_ARGUMENT"}}}}"#);
    let bad_request =
        format!("HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{error}", error.len());
    let (base_url, _) = serve(vec![bad_request]).await;
    let refused = GeminiImages::new(base_url, KEY, MODEL).generate(&request("x", None)).await.unwrap_err();
    let message = refused.to_string();
    assert!(message.contains("HTTP 400") && message.contains("******"), "{message}");
    assert!(!message.contains(KEY), "{message}");

    // A refusal by the provider says why, when the provider does.
    let blocked = ok_json(r#"{"candidates":[{"finishReason":"IMAGE_SAFETY"}]}"#);
    let (base_url, _) = serve(vec![blocked]).await;
    let refused = GeminiImages::new(base_url, KEY, MODEL).generate(&request("x", None)).await.unwrap_err();
    assert!(refused.to_string().contains("holds no image (IMAGE_SAFETY)"), "{refused}");
    let blocked = ok_json(r#"{"promptFeedback":{"blockReason":"PROHIBITED_CONTENT"}}"#);
    let (base_url, _) = serve(vec![blocked]).await;
    let refused = GeminiImages::new(base_url, KEY, MODEL).generate(&request("x", None)).await.unwrap_err();
    assert!(refused.to_string().contains("(PROHIBITED_CONTENT)"), "{refused}");

    let text_only = answer(r#"{"text":"I cannot draw that."}"#);
    let not_base64 = answer(r#"{"inlineData":{"mimeType":"image/png","data":"***not base64***"}}"#);
    for response in [ok_json("not json"), ok_json("{}"), text_only, not_base64] {
        let (base_url, _) = serve(vec![response]).await;
        let refused = GeminiImages::new(base_url, KEY, MODEL).generate(&request("x", None)).await;
        assert!(matches!(refused, Err(ImageError::Generation(_))), "{refused:?}");
    }
}

/// Following a redirect would carry the key and the prompt to another host.
#[tokio::test]
async fn redirects_are_not_followed() {
    let (elsewhere, elsewhere_seen) = serve(vec![answer(r#"{"inlineData":{"data":"AAAA"}}"#)]).await;
    let redirect = format!(
        "HTTP/1.1 307 Temporary Redirect\r\nlocation: {elsewhere}/models/{MODEL}:generateContent\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    let (base_url, _) = serve(vec![redirect]).await;
    let refused = GeminiImages::new(base_url, KEY, MODEL).generate(&request("x", None)).await.unwrap_err();
    assert!(refused.to_string().contains("HTTP 307"), "{refused}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(elsewhere_seen.lock().unwrap().is_empty(), "the redirect target was contacted");
}

/// The model name becomes part of the URL path, so only a plain name may be one.
#[tokio::test]
async fn a_model_name_cannot_change_the_request_path() {
    let (base_url, seen) = serve(Vec::new()).await;
    for model in ["../../v1/files", "m:generateContent?key=x#", "a/b", "m odel", "m\r\nHost: evil", "", "models/"] {
        let refused = GeminiImages::new(base_url.clone(), KEY, model).generate(&request("x", None)).await.unwrap_err();
        assert!(refused.to_string().contains("is not a model name"), "{model}: {refused}");
    }
    assert!(seen.lock().unwrap().is_empty(), "a request left for an invalid model name");
}

#[tokio::test]
async fn bad_requests_never_leave_and_slow_servers_time_out() {
    let (base_url, seen) = serve(Vec::new()).await;
    let generator = GeminiImages::new(base_url, KEY, MODEL);
    for bad in [request("  ", None), request("x", Some("big")), request(&"x".repeat(4_001), None)] {
        assert!(matches!(generator.generate(&bad).await, Err(ImageError::Invalid(_))));
    }
    assert!(seen.lock().unwrap().is_empty());

    // A server that accepts the connection and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let _held = tokio::spawn(async move {
        let _socket = listener.accept().await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let slow = GeminiImages::new(base_url, KEY, MODEL).with_timeout(Duration::from_millis(100));
    let refused = slow.generate(&request("x", None)).await.unwrap_err();
    assert!(refused.to_string().contains("no answer within"), "{refused}");
}
