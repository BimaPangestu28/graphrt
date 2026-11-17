//! Host credential callbacks through the embedding API, against a local
//! OpenAI-compatible server standing in for a provider or a thin proxy.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use graphrt_ffi::{CredentialProvider, CredentialRecord, EventListener, GraphrtError, Runtime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Quiet;

impl EventListener for Quiet {
    fn on_event(&self, _: String) {}
    fn on_answer_chunk(&self, _: String) {}
    fn on_finish(&self, _: String) {}
}

const SSE_OK: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello from the model\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
);

/// Start a server on its own thread. Accepts `Bearer <accepted>`; records every
/// `authorization` header it sees.
fn serve(accepted: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let end = loop {
                        let n = socket.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned()))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    while buf.len() < end + len {
                        let n = socket.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let auth = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("authorization").then(|| v.trim().to_owned())
                        })
                        .unwrap_or_default();
                    let ok = auth == format!("Bearer {accepted}");
                    seen.lock().unwrap().push(auth);
                    let (status, ctype, body) = if ok {
                        ("200 OK", "text/event-stream", SSE_OK.to_owned())
                    } else {
                        ("401 Unauthorized", "application/json", r#"{"error":{"message":"bad key"}}"#.to_owned())
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
    });
    let addr = rx.recv().unwrap();
    (format!("http://{addr}"), seen)
}

const CHAT: &str = r#"
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - id: llm
      type: llm
      model: { provider: proxy, name: test-model }
      prompt_template: [{ role: user, text: "hi" }]
    - { id: answer, type: answer, answer: "{{#llm.text#}}" }
  edges:
    - { source: start, target: llm }
    - { source: llm, target: answer }
"#;

fn runtime(base_url: &str) -> Arc<Runtime> {
    let config = serde_json::json!({
        "credentials": {
            "proxy": { "source": "external", "kind": "openai", "base_url": format!("{base_url}/v1") }
        }
    });
    Runtime::new(config.to_string()).unwrap()
}

fn run(rt: &Arc<Runtime>) -> serde_json::Value {
    let workflow = rt.clone().load_workflow(CHAT.into()).unwrap();
    let handle = workflow.run("{}".into(), "{}".into(), Arc::new(Quiet)).unwrap();
    serde_json::from_str(&handle.wait_json(20_000).expect("run finished")).unwrap()
}

/// Bring-your-own-key provider: returns a stale key first, then the real one.
#[derive(Default)]
struct Keychain {
    gets: AtomicUsize,
    invalidations: AtomicUsize,
}

