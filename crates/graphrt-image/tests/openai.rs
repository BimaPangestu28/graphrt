//! `OpenAiImages` against a local server that speaks just enough HTTP.

#![cfg(feature = "generate-openai")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

#[path = "support/server.rs"]
mod server;

use std::time::Duration;

use graphrt_core::file::base64_encode;
use graphrt_image::{GenerateRequest, ImageError, ImageGenerator, OpenAiImages};
use server::{ok_json, serve};

const KEY: &str = "sk-test-very-secret-key";

fn request(prompt: &str) -> GenerateRequest {
    GenerateRequest { prompt: prompt.to_owned(), size: Some("1024x1024".to_owned()) }
}

#[tokio::test]
async fn a_base64_answer_becomes_image_bytes() {
    let image = b"\x89PNG\r\n\x1a\nnot-really-pixels";
    let body = format!(r#"{{"created":1,"data":[{{"b64_json":"{}"}}]}}"#, base64_encode(image));
    let (base_url, seen) = serve(vec![ok_json(&body)]).await;
    let generator = OpenAiImages::new(base_url, KEY, "gpt-image-2");
    let generated = generator.generate(&request("a red bicycle")).await.unwrap();
    assert_eq!(generated.bytes, image);

    let sent = seen.lock().unwrap()[0].clone();
    assert!(sent.starts_with("POST /v1/images/generations HTTP/1.1"), "{sent}");
    assert!(sent.to_ascii_lowercase().contains(&format!("authorization: bearer {KEY}")));
    let body: serde_json::Value = serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"model": "gpt-image-2", "prompt": "a red bicycle", "n": 1, "size": "1024x1024"})
    );
    // The key never shows up in debug output.
    assert!(!format!("{generator:?}").contains(KEY));
}

/// A URL in the answer would make the runtime fetch an address a remote
/// service chose. It is refused, not fetched.
#[tokio::test]
async fn an_answer_with_a_url_is_refused_not_fetched() {
    let body = r#"{"data":[{"url":"http://169.254.169.254/latest/meta-data/iam"}]}"#;
    let (base_url, seen) = serve(vec![ok_json(body)]).await;
    let refused = OpenAiImages::new(base_url, KEY, "dall-e-3").generate(&request("x")).await.unwrap_err();
    assert!(matches!(&refused, ImageError::Generation(m) if m.contains("URL instead of image data")), "{refused:?}");
    assert_eq!(seen.lock().unwrap().len(), 1, "exactly one request: nothing was fetched");
}

#[tokio::test]
async fn errors_are_reported_without_the_key() {
    let error = format!(r#"{{"error":{{"message":"Incorrect API key provided: {KEY}"}}}}"#);
    let unauthorized =
        format!("HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{error}", error.len());
    let (base_url, _) = serve(vec![unauthorized]).await;
    let refused = OpenAiImages::new(base_url, KEY, "gpt-image-2").generate(&request("x")).await.unwrap_err();
    let message = refused.to_string();
    assert!(message.contains("HTTP 401") && message.contains("******"), "{message}");
    assert!(!message.contains(KEY), "{message}");

    for body in ["not json", r#"{"data":[]}"#, r#"{"data":[{"b64_json":"***not base64***"}]}"#] {
        let (base_url, _) = serve(vec![ok_json(body)]).await;
        let refused = OpenAiImages::new(base_url, KEY, "gpt-image-2").generate(&request("x")).await;
        assert!(matches!(refused, Err(ImageError::Generation(_))), "{body}: {refused:?}");
    }
}

/// Following a redirect would carry the key and the prompt to another host.
#[tokio::test]
async fn redirects_are_not_followed() {
    let (elsewhere, elsewhere_seen) = serve(vec![ok_json(r#"{"data":[{"b64_json":"AAAA"}]}"#)]).await;
    let redirect = format!(
        "HTTP/1.1 307 Temporary Redirect\r\nlocation: {elsewhere}/images/generations\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    let (base_url, _) = serve(vec![redirect]).await;
    let refused = OpenAiImages::new(base_url, KEY, "gpt-image-2").generate(&request("x")).await.unwrap_err();
    assert!(refused.to_string().contains("HTTP 307"), "{refused}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(elsewhere_seen.lock().unwrap().is_empty(), "the redirect target was contacted");
}

#[tokio::test]
async fn bad_requests_never_leave_and_slow_servers_time_out() {
    let (base_url, seen) = serve(Vec::new()).await;
    let generator = OpenAiImages::new(base_url, KEY, "gpt-image-2");
    for bad in [
        GenerateRequest { prompt: "  ".into(), size: None },
        GenerateRequest { prompt: "x".into(), size: Some("big".into()) },
        GenerateRequest { prompt: "x".into(), size: Some("1024x".into()) },
        GenerateRequest { prompt: "x".repeat(4_001), size: None },
    ] {
        assert!(matches!(generator.generate(&bad).await, Err(ImageError::Invalid(_))), "{bad:?}");
    }
    assert!(seen.lock().unwrap().is_empty());

    // A server that accepts the connection and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _held = listener.accept().await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let slow =
        OpenAiImages::new(format!("http://{address}/v1"), KEY, "gpt-image-2").with_timeout(Duration::from_millis(200));
    let refused = slow.generate(&request("x")).await.unwrap_err();
    assert!(refused.to_string().contains("no answer within"), "{refused}");
}
