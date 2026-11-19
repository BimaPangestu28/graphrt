//! Fake OpenAI-compatible chat endpoint shared by both sides of the comparison.
//!
//! ```text
//! fakeoai [ADDR] [DELAY_MS]     # default 127.0.0.1:9911, 0 ms
//! ```
//!
//! Every POST, whatever its path or body, waits `DELAY_MS` (simulated model
//! latency) and then answers with the same `text/event-stream` body: 10 text
//! deltas, a `stop` finish reason and `[DONE]`. HTTP/1.1 with keep-alive; the
//! request body is read and discarded. It runs on two Tokio worker threads so it
//! never becomes the bottleneck of a single-threaded client.
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn sse_body() -> String {
    let mut body = String::new();
    for i in 0..10 {
        body.push_str(&format!(
            "data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gpt-4o-mini\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i} \"}},\"finish_reason\":null}}]}}\n\n"
        ));
    }
    body.push_str("data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
    body.push_str("data: [DONE]\n\n");
    body
}

/// Reads one request (headers, then `content-length` body bytes) from `buffer`
/// and the socket. Returns `false` when the peer closed the connection.
async fn read_request(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 8192];
    let (header_end, body_length) = loop {
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..position]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            break (position + 4, length);
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    };
    while buffer.len() < header_end + body_length {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
    buffer.drain(..header_end + body_length);
    true
}

async fn serve_connection(mut socket: TcpStream, response: &'static [u8], delay: Duration) {
    let _ = socket.set_nodelay(true);
    let mut buffer = Vec::with_capacity(8192);
    while read_request(&mut socket, &mut buffer).await {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if socket.write_all(response).await.is_err() {
            return;
        }
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() {
    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "127.0.0.1:9911".into());
    let delay_ms: u64 = args.next().map(|v| v.parse().expect("DELAY_MS must be an integer")).unwrap_or(0);
    let listener = TcpListener::bind(&address).await.unwrap_or_else(|e| panic!("cannot bind {address}: {e}"));
    let body = sse_body();
    let response: &'static [u8] = Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
        .into_boxed_slice(),
    );
    eprintln!("fakeoai listening on {address}, delay {delay_ms} ms");
    let delay = Duration::from_millis(delay_ms);
    loop {
        let Ok((socket, _)) = listener.accept().await else { continue };
        tokio::spawn(serve_connection(socket, response, delay));
    }
}
