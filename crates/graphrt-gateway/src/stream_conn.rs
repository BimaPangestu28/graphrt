//! Lean HTTP/1.1 client of the long-lived gateway → agent run streams
//! (`POST /internal/v1/runs`, `GET /internal/v1/runs/{id}/stream`) over a Unix
//! socket, a Windows named pipe or plain `http://`.
//!
//! The gateway holds one agent connection per streaming run, for the whole run.
//! Through reqwest each of them kept ~34 KiB alive: hyper's 8 KiB read and
//! 8 KiB write buffers, the response head buffer (pinned by the header map)
//! and the per-connection dispatch channel (one 9 KiB block). This client
//! keeps one small read buffer per stream instead (2 KiB, grown only while an
//! event is larger and shrunk back when it drained) and hands the de-chunked
//! bytes of that buffer to the SSE parser without another copy.
//!
//! Scope: the agent is our own `graphrt serve --trust-gateway` (or a platform
//! proxy in front of it), so only what it speaks is supported: HTTP/1.1
//! responses with a chunked, `content-length` or until-close body. `https://`
//! agents keep going through reqwest (rustls); the short control calls
//! (`hello`, `orphans`, `drain`, `abort`) too.
//!
//! Connections of finished streams are kept for reuse (keep-alive, at most
//! [`MAX_IDLE_PER_AGENT`] per agent, [`IDLE_TIMEOUT`]): a reused connection that
//! the agent closed in between is detected before the request, and a request
//! that still fails on it before any response byte is retried once on a new
//! connection.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use graphrt_provider_api::EndpointAddress;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient;

/// Read buffer of a stream at rest.
pub const INITIAL_READ_BUFFER: usize = 2 * 1024;
/// Largest read buffer: one read never asks for more than this.
pub const MAX_READ_BUFFER: usize = 64 * 1024;
/// Largest response head accepted from an agent.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Idle connections kept per agent.
pub const MAX_IDLE_PER_AGENT: usize = 32;
/// An idle connection older than this is closed instead of reused.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Connect timeout (same as the reqwest client of the control calls).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest error body read from a refusing agent.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Where a stream connects to, parsed from an [`EndpointAddress`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    socket: SocketTarget,
    /// `Host` header value.
    host: String,
    /// Path prefix of the base URL (`http://host/prefix`), without a trailing slash.
    prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SocketTarget {
    Unix(PathBuf),
    /// Windows named pipe name (`\\.\pipe\…`).
    Pipe(String),
    /// `host:port`.
    Tcp(String),
}

impl Target {
    /// The target of `address`, or `None` when this client does not speak its
    /// scheme (`https://`: reqwest handles it).
    pub fn of(address: &EndpointAddress) -> Option<Self> {
        match address {
            EndpointAddress::Unix(path) => {
                Some(Self { socket: SocketTarget::Unix(path.clone()), host: "agent".to_owned(), prefix: String::new() })
            }
            EndpointAddress::Pipe(name) => {
                Some(Self { socket: SocketTarget::Pipe(name.clone()), host: "agent".to_owned(), prefix: String::new() })
            }
            EndpointAddress::Http(base) => {
                let rest = base.strip_prefix("http://")?;
                let (authority, prefix) =
                    rest.split_once('/').map_or((rest, ""), |(authority, path)| (authority, path));
                if authority.is_empty() || authority.contains('@') {
                    return None;
                }
                let has_port = match authority.rsplit_once(':') {
                    Some((host, port)) => {
                        !port.is_empty() && !host.ends_with(':') && port.bytes().all(|b| b.is_ascii_digit())
                    }
                    None => false,
                };
                let bracketed_v6_without_port = authority.starts_with('[') && authority.ends_with(']');
                let connect = if has_port && !bracketed_v6_without_port {
                    authority.to_owned()
                } else {
                    format!("{authority}:80")
                };
                let prefix = prefix.trim_end_matches('/');
                Some(Self {
                    socket: SocketTarget::Tcp(connect),
                    host: authority.to_owned(),
                    prefix: if prefix.is_empty() { String::new() } else { format!("/{prefix}") },
                })
            }
            _ => None,
        }
    }
}

/// A connected socket: Unix, named pipe or TCP.
#[derive(Debug)]
enum Socket {
    #[cfg(unix)]
    Unix(UnixStream),
    #[cfg(windows)]
    Pipe(NamedPipeClient),
    Tcp(TcpStream),
}

