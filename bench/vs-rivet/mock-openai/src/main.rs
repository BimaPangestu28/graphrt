//! Deterministic OpenAI-compatible mock for the graphrt vs Rivet benchmark.
//!
//! `POST .../chat/completions` with `"stream": true` answers a chat-completions
//! SSE stream: a role frame, `--chunks` content deltas (`tok0 `, `tok1 `, ...),
//! a finish frame, a usage frame and `[DONE]`. The first content delta is
//! written together with the response headers (0 ms to first token); later
//! deltas follow `--interval-ms` apart. Without `"stream": true` the same text
//! comes back as one JSON completion. Both runtimes under test talk to the same
//! instance, so the model side is identical for both.
//!
//! HTTP/1.1 keep-alive is supported (responses use chunked encoding), so the
//! clients' connection pools work as they would against a real provider.
//!
//! `GET /health` answers `ok`; `GET /stats` answers `{"requests": n}`.
//!
//! Usage: mock-openai --port 18080 --chunks 20 --interval-ms 0

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Copy, Debug)]
struct Config {
    port: u16,
    chunks: u32,
    interval: Duration,
}

fn parse_args() -> Result<Config> {
    let mut config = Config { port: 18080, chunks: 20, interval: Duration::ZERO };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--port" => config.port = value.parse().context("--port")?,
            "--chunks" => config.chunks = value.parse().context("--chunks")?,
            "--interval-ms" => config.interval = Duration::from_millis(value.parse().context("--interval-ms")?),
            other => bail!("unknown option {other}"),
        }
    }
    Ok(config)
}

fn main() -> Result<()> {
    let config = parse_args()?;
    // A small fixed pool: the mock competes with the runtimes for CPU the same
    // way in every run.
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()?;
    runtime.block_on(serve(config))
}

async fn serve(config: Config) -> Result<()> {
    // A deep accept queue: 1,000 clients connecting at once must not see a
    // dropped SYN (a 1 s retransmit) at the mock.
    let socket = tokio::net::TcpSocket::new_v4().context("socket")?;
    socket.set_reuseaddr(true)?;
    socket.bind(std::net::SocketAddr::from(([127, 0, 0, 1], config.port))).context("bind")?;
    let listener: TcpListener = socket.listen(4096).context("listen")?;
    let requests = Arc::new(AtomicU64::new(0));
    eprintln!(
        "mock-openai listening on 127.0.0.1:{} ({} chunks, {:?} apart)",
        config.port, config.chunks, config.interval
    );
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept")?;
                let requests = requests.clone();
                tokio::spawn(async move {
                    if let Err(error) = connection(stream, config, requests).await {
                        // Clients closing a kept-alive connection are normal.
                        if !is_disconnect(&error) {
                            eprintln!("mock-openai: {error:#}");
                        }
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}

fn is_disconnect(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof
        )
    })
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
    close: bool,
}

/// Reads one request; `None` when the client closed the connection between requests.
async fn read_request(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Result<Option<Request>> {
    let mut head = Vec::with_capacity(1024);
    loop {
        let mut byte = [0u8; 1];
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            if head.is_empty() {
                return Ok(None);
            }
            bail!("connection closed inside the request head");
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            bail!("request head too large");
        }
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut length = 0usize;
    let mut chunked = false;
    let mut close = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        match name.as_str() {
            "content-length" => length = value.parse().context("content-length")?,
            "transfer-encoding" => chunked = value.contains("chunked"),
            "connection" => close = value == "close",
            _ => {}
        }
    }
    let body = if chunked {
        read_chunked(reader).await?
    } else {
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).await?;
        body
    };
    Ok(Some(Request { method, path, body, close }))
}