impl CredentialProvider for Keychain {
    fn get_credential(&self, provider: String) -> Result<CredentialRecord, GraphrtError> {
        assert_eq!(provider, "proxy");
        let n = self.gets.fetch_add(1, Ordering::SeqCst);
        let secret = if n == 0 { "stale-key" } else { "user-key" };
        Ok(CredentialRecord { secret: secret.into(), expires_at_ms: None, headers_json: None, base_url: None })
    }
    fn invalidate(&self, _provider: String) {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn host_credentials_are_used_and_refreshed_after_401() {
    let (url, seen) = serve("user-key");
    let rt = runtime(&url);
    let keychain = Arc::new(Keychain::default());
    rt.set_credential_provider("*".into(), keychain.clone());

    let result = run(&rt);
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(result["answer"], "hello from the model");
    assert_eq!(keychain.invalidations.load(Ordering::SeqCst), 1);
    assert_eq!(*seen.lock().unwrap(), vec!["Bearer stale-key".to_owned(), "Bearer user-key".to_owned()]);

    // The refreshed key is cached: the next run makes no new callback.
    let gets = keychain.gets.load(Ordering::SeqCst);
    assert_eq!(run(&rt)["status"], "succeeded");
    assert_eq!(keychain.gets.load(Ordering::SeqCst), gets);
}

#[test]
fn missing_provider_fails_the_run_cleanly() {
    let (url, seen) = serve("user-key");
    let rt = runtime(&url);
    let result = run(&rt);
    assert_eq!(result["status"], "failed");
    assert!(result["error"].as_str().unwrap().contains("no credential provider"), "{result}");
    assert!(seen.lock().unwrap().is_empty(), "no request without a credential");
}

struct Broken;

impl CredentialProvider for Broken {
    fn get_credential(&self, _provider: String) -> Result<CredentialRecord, GraphrtError> {
        panic!("keychain bug")
    }
    fn invalidate(&self, _provider: String) {
        panic!("keychain bug")
    }
}

struct Denied;

impl CredentialProvider for Denied {
    fn get_credential(&self, _provider: String) -> Result<CredentialRecord, GraphrtError> {
        Err(GraphrtError::InvalidInput { message: "user has not entered a key".into() })
    }
    fn invalidate(&self, _provider: String) {}
}

#[test]
fn failing_or_panicking_callbacks_do_not_crash() {
    let (url, _) = serve("user-key");
    let rt = runtime(&url);
    rt.set_credential_provider("proxy".into(), Arc::new(Broken));
    let result = run(&rt);
    assert_eq!(result["status"], "failed");
    assert!(result["error"].as_str().unwrap().contains("panicked"), "{result}");

    rt.set_credential_provider("proxy".into(), Arc::new(Denied));
    let result = run(&rt);
    assert_eq!(result["status"], "failed");
    assert!(result["error"].as_str().unwrap().contains("not entered a key"), "{result}");

    rt.clear_credential_provider("proxy".into());
    assert_eq!(run(&rt)["status"], "failed");
}

#[test]
fn config_needs_no_provider_keys_and_rejects_typos() {
    // No API key anywhere in the config.
    assert!(Runtime::new(r#"{"credentials": {"openai": {"source": "external"}}}"#.into()).is_ok());
    let err = Runtime::new(r#"{"credentials": {"openai": {"source": "external", "api_key": "x"}}}"#.into());
    assert!(matches!(err, Err(GraphrtError::InvalidConfig { .. })));
    let err = Runtime::new(r#"{"credentials": {"openai": {"source": "token_broker"}}}"#.into());
    assert!(matches!(err, Err(GraphrtError::InvalidConfig { .. })));
    // Unknown provider names must say which API they speak.
    let err = Runtime::new(r#"{"credentials": {"mistral": {"source": "external"}}}"#.into());
    assert!(matches!(err, Err(GraphrtError::InvalidConfig { .. })));
    // A typesafe entry is accepted (it builds the System One client, not a chat model).
    assert!(Runtime::new(r#"{"credentials": {"typesafe": {"source": "external"}}}"#.into()).is_ok());
    // Plain-http brokers are refused outside localhost.
    let err = Runtime::new(
        r#"{"credentials": {"openai": {"source": "token_broker", "url": "http://broker.example.com/t"}}}"#.into(),
    );
    assert!(matches!(err, Err(GraphrtError::InvalidConfig { .. })));
}

/// Returns whichever user is currently "signed in".
struct SignedIn(Mutex<&'static str>);

impl CredentialProvider for SignedIn {
    fn get_credential(&self, _provider: String) -> Result<CredentialRecord, GraphrtError> {
        let user = *self.0.lock().unwrap();
        Ok(CredentialRecord { secret: user.into(), expires_at_ms: None, headers_json: None, base_url: None })
    }
    fn invalidate(&self, _provider: String) {}
}

#[test]
fn switching_or_clearing_providers_drops_cached_keys() {
    let (url, seen) = serve("user-b");
    let rt = runtime(&url);
    rt.set_credential_provider("proxy".into(), Arc::new(SignedIn(Mutex::new("user-a"))));
    // User A's key is rejected by this server, but it was fetched and cached.
    assert_eq!(run(&rt)["status"], "failed");
    // User A signs out, user B signs in: the cached key must not be reused.
    rt.clear_credential_provider("proxy".into());
    rt.set_credential_provider("proxy".into(), Arc::new(SignedIn(Mutex::new("user-b"))));
    let result = run(&rt);
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(seen.lock().unwrap().last().map(String::as_str), Some("Bearer user-b"));
}