/// `ERROR_PIPE_BUSY`: every instance of the pipe is connected right now; the
/// agent creates the next one as soon as it accepted the last.
#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

/// Opens a client end of the named pipe `name`, waiting while every instance is busy.
///
/// # Errors
/// When the pipe does not exist or cannot be opened (the caller bounds the wait).
#[cfg(windows)]
pub async fn connect_pipe(name: &str) -> io::Result<NamedPipeClient> {
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(name) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn unsupported(kind: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, format!("{kind} endpoints are not supported on this platform"))
}

impl Socket {
    async fn connect(target: &SocketTarget) -> io::Result<Self> {
        let connect = async {
            match target {
                #[cfg(unix)]
                SocketTarget::Unix(path) => UnixStream::connect(path).await.map(Self::Unix),
                #[cfg(not(unix))]
                SocketTarget::Unix(_) => Err(unsupported("Unix socket")),
                #[cfg(windows)]
                SocketTarget::Pipe(name) => connect_pipe(name).await.map(Self::Pipe),
                #[cfg(not(windows))]
                SocketTarget::Pipe(_) => Err(unsupported("named pipe")),
                SocketTarget::Tcp(authority) => {
                    let stream = TcpStream::connect(authority.as_str()).await?;
                    // Frames are small and latency-bound: never wait for Nagle.
                    stream.set_nodelay(true)?;
                    Ok(Self::Tcp(stream))
                }
            }
        };
        tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))?
    }

    /// Whether an idle connection still looks usable: no EOF, no unexpected
    /// bytes. Never blocks.
    fn is_reusable(&self) -> bool {
        let mut probe = [0u8; 1];
        let result = match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.try_read(&mut probe),
            #[cfg(windows)]
            Self::Pipe(stream) => stream.try_read(&mut probe),
            Self::Tcp(stream) => stream.try_read(&mut probe),
        };
        matches!(result, Err(error) if error.kind() == io::ErrorKind::WouldBlock)
    }
}