async fn read_line(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Result<String> {
    let mut line = Vec::new();
    loop {
        let byte = reader.read_u8().await?;
        line.push(byte);
        if line.ends_with(b"\r\n") {
            line.truncate(line.len() - 2);
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
    }
}

async fn read_chunked(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let size_line = read_line(reader).await?;
        let size =
            usize::from_str_radix(size_line.split(';').next().unwrap_or("0").trim(), 16).context("chunk size")?;
        if size == 0 {
            // Trailers end with an empty line.
            while !read_line(reader).await?.is_empty() {}
            return Ok(body);
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..]).await?;
        read_line(reader).await?;
    }
}

fn frame(delta: serde_json::Value, finish: Option<&str>) -> String {
    let body = serde_json::json!({
        "id": "chatcmpl-bench", "object": "chat.completion.chunk", "created": 0, "model": "gpt-4o-mini",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    });
    format!("data: {body}\n\n")
}

fn chunk(data: &str) -> Vec<u8> {
    format!("{:x}\r\n{data}\r\n", data.len()).into_bytes()
}

async fn connection(stream: TcpStream, config: Config, requests: Arc<AtomicU64>) -> Result<()> {
    stream.set_nodelay(true)?;
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    while let Some(request) = read_request(&mut reader).await? {
        let connection_header = if request.close { "close" } else { "keep-alive" };
        if request.method == "GET" && (request.path == "/health" || request.path == "/stats") {
            let body = if request.path == "/health" {
                "ok".to_string()
            } else {
                format!("{{\"requests\":{}}}", requests.load(Ordering::Relaxed))
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: {connection_header}\r\n\r\n{body}",
                body.len()
            );
            write.write_all(head.as_bytes()).await?;
        } else if request.method == "POST" && request.path.ends_with("/chat/completions") {
            requests.fetch_add(1, Ordering::Relaxed);
            let stream_requested = serde_json::from_slice::<serde_json::Value>(&request.body)
                .ok()
                .and_then(|body| body.get("stream").and_then(serde_json::Value::as_bool))
                .unwrap_or(false);
            if stream_requested {
                stream_completion(&mut write, config, connection_header).await?;
            } else {
                let text: String = (0..config.chunks).map(|i| format!("tok{i} ")).collect();
                let body = serde_json::json!({
                    "id": "chatcmpl-bench", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": text }, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": config.chunks, "total_tokens": 10 + config.chunks },
                })
                .to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: {connection_header}\r\n\r\n{body}",
                    body.len()
                );
                write.write_all(head.as_bytes()).await?;
            }
        } else {
            let head =
                format!("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: {connection_header}\r\n\r\n");
            write.write_all(head.as_bytes()).await?;
        }
        write.flush().await?;
        if request.close {
            break;
        }
    }
    Ok(())
}

async fn stream_completion(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    config: Config,
    connection_header: &str,
) -> Result<()> {
    let mut first = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ntransfer-encoding: chunked\r\nconnection: {connection_header}\r\n\r\n"
    )
    .into_bytes();
    first.extend(chunk(&frame(serde_json::json!({ "role": "assistant", "content": "" }), None)));
    // Token 0 leaves with the headers: 0 ms to first token.
    for i in 0..config.chunks {
        let delta = chunk(&frame(serde_json::json!({ "content": format!("tok{i} ") }), None));
        if i == 0 {
            first.extend(delta);
            write.write_all(&first).await?;
        } else {
            if !config.interval.is_zero() {
                tokio::time::sleep(config.interval).await;
            }
            write.write_all(&delta).await?;
        }
    }
    if config.chunks == 0 {
        write.write_all(&first).await?;
    }
    let mut tail = chunk(&frame(serde_json::json!({}), Some("stop")));
    let usage = serde_json::json!({
        "id": "chatcmpl-bench", "object": "chat.completion.chunk", "created": 0, "model": "gpt-4o-mini", "choices": [],
        "usage": { "prompt_tokens": 10, "completion_tokens": config.chunks, "total_tokens": 10 + config.chunks },
    });
    tail.extend(chunk(&format!("data: {usage}\n\n")));
    tail.extend(chunk("data: [DONE]\n\n"));
    tail.extend(b"0\r\n\r\n");
    write.write_all(&tail).await?;
    Ok(())
}
