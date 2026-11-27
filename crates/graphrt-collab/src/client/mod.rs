//! The client (cargo feature `client`): what an app needs to work on shared
//! projects. [`Api`] checks a server, signs in and manages workspaces,
//! [`LiveProject`] keeps a local replica of one project in sync, and
//! [`LoopbackLogin`] runs the browser sign in of a desktop app (RFC 8252).

mod api;
mod live;

pub use api::{Api, Pkce, Redeemed, ServerInfo, SignIn, Tokens, normalize_url, random_text};
pub use live::{LiveEvent, LiveOptions, LiveProject, Peer, TokenSource};

/// What can go wrong on the client side, worded so an app can tell people what to do.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The address is not usable (not https, has a query...).
    #[error("{0}")]
    Address(String),
    /// Nothing answers at the address (wrong host or port, no network, firewall).
    #[error("the server cannot be reached: {0}")]
    Unreachable(String),
    /// The server's certificate is not valid for its name, expired or unknown.
    #[error("the server's TLS certificate is not valid: {0}")]
    Tls(String),
    /// Something answers, but it is not a Graphrt collaboration server.
    #[error("this address is not a Graphrt collaboration server")]
    NotCollab,
    /// The server refused the request.
    #[error("the server answered {status}: {message}")]
    Http { status: u16, message: String },
    /// The live session ended.
    #[error("the live session is closed")]
    Closed,
    /// The content was refused locally.
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Internal(String),
}

impl ClientError {
    /// Whether signing in again would help (401).
    pub fn needs_sign_in(&self) -> bool {
        matches!(self, ClientError::Http { status: 401, .. })
    }
}

impl From<crate::CollabError> for ClientError {
    fn from(error: crate::CollabError) -> Self {
        ClientError::Invalid(error.to_string())
    }
}

/// The result of the client.
pub type ClientResult<T> = std::result::Result<T, ClientError>;

/// Use ring as rustls' crypto provider (the one the rest of graphrt uses), once.
pub(crate) fn install_tls_provider() {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
}

/// The browser sign in of a desktop app: listen on a loopback port, send the
/// browser to [`Self::url`], wait for the server to send it back with a code.
#[derive(Debug)]
pub struct LoopbackLogin {
    listener: tokio::net::TcpListener,
    pub redirect_uri: String,
    pub state: String,
    pub pkce: Pkce,
}

impl LoopbackLogin {
    /// Listen on a free port on 127.0.0.1.
    ///
    /// @throws ClientError::Internal - When no port can be opened.
    pub async fn start() -> ClientResult<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| ClientError::Internal(format!("cannot listen for the sign in: {error}")))?;
        let port = listener.local_addr().map_err(|e| ClientError::Internal(e.to_string()))?.port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
            state: random_text()?,
            pkce: Pkce::new()?,
        })
    }

    /// Where to open the browser.
    pub fn url(&self, api: &Api, device: &str) -> ClientResult<String> {
        api.login_url(&self.redirect_uri, &self.state, &self.pkce, device)
    }

    /// Wait for the browser to come back (at most `timeout`), then trade the code for tokens.
    ///
    /// @throws ClientError - When the sign in was refused, timed out or the state does not match.
    pub async fn finish(self, api: &Api, timeout: std::time::Duration) -> ClientResult<Tokens> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let accepted = tokio::time::timeout_at(deadline, self.listener.accept())
                .await
                .map_err(|_| ClientError::Internal("the sign in took too long".into()))?;
            let Ok((mut stream, _)) = accepted else { continue };
            let mut buffer = vec![0u8; 8192];
            let read = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buffer)).await;
            let Ok(Ok(n)) = read else { continue };
            let request = String::from_utf8_lossy(buffer.get(..n).unwrap_or_default()).into_owned();
            let Some(target) = request.lines().next().and_then(|line| line.split_whitespace().nth(1)) else { continue };
            let Some(query) = target.strip_prefix("/callback?") else {
                let _ =
                    stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                continue;
            };
            let params: std::collections::HashMap<String, String> = reqwest::Url::parse(&format!("http://x/?{query}"))
                .map(|url| url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
                .unwrap_or_default();
            let done = params.get("state") == Some(&self.state);
            let (status, page) = match (done, params.get("code"), params.get("error")) {
                (true, Some(_), _) => ("200 OK", "Signed in. You can close this tab and go back to Graphrt."),
                _ => ("400 Bad Request", "The sign in did not finish. Go back to Graphrt and try again."),
            };
            let body = format!("<!doctype html><meta charset=utf-8><title>Graphrt</title><p>{page}</p>");
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            if !done {
                continue;
            }
            if let Some(error) = params.get("error") {
                let why = params.get("error_description").cloned().unwrap_or_else(|| error.clone());
                return Err(ClientError::Http { status: 403, message: why });
            }
            let code = params.get("code").ok_or_else(|| ClientError::Internal("no code".into()))?;
            return api.exchange_code(code, &self.pkce, &self.redirect_uri).await;
        }
    }
}