impl AsyncRead for Socket {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(windows)]
            Self::Pipe(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Socket {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(windows)]
            Self::Pipe(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(windows)]
            Self::Pipe(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tcp(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(windows)]
            Self::Pipe(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[derive(Debug)]
struct IdleSocket {
    socket: Socket,
    since: Instant,
}

/// Keep-alive connections of finished streams, per agent.
#[derive(Debug, Default)]
pub struct IdlePool {
    idle: Mutex<HashMap<SocketTarget, Vec<IdleSocket>>>,
}

impl IdlePool {
    fn take(&self, target: &SocketTarget) -> Option<Socket> {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        let sockets = idle.get_mut(target)?;
        while let Some(candidate) = sockets.pop() {
            if candidate.since.elapsed() < IDLE_TIMEOUT && candidate.socket.is_reusable() {
                return Some(candidate.socket);
            }
        }
        idle.remove(target);
        None
    }

    fn put(&self, target: SocketTarget, socket: Socket) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        let sockets = idle.entry(target).or_default();
        sockets.retain(|idle| idle.since.elapsed() < IDLE_TIMEOUT);
        if sockets.len() < MAX_IDLE_PER_AGENT {
            sockets.push(IdleSocket { socket, since: Instant::now() });
        }
    }

    /// Closes the idle connections to `address` (a replica that went away).
    pub fn forget(&self, address: &EndpointAddress) {
        if let Some(target) = Target::of(address) {
            self.idle.lock().unwrap_or_else(PoisonError::into_inner).remove(&target.socket);
        }
    }

    /// Idle connections held (all agents).
    pub fn idle_count(&self) -> usize {
        self.idle.lock().unwrap_or_else(PoisonError::into_inner).values().map(Vec::len).sum()
    }
}

/// One request: method, path (with query), extra headers and body.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// `GET` / `POST`.
    pub method: &'a str,
    /// Path and query, starting with `/`.
    pub path: &'a str,
    /// Headers besides `host`, `content-length` and `content-type`.
    pub headers: &'a [(&'a str, &'a str)],
    /// JSON body (`content-type: application/json` when not empty).
    pub body: &'a [u8],
}

/// Why a request failed.
#[derive(Debug)]
pub enum OpenError {
    /// No response: connect, write or read of the head failed.
    Unreachable(String),
    /// A non-2xx answer with (an excerpt of) its body.
    Refused {
        /// HTTP status.
        status: u16,
        /// Body excerpt.
        body: String,
    },
}

/// Sends `request` to `target` and returns the 2xx response body as a stream.
///
/// # Errors
/// [`OpenError::Unreachable`] when no response head arrived,
/// [`OpenError::Refused`] for a non-2xx status.
pub async fn open(pool: &Arc<IdlePool>, target: &Target, request: Request<'_>) -> Result<ResponseBody, OpenError> {
    let encoded = encode_request(target, request);
    if let Some(socket) = pool.take(&target.socket) {
        match exchange(socket, &encoded).await {
            Ok(response) => return finish_open(pool, target, response).await,
            // The agent closed the kept connection before it saw the request:
            // nothing reached it, so a new connection is safe.
            Err(Exchange::Unanswered(_)) => {}
            Err(Exchange::Failed(error)) => return Err(OpenError::Unreachable(error)),
        }
    }
    let socket = Socket::connect(&target.socket).await.map_err(|error| OpenError::Unreachable(error.to_string()))?;
    match exchange(socket, &encoded).await {
        Ok(response) => finish_open(pool, target, response).await,
        Err(Exchange::Unanswered(error) | Exchange::Failed(error)) => Err(OpenError::Unreachable(error)),
    }
}

async fn finish_open(pool: &Arc<IdlePool>, target: &Target, response: ResponseBody) -> Result<ResponseBody, OpenError> {
    let mut body = response;
    if (200..300).contains(&body.status) {
        body.pool = Some((Arc::clone(pool), target.socket.clone()));
        return Ok(body);
    }
    let status = body.status;
    let mut excerpt = Vec::new();
    while excerpt.len() < MAX_ERROR_BODY {
        match body.next_data().await {
            Ok(Some(data)) => excerpt.extend_from_slice(data),
            Ok(None) | Err(_) => break,
        }
    }
    Err(OpenError::Refused { status, body: String::from_utf8_lossy(&excerpt).chars().take(512).collect() })
}

fn encode_request(target: &Target, request: Request<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + request.body.len());
    let mut push = |parts: &[&str]| parts.iter().for_each(|part| out.extend_from_slice(part.as_bytes()));
    push(&[request.method, " ", &target.prefix, request.path, " HTTP/1.1\r\nhost: ", &target.host, "\r\n"]);
    for (name, value) in request.headers {
        push(&[name, ": ", value, "\r\n"]);
    }
    if !request.body.is_empty() || request.method == "POST" {
        let length = request.body.len().to_string();
        push(&["content-length: ", &length, "\r\n"]);
    }
    if !request.body.is_empty() {
        push(&["content-type: application/json\r\n"]);
    }
    push(&["\r\n"]);
    out.extend_from_slice(request.body);
    out
}

enum Exchange {
    /// Nothing came back (EOF / reset before the first response byte).
    Unanswered(String),
    /// Anything else.
    Failed(String),
}

async fn exchange(mut socket: Socket, encoded: &[u8]) -> Result<ResponseBody, Exchange> {
    if let Err(error) = socket.write_all(encoded).await {
        return Err(Exchange::Unanswered(error.to_string()));
    }
    let mut body = ResponseBody::new(socket);
    body.read_head().await?;
    Ok(body)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// `transfer-encoding: chunked`, in the given decoder state.
    Chunked(Chunk),
    /// `content-length`: bytes left.
    Length(u64),
    /// Until the agent closes the connection.
    UntilClose,
    /// The whole body was read.
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    /// Expecting a `size[;ext]\r\n` line.
    Size,
    /// Inside chunk data: bytes left.
    Data(u64),
    /// Expecting the `\r\n` after chunk data.
    DataEnd,
    /// After the last chunk: trailer lines until an empty one.
    Trailer,
}

/// The body of a 2xx response, read incrementally from one small buffer.
#[derive(Debug)]
pub struct ResponseBody {
    socket: Option<Socket>,
    status: u16,
    buffer: Vec<u8>,
    /// Consumed bytes at the front of `buffer`.
    start: usize,
    framing: Framing,
    keep_alive: bool,
    /// The last read filled the buffer: the next one gets a larger one.
    filled_last: bool,
    pool: Option<(Arc<IdlePool>, SocketTarget)>,
}

