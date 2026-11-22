//! Minimal HTTP/1.1 client for the agent's reports to its gateway.
//!
//! The gateway is reached over a private network or a Unix socket, never the
//! internet, and requests are authenticated by their HMAC signature, so plain
//! HTTP is enough. Writing the request by hand keeps `serve` free of an HTTP
//! client dependency (the slim agent build has none of its own).

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::listen::Connection;

/// Where the gateway listens: `http://host:port[/base]` or `unix:/path/to.sock`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayAddress {
    Tcp { authority: String, base_path: String },
    Unix { path: std::path::PathBuf },
}

/// Time allowed for one report (connect, send, read the status line).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

impl GatewayAddress {
    /// Parse `--gateway-url`.
    ///
    /// @throws String - For `https://` (agents talk to their gateway over a private
    /// network or socket) or anything that is not an `http://` URL or `unix:` path.
    pub fn parse(text: &str) -> Result<Self, String> {
        if let Some(path) = text.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("`unix:` needs a socket path".to_owned());
            }
            return Ok(Self::Unix { path: path.into() });
        }
        let Some(rest) = text.strip_prefix("http://") else {
            return Err(format!(
                "gateway URL `{text}` must be http://host:port or unix:/path (private network or socket; \
                 requests are HMAC-signed)"
            ));
        };
        let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
        if authority.is_empty() || authority.contains('@') {
            return Err(format!("gateway URL `{text}` needs a host and no credentials"));
        }
        let base_path = path.trim_end_matches('/');
        let base_path = if base_path.is_empty() { String::new() } else { format!("/{base_path}") };
        Ok(Self::Tcp { authority: authority.to_owned(), base_path })
    }

    /// The request path of `path` (which starts with `/`) on this gateway.
    pub fn path_of(&self, path: &str) -> String {
        match self {
            Self::Tcp { base_path, .. } => format!("{base_path}{path}"),
            Self::Unix { .. } => path.to_owned(),
        }
    }

    fn host(&self) -> &str {
        match self {
            Self::Tcp { authority, .. } => authority,
            Self::Unix { .. } => "localhost",
        }
    }
}

/// A `POST` of JSON `body` to `request_path` with extra `headers`, as bytes.
/// Header values must not contain line breaks (callers pass validated ids,
/// numbers and signatures).
pub fn request(address: &GatewayAddress, request_path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + body.len());
    for part in ["POST ", request_path, " HTTP/1.1\r\nhost: ", address.host(), "\r\ncontent-type: application/json"] {
        out.extend_from_slice(part.as_bytes());
    }
    out.extend_from_slice(format!("\r\ncontent-length: {}\r\nconnection: close\r\n", body.len()).as_bytes());
    for (name, value) in headers {
        for part in [*name, ": ", *value, "\r\n"] {
            out.extend_from_slice(part.as_bytes());
        }
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// Send `request` (from [`request`]) and return the response status code.
///
/// @throws std::io::Error - On connection or I/O errors, a reply without a
/// status line, or after the timeout.
pub async fn send(address: &GatewayAddress, request: &[u8]) -> std::io::Result<u16> {
    let exchange = async {
        // One connection type for both socket kinds: one copy of the exchange.
        let mut stream = match address {
            GatewayAddress::Tcp { authority, .. } => {
                Connection::Tcp(tokio::net::TcpStream::connect(authority.as_str()).await?)
            }
            #[cfg(unix)]
            GatewayAddress::Unix { path } => Connection::Unix(tokio::net::UnixStream::connect(path).await?),
            #[cfg(not(unix))]
            GatewayAddress::Unix { .. } => return Err(std::io::ErrorKind::Unsupported.into()),
        };
        stream.write_all(request).await?;
        // Only the status line matters; read until it is complete.
        let mut buf = Vec::with_capacity(64);
        let mut chunk = [0u8; 256];
        while !buf.contains(&b'\n') && buf.len() < 4096 {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        }
        parse_status(&buf).ok_or_else(|| std::io::ErrorKind::InvalidData.into())
    };
    tokio::time::timeout(REQUEST_TIMEOUT, exchange).await.unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))
}

fn parse_status(response: &[u8]) -> Option<u16> {
    let line = response.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split_whitespace();
    parts.next().filter(|v| v.starts_with("HTTP/1."))?;
    parts.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_addresses_are_parsed() {
        assert_eq!(
            GatewayAddress::parse("http://gw.internal:9000").unwrap(),
            GatewayAddress::Tcp { authority: "gw.internal:9000".into(), base_path: String::new() }
        );
        let with_base = GatewayAddress::parse("http://10.0.0.2:9000/gw/").unwrap();
        assert_eq!(with_base.path_of("/internal/v1/orphans/r"), "/gw/internal/v1/orphans/r");
        assert_eq!(
            GatewayAddress::parse("unix:/run/gw.sock").unwrap(),
            GatewayAddress::Unix { path: "/run/gw.sock".into() }
        );
        for bad in ["https://gw:1", "gw:1", "http://", "http://u:p@gw:1", "unix:"] {
            assert!(GatewayAddress::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn status_lines_are_parsed() {
        assert_eq!(parse_status(b"HTTP/1.1 202 Accepted\r\n"), Some(202));
        assert_eq!(parse_status(b"HTTP/1.0 500\r\n"), Some(500));
        assert_eq!(parse_status(b"SSH-2.0\r\n"), None);
        assert_eq!(parse_status(b""), None);
    }

    #[tokio::test]
    async fn posts_to_a_tcp_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut chunk = [0u8; 1024];
            while !String::from_utf8_lossy(&received).contains("{\"ok\":1}") {
                let n = socket.read(&mut chunk).await.unwrap();
                received.extend_from_slice(&chunk[..n]);
            }
            socket.write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n").await.unwrap();
            String::from_utf8(received).unwrap()
        });
        let address = GatewayAddress::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let bytes = request(&address, "/internal/v1/orphans/r1", &[("graphrt-sig", "v1;x")], b"{\"ok\":1}");
        let status = send(&address, &bytes).await.unwrap();
        assert_eq!(status, 202);
        let received = server.await.unwrap();
        assert!(received.starts_with("POST /internal/v1/orphans/r1 HTTP/1.1\r\n"), "{received}");
        assert!(received.contains("graphrt-sig: v1;x\r\n") && received.contains("content-length: 8\r\n"), "{received}");
    }
}
