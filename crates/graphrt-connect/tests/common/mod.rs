//! A tiny HTTP/1.1 mock server for connector tests: canned responses by
//! `METHOD /path?query` (or `METHOD /path` for any query), changeable while
//! the test runs, and a log of every request.

#![allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use graphrt_connect::{BuildContext, ConnectorRegistry, KnowledgeSink, SourceConfig};
use graphrt_rag::{HashEmbedder, KnowledgeBase, MemoryStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(value: serde_json::Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json".into(),
            headers: Vec::new(),
            body: value.to_string().into_bytes(),
        }
    }

    pub fn html(text: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8".into(),
            headers: Vec::new(),
            body: text.as_bytes().to_vec(),
        }
    }

    pub fn text(content_type: &str, text: &str) -> Self {
        Self { status: 200, content_type: content_type.into(), headers: Vec::new(), body: text.as_bytes().to_vec() }
    }

    pub fn status(status: u16) -> Self {
        Self { status, content_type: "application/json".into(), headers: Vec::new(), body: b"{}".to_vec() }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

#[derive(Default)]
struct State {
    routes: HashMap<String, Vec<Reply>>,
    log: Vec<Request>,
}

/// The mock server. Clones share routes and log.
#[derive(Clone)]
pub struct Mock {
    pub base: String,
    state: Arc<Mutex<State>>,
}

impl Mock {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let state = Arc::clone(&shared);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let header_end = loop {
                        let Ok(read) = socket.read(&mut chunk).await else { return };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end;
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
                    let mut lines = head.split("\r\n");
                    let mut first = lines.next().unwrap_or_default().split(' ');
                    let method = first.next().unwrap_or_default().to_owned();
                    let target = first.next().unwrap_or_default().to_owned();
                    let headers: HashMap<String, String> = lines
                        .filter_map(|line| line.split_once(':'))
                        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                        .collect();
                    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let mut body = buffer[header_end + 4..].to_vec();
                    while body.len() < length {
                        let Ok(read) = socket.read(&mut chunk).await else { return };
                        if read == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..read]);
                    }
                    let request = Request {
                        method: method.clone(),
                        target: target.clone(),
                        headers,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    };
                    let reply = {
                        let mut state = state.lock().unwrap();
                        state.log.push(request);
                        let path = target.split('?').next().unwrap_or_default();
                        let exact = format!("{method} {target}");
                        let loose = format!("{method} {path}");
                        let key = if state.routes.contains_key(&exact) { exact } else { loose };
                        match state.routes.get_mut(&key) {
                            // Several replies: served in order, the last one repeats.
                            Some(replies) if replies.len() > 1 => replies.remove(0),
                            Some(replies) => replies[0].clone(),
                            None => Reply {
                                status: 404,
                                content_type: "text/plain".into(),
                                headers: Vec::new(),
                                body: b"not found".to_vec(),
                            },
                        }
                    };
                    let mut response = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
                        reply.status,
                        reply.content_type,
                        reply.body.len()
                    );
                    for (name, value) in &reply.headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str("\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&reply.body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { base, state }
    }

    /// Serve `reply` for `route` (`GET /path` or `GET /path?query`).
    pub fn route(&self, route: &str, reply: Reply) {
        self.state.lock().unwrap().routes.insert(route.to_owned(), vec![reply]);
    }

    /// Serve the replies in order (the last one repeats).
    pub fn route_sequence(&self, route: &str, replies: Vec<Reply>) {
        self.state.lock().unwrap().routes.insert(route.to_owned(), replies);
    }

    pub fn unroute(&self, route: &str) {
        self.state.lock().unwrap().routes.remove(route);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().log.clone()
    }

    pub fn clear_log(&self) {
        self.state.lock().unwrap().log.clear();
    }

    /// Requests whose target starts with `prefix`.
    pub fn count(&self, method: &str, prefix: &str) -> usize {
        self.requests().iter().filter(|r| r.method == method && r.target.starts_with(prefix)).count()
    }
}

/// A knowledge base with one in-memory dataset `kb`.
pub fn knowledge() -> (KnowledgeBase, KnowledgeSink) {
    let kb = KnowledgeBase::new().with_store_dataset(
        "kb",
        Arc::new(MemoryStore::new()),
        Arc::new(HashEmbedder::new(128)),
        None,
    );
    let sink = KnowledgeSink::new(kb.clone());
    (kb, sink)
}

/// Build a source's connector, with private addresses allowed for the mock.
pub fn build(source: &SourceConfig, secrets: &dyn Fn(&str) -> Option<String>) -> Box<dyn graphrt_connect::Connector> {
    let mut ctx = BuildContext::new(secrets);
    ctx.private_service_endpoints = true;
    ConnectorRegistry::with_builtins().build(source, &ctx).unwrap()
}

/// Every document id in dataset `kb`.
pub async fn documents(kb: &KnowledgeBase) -> Vec<String> {
    let mut ids: Vec<String> =
        kb.list_documents("kb", 500, None).await.unwrap().into_iter().map(|d| d.document_id).collect();
    ids.sort();
    ids
}

/// The text of every chunk of the knowledge base, joined.
pub async fn all_text(kb: &KnowledgeBase) -> String {
    let mut text = String::new();
    for id in documents(kb).await {
        for chunk in kb.document_chunks("kb", &id).await.unwrap() {
            text.push_str(&chunk.text);
            text.push('\n');
        }
    }
    text
}