impl ResponseBody {
    fn new(socket: Socket) -> Self {
        Self {
            socket: Some(socket),
            status: 0,
            buffer: Vec::with_capacity(INITIAL_READ_BUFFER),
            start: 0,
            framing: Framing::UntilClose,
            keep_alive: false,
            filled_last: false,
            pool: None,
        }
    }

    /// HTTP status of the response.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Whether bytes are already buffered: the next [`Self::next_data`] may
    /// complete without waiting for the agent.
    pub fn has_buffered(&self) -> bool {
        self.start < self.buffer.len()
    }

    fn unread(&self) -> &[u8] {
        self.buffer.get(self.start..).unwrap_or_default()
    }

    /// Reads more bytes into the buffer; `Ok(0)` at EOF.
    ///
    /// The buffer adapts like hyper's: it doubles (up to [`MAX_READ_BUFFER`])
    /// after a read that filled it, and goes back to [`INITIAL_READ_BUFFER`]
    /// after a read that did not, so a stream of small frames keeps 2 KiB and a
    /// large event is not read 2 KiB per syscall.
    async fn fill(&mut self) -> io::Result<usize> {
        let Some(socket) = self.socket.as_mut() else { return Ok(0) };
        if self.start == self.buffer.len() {
            self.buffer.clear();
        } else if self.start > 0 {
            self.buffer.copy_within(self.start.., 0);
            self.buffer.truncate(self.buffer.len() - self.start);
        }
        self.start = 0;
        let capacity = self.buffer.capacity();
        let mut wanted = if self.filled_last { (capacity * 2).min(MAX_READ_BUFFER) } else { capacity };
        // Always room for at least one more read (a partial line or head).
        wanted = wanted.max(self.buffer.len() + INITIAL_READ_BUFFER);
        if wanted > capacity {
            self.buffer.reserve_exact(wanted - self.buffer.len());
        }
        // Reads into the spare capacity only: the buffer never grows here.
        let read = socket.read_buf(&mut self.buffer).await?;
        self.filled_last = self.buffer.len() == self.buffer.capacity();
        if !self.filled_last
            && self.buffer.capacity() > INITIAL_READ_BUFFER * 4
            && self.buffer.len() <= INITIAL_READ_BUFFER
        {
            // A large event passed: give its buffer back.
            self.buffer.shrink_to(INITIAL_READ_BUFFER);
        }
        Ok(read)
    }

    async fn read_head(&mut self) -> Result<(), Exchange> {
        let mut answered = false;
        let broken = |answered: bool, message: String| {
            if answered { Exchange::Failed(message) } else { Exchange::Unanswered(message) }
        };
        loop {
            if !self.unread().is_empty() {
                match self.parse_head().map_err(Exchange::Failed)? {
                    Some(true) => return Ok(()),
                    // An interim 1xx response: the real one follows (maybe already buffered).
                    Some(false) => continue,
                    None if self.unread().len() >= MAX_HEAD_BYTES => {
                        return Err(Exchange::Failed(format!("response head larger than {MAX_HEAD_BYTES} bytes")));
                    }
                    None => {}
                }
            }
            match self.fill().await {
                Ok(0) => return Err(broken(answered, "agent closed the connection before the response".to_owned())),
                Ok(_) => answered = true,
                Err(error) => return Err(broken(answered, error.to_string())),
            }
        }
    }

    /// `Some(true)`: a final head was parsed; `Some(false)`: a 1xx head was
    /// skipped; `None`: more bytes needed.
    fn parse_head(&mut self) -> Result<Option<bool>, String> {
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut response = httparse::Response::new(&mut headers);
        let length = match response.parse(self.unread()).map_err(|error| format!("bad response head: {error}"))? {
            httparse::Status::Complete(length) => length,
            httparse::Status::Partial => return Ok(None),
        };
        let status = response.code.unwrap_or(0);
        if (100..200).contains(&status) {
            self.start += length;
            return Ok(Some(false));
        }
        let mut chunked = false;
        let mut content_length = None;
        let mut close = response.version == Some(0);
        for header in response.headers.iter() {
            let value = std::str::from_utf8(header.value).unwrap_or_default().trim();
            if header.name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.rsplit(',').next().is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"));
            } else if header.name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.parse::<u64>().map_err(|_| format!("bad content-length {value:?}"))?);
            } else if header.name.eq_ignore_ascii_case("connection") {
                close |= value.split(',').any(|token| token.trim().eq_ignore_ascii_case("close"));
            }
        }
        self.start += length;
        self.status = status;
        self.framing = match (chunked, content_length) {
            (true, _) => Framing::Chunked(Chunk::Size),
            (false, Some(0)) => Framing::Done,
            (false, Some(length)) => Framing::Length(length),
            (false, None) if status == 204 || status == 304 => Framing::Done,
            (false, None) => {
                close = true;
                Framing::UntilClose
            }
        };
        self.keep_alive = !close;
        Ok(Some(true))
    }

    /// The next piece of body data, borrowed from the read buffer; `Ok(None)`
    /// at the end of the body. A finished keep-alive connection goes back to
    /// the idle pool.
    ///
    /// # Errors
    /// A broken connection, a malformed chunk or a body cut short.
    pub async fn next_data(&mut self) -> io::Result<Option<&[u8]>> {
        loop {
            match self.framing {
                Framing::Done => {
                    self.release();
                    return Ok(None);
                }
                Framing::Length(0) => self.framing = Framing::Done,
                Framing::Length(left) => {
                    if self.unread().is_empty() && self.fill().await? == 0 {
                        return Err(cut_short());
                    }
                    let take = (self.unread().len() as u64).min(left) as usize;
                    self.framing = Framing::Length(left - take as u64);
                    return Ok(Some(self.advance(take)));
                }
                Framing::UntilClose => {
                    if self.unread().is_empty() && self.fill().await? == 0 {
                        self.framing = Framing::Done;
                        self.keep_alive = false;
                        continue;
                    }
                    let take = self.unread().len();
                    return Ok(Some(self.advance(take)));
                }
                Framing::Chunked(Chunk::Data(left)) => {
                    if self.unread().is_empty() && self.fill().await? == 0 {
                        return Err(cut_short());
                    }
                    let take = (self.unread().len() as u64).min(left) as usize;
                    let left = left - take as u64;
                    self.framing = Framing::Chunked(if left == 0 { Chunk::DataEnd } else { Chunk::Data(left) });
                    return Ok(Some(self.advance(take)));
                }
                Framing::Chunked(state) => {
                    let Some(end) = self.unread().iter().position(|byte| *byte == b'\n') else {
                        if self.unread().len() > MAX_HEAD_BYTES {
                            return Err(invalid("chunk line too long"));
                        }
                        if self.fill().await? == 0 {
                            return Err(cut_short());
                        }
                        continue;
                    };
                    let next = {
                        let line = self.advance(end + 1);
                        let line = line.strip_suffix(b"\n").unwrap_or(line);
                        let line = line.strip_suffix(b"\r").unwrap_or(line);
                        if state == Chunk::Trailer && line.is_empty() {
                            Framing::Done
                        } else {
                            Framing::Chunked(next_chunk_state(state, line)?)
                        }
                    };
                    self.framing = next;
                }
            }
        }
    }

    /// Reads the rest of the body (at most `limit` bytes) so that a
    /// keep-alive connection goes back to the pool: a run's stream ends right
    /// after its terminal event, which the reader stops at. `true` when the
    /// connection was kept.
    pub async fn drain(mut self, limit: usize) -> bool {
        let mut read = 0usize;
        loop {
            match self.next_data().await {
                Ok(Some(data)) => {
                    read += data.len();
                    if read > limit {
                        return false;
                    }
                }
                Ok(None) => return self.socket.is_none(),
                Err(_) => return false,
            }
        }
    }

    fn advance(&mut self, count: usize) -> &[u8] {
        let start = self.start;
        self.start += count;
        self.buffer.get(start..self.start).unwrap_or_default()
    }

    /// Hands a finished keep-alive connection back to the pool (once).
    fn release(&mut self) {
        if !self.keep_alive || self.start != self.buffer.len() {
            return;
        }
        if let (Some(socket), Some((pool, target))) = (self.socket.take(), self.pool.take()) {
            pool.put(target, socket);
        }
    }
}

fn next_chunk_state(state: Chunk, line: &[u8]) -> io::Result<Chunk> {
    match state {
        Chunk::Size => {
            let size = line.split(|byte| *byte == b';').next().unwrap_or_default();
            let size = std::str::from_utf8(size).map_err(|_| invalid("bad chunk size"))?.trim();
            let size = u64::from_str_radix(size, 16).map_err(|_| invalid("bad chunk size"))?;
            Ok(if size == 0 { Chunk::Trailer } else { Chunk::Data(size) })
        }
        Chunk::DataEnd if line.is_empty() => Ok(Chunk::Size),
        Chunk::DataEnd => Err(invalid("missing CRLF after chunk data")),
        Chunk::Trailer => Ok(Chunk::Trailer),
        Chunk::Data(_) => Err(invalid("chunk decoder out of sync")),
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn cut_short() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "agent closed the stream mid-body")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::net::TcpListener;

    fn address(listener: &TcpListener) -> EndpointAddress {
        EndpointAddress::Http(format!("http://{}", listener.local_addr().unwrap()))
    }

    /// Reads one request head (+ content-length body) and returns it as text.
    async fn read_request(reader: &mut BufReader<TcpStream>) -> Option<String> {
        let mut head = String::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.ok()? == 0 {
                return None;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            head.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).await.ok()?;
        head.push_str(&String::from_utf8(body).unwrap());
        Some(head)
    }

    async fn collect(body: &mut ResponseBody) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(data) = body.next_data().await? {
            out.extend_from_slice(data);
        }
        Ok(out)
    }

    fn post<'a>(body: &'a [u8]) -> Request<'a> {
        Request { method: "POST", path: "/internal/v1/runs", headers: &[("graphrt-run-id", "r1")], body }
    }

    #[test]
    fn targets() {
        let target = Target::of(&EndpointAddress::Http("http://10.0.0.7:8080".into())).unwrap();
        assert_eq!(target.socket, SocketTarget::Tcp("10.0.0.7:8080".into()));
        assert_eq!(target.host, "10.0.0.7:8080");
        assert_eq!(target.prefix, "");
        let target = Target::of(&EndpointAddress::Http("http://agent.internal/base/".into())).unwrap();
        assert_eq!(target.socket, SocketTarget::Tcp("agent.internal:80".into()));
        assert_eq!(target.prefix, "/base");
        let target = Target::of(&EndpointAddress::Http("http://[::1]:9000".into())).unwrap();
        assert_eq!(target.socket, SocketTarget::Tcp("[::1]:9000".into()));
        let target = Target::of(&EndpointAddress::Http("http://[::1]".into())).unwrap();
        assert_eq!(target.socket, SocketTarget::Tcp("[::1]:80".into()));
        assert!(Target::of(&EndpointAddress::Http("https://run.app".into())).is_none());
        assert!(Target::of(&EndpointAddress::Http("http://user@host".into())).is_none());
        let unix = Target::of(&EndpointAddress::Unix("/tmp/a.sock".into())).unwrap();
        assert_eq!(unix.host, "agent");
    }

    #[test]
    fn request_encoding() {
        let target = Target::of(&EndpointAddress::Http("http://h:1/p".into())).unwrap();
        let encoded = String::from_utf8(encode_request(&target, post(b"{}"))).unwrap();
        assert_eq!(
            encoded,
            "POST /p/internal/v1/runs HTTP/1.1\r\nhost: h:1\r\ngraphrt-run-id: r1\r\ncontent-length: 2\r\ncontent-type: application/json\r\n\r\n{}"
        );
        let get = Request { method: "GET", path: "/x?from=3", headers: &[], body: b"" };
        assert_eq!(
            String::from_utf8(encode_request(&target, get)).unwrap(),
            "GET /p/x?from=3 HTTP/1.1\r\nhost: h:1\r\n\r\n"
        );
    }

    /// Chunked body split at every awkward boundary, then reuse of the connection.
    #[tokio::test]
    async fn chunked_stream_then_keep_alive_reuse() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::of(&address(&listener)).unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let first = read_request(&mut reader).await.unwrap();
            assert!(first.starts_with("POST /internal/v1/runs HTTP/1.1\r\n"), "{first}");
            assert!(first.ends_with("{\"a\":1}"));
            let response = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n6;ext=1\r\nevent:\r\n3\r\n x\n\r\n0\r\nx-trailer: 1\r\n\r\n";
            for byte in response {
                reader.get_mut().write_all(&[*byte]).await.unwrap();
            }
            // Same connection, second request.
            let second = read_request(&mut reader).await.unwrap();
            assert!(second.starts_with("GET /again HTTP/1.1\r\n"), "{second}");
            reader.get_mut().write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await.unwrap();
        });
        let pool = Arc::new(IdlePool::default());
        let mut body = open(&pool, &target, post(b"{\"a\":1}")).await.unwrap();
        assert_eq!(body.status(), 200);
        assert_eq!(collect(&mut body).await.unwrap(), b"event: x\n");
        assert_eq!(pool.idle_count(), 1, "a finished keep-alive stream returns its connection");
        let again = Request { method: "GET", path: "/again", headers: &[], body: b"" };
        let mut body = open(&pool, &target, again).await.unwrap();
        assert_eq!(collect(&mut body).await.unwrap(), b"ok");
        server.await.unwrap();
    }

    /// A reader that stopped at the last event hands the connection back once
    /// the agent ended the body.
    #[tokio::test]
    async fn drained_stream_returns_its_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::of(&address(&listener)).unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            read_request(&mut reader).await.unwrap();
            let socket = reader.get_mut();
            socket.write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\nlast\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            socket.write_all(b"0\r\n\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let pool = Arc::new(IdlePool::default());
        let mut body = open(&pool, &target, post(b"{}")).await.unwrap();
        assert_eq!(body.next_data().await.unwrap().unwrap(), b"last");
        assert!(body.drain(1024).await);
        assert_eq!(pool.idle_count(), 1);
    }

    /// The local provider's agents listen on named pipes on Windows.
    #[cfg(windows)]
    #[tokio::test]
    async fn named_pipe_stream() {
        use tokio::net::windows::named_pipe::ServerOptions;
        let name = format!(r"\\.\pipe\graphrt-stream-conn-{}", std::process::id());
        let server = ServerOptions::new().first_pipe_instance(true).create(&name).unwrap();
        tokio::spawn(async move {
            let mut server = server;
            server.connect().await.unwrap();
            let mut buffer = vec![0u8; 4096];
            let read = server.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            assert!(request.starts_with("GET /internal/v1/hello HTTP/1.1\r\nhost: agent\r\n"), "{request}");
            server.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await.unwrap();
            // Keep the pipe open until the client is done with it.
            let _ = server.read(&mut buffer).await;
        });
        let target = Target::of(&EndpointAddress::Pipe(name.clone())).unwrap();
        let pool = Arc::new(IdlePool::default());
        let request = Request { method: "GET", path: "/internal/v1/hello", headers: &[], body: b"" };
        let mut body = open(&pool, &target, request).await.unwrap();
        assert_eq!(body.status(), 200);
        assert_eq!(collect(&mut body).await.unwrap(), b"ok");
        let gone = Target::of(&EndpointAddress::Pipe(format!("{name}-gone"))).unwrap();
        let request = Request { method: "GET", path: "/internal/v1/hello", headers: &[], body: b"" };
        assert!(matches!(open(&pool, &gone, request).await, Err(OpenError::Unreachable(_))));
    }

    /// The local provider's agents listen on Unix sockets.
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_stream() {
        let path = std::env::temp_dir().join(format!("graphrt-stream-conn-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            assert!(
                request.starts_with("GET /internal/v1/runs/r/stream?from=2 HTTP/1.1\r\nhost: agent\r\n"),
                "{request}"
            );
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n")
                .await
                .unwrap();
        });
        let target = Target::of(&EndpointAddress::Unix(path.clone())).unwrap();
        let pool = Arc::new(IdlePool::default());
        let request = Request { method: "GET", path: "/internal/v1/runs/r/stream?from=2", headers: &[], body: b"" };
        let mut body = open(&pool, &target, request).await.unwrap();
        assert_eq!(collect(&mut body).await.unwrap(), b"abc");
        assert_eq!(pool.idle_count(), 1);
        pool.forget(&EndpointAddress::Unix(path.clone()));
        assert_eq!(pool.idle_count(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn refusals_carry_status_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::of(&address(&listener)).unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            read_request(&mut reader).await.unwrap();
            let body = r#"{"error_type":"epoch_conflict"}"#;
            let response = format!("HTTP/1.1 409 Conflict\r\ncontent-length: {}\r\n\r\n{body}", body.len());
            reader.get_mut().write_all(response.as_bytes()).await.unwrap();
        });
        let pool = Arc::new(IdlePool::default());
        match open(&pool, &target, post(b"{}")).await {
            Err(OpenError::Refused { status: 409, body }) => assert!(body.contains("epoch_conflict")),
            other => panic!("{other:?}"),
        }
    }

    /// A kept connection the agent closed meanwhile is never used: the request
    /// goes out on a new connection.
    #[tokio::test]
    async fn closed_idle_connection_is_replaced() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::of(&address(&listener)).unwrap();
        tokio::spawn(async move {
            // First connection: answer, then close it while idle.
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            read_request(&mut reader).await.unwrap();
            reader.get_mut().write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\na").await.unwrap();
            drop(reader);
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            read_request(&mut reader).await.unwrap();
            reader.get_mut().write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\nb").await.unwrap();
        });
        let pool = Arc::new(IdlePool::default());
        let mut body = open(&pool, &target, post(b"{}")).await.unwrap();
        assert_eq!(collect(&mut body).await.unwrap(), b"a");
        assert_eq!(pool.idle_count(), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut body = open(&pool, &target, post(b"{}")).await.unwrap();
        assert_eq!(collect(&mut body).await.unwrap(), b"b");
    }

    #[tokio::test]
    async fn until_close_bodies_and_cut_streams() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::of(&address(&listener)).unwrap();
        tokio::spawn(async move {
            for response in [
                &b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\nall of it"[..],
                &b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nab"[..],
                &b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\nzz\r\n"[..],
            ] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                read_request(&mut reader).await.unwrap();
                reader.get_mut().write_all(response).await.unwrap();
            }
        });
        let pool = Arc::new(IdlePool::default());
        let mut body = open(&pool, &target, post(b"{}")).await.unwrap();
        assert_eq!(collect(&mut body).await.unwrap(), b"all of it");
        assert_eq!(pool.idle_count(), 0, "a closed connection is not kept");
        let mut body = open(&pool, &target, post(b"{}")).await.unwrap();
        assert_eq!(collect(&mut body).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let mut body = open(&pool, &target, post(b"{}")).await.unwrap();
        assert_eq!(collect(&mut body).await.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn unreachable_and_oversized_heads() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::of(&address(&listener)).unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            read_request(&mut reader).await.unwrap();
            let mut head = b"HTTP/1.1 200 OK\r\n".to_vec();
            while head.len() <= MAX_HEAD_BYTES {
                head.extend_from_slice(b"x-filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
            }
            let _ = reader.get_mut().write_all(&head).await;
        });
        let pool = Arc::new(IdlePool::default());
        assert!(matches!(open(&pool, &target, post(b"{}")).await, Err(OpenError::Unreachable(_))));
        let gone = Target::of(&EndpointAddress::Unix("/nonexistent/graphrt-agent.sock".into())).unwrap();
        assert!(matches!(open(&pool, &gone, post(b"{}")).await, Err(OpenError::Unreachable(_))));
    }

    /// The read buffer stays small for small frames and gives a large event's
    /// memory back once it drained.
    #[tokio::test]
    async fn read_buffer_grows_for_large_events_then_shrinks() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target::of(&address(&listener)).unwrap();
        let large = vec![b'x'; 200 * 1024];
        let sent = large.clone();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            read_request(&mut reader).await.unwrap();
            let socket = reader.get_mut();
            socket.write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n").await.unwrap();
            socket.write_all(format!("{:x}\r\n", sent.len()).as_bytes()).await.unwrap();
            socket.write_all(&sent).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            socket.write_all(b"2\r\nhi\r\n0\r\n\r\n").await.unwrap();
        });
        let pool = Arc::new(IdlePool::default());
        let mut body = open(&pool, &target, post(b"{}")).await.unwrap();
        assert!(body.buffer.capacity() <= INITIAL_READ_BUFFER * 2);
        let mut received = Vec::new();
        let mut peak = 0;
        while received.len() < large.len() {
            let data = body.next_data().await.unwrap().unwrap();
            received.extend_from_slice(data);
            peak = peak.max(body.buffer.capacity());
        }
        assert_eq!(received, large);
        assert!(peak > INITIAL_READ_BUFFER * 4, "a large event reads in larger pieces: {peak}");
        assert!(peak <= MAX_READ_BUFFER * 2, "{peak}");
        assert_eq!(body.next_data().await.unwrap().unwrap(), b"hi");
        assert!(body.buffer.capacity() <= INITIAL_READ_BUFFER * 4, "{}", body.buffer.capacity());
        assert!(body.next_data().await.unwrap().is_none());
    }
}
