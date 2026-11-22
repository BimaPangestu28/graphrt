//! Listening sockets of `graphrt serve`: TCP (`host:port`) or a Unix socket
//! (`unix:/path/to.sock`, `--listen`).
//!
//! A Unix socket is how a gateway on the same host (the `local-process`
//! provider) reaches its agents: no port to allocate, no network exposure, and
//! access controlled by file permissions (the socket is created mode 0600).

use std::future::Future;
use std::path::PathBuf;

use crate::error::CliError;

/// A bound listening socket.
#[derive(Debug)]
pub enum Listener {
    Tcp(tokio::net::TcpListener),
    #[cfg(unix)]
    Unix {
        listener: tokio::net::UnixListener,
        path: PathBuf,
    },
}

/// The socket path of a `unix:` address, if it is one.
pub fn unix_path(addr: &str) -> Option<PathBuf> {
    addr.strip_prefix("unix:").filter(|p| !p.is_empty()).map(PathBuf::from)
}

impl Listener {
    /// Bind `addr` (`host:port` or `unix:/path`). A stale socket file left at the
    /// path by an earlier run is replaced; any other file there is an error.
    ///
    /// @throws CliError - Usage error when the address cannot be bound.
    pub async fn bind(addr: &str) -> Result<Self, CliError> {
        if addr.starts_with("unix:") {
            let path = unix_path(addr).ok_or_else(|| CliError::usage("`unix:` needs a socket path"))?;
            return Self::bind_unix(path);
        }
        tokio::net::TcpListener::bind(addr)
            .await
            .map(Self::Tcp)
            .map_err(|e| CliError::usage(format!("cannot listen on {addr}: {e}")))
    }

    #[cfg(unix)]
    fn bind_unix(path: PathBuf) -> Result<Self, CliError> {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        let shown = path.display().to_string();
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if !meta.file_type().is_socket() {
                return Err(CliError::usage(format!(
                    "cannot listen on unix:{shown}: a file that is not a socket is there"
                )));
            }
            std::fs::remove_file(&path)
                .map_err(|e| CliError::usage(format!("cannot replace the stale socket unix:{shown}: {e}")))?;
        }
        let listener = tokio::net::UnixListener::bind(&path)
            .map_err(|e| CliError::usage(format!("cannot listen on unix:{shown}: {e}")))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| CliError::usage(format!("cannot restrict unix:{shown} to its owner: {e}")))?;
        Ok(Self::Unix { listener, path })
    }

    #[cfg(not(unix))]
    fn bind_unix(_path: PathBuf) -> Result<Self, CliError> {
        Err(CliError::usage("Unix socket listeners need a Unix platform"))
    }

    /// Whether only this host can connect (loopback TCP, or any Unix socket).
    pub fn is_local(&self) -> bool {
        match self {
            Self::Tcp(listener) => listener.local_addr().map(|a| a.ip().is_loopback()).unwrap_or(false),
            #[cfg(unix)]
            Self::Unix { .. } => true,
        }
    }

    /// Whether requests can come from a web browser on this host, so a loopback
    /// server without an API key needs DNS rebinding protection (TCP only).
    pub fn is_browser_reachable(&self) -> bool {
        matches!(self, Self::Tcp(_))
    }

    /// Serve `app` until `shutdown` resolves. TCP connections get `TCP_NODELAY`:
    /// without it, the second and later small writes of a streamed (SSE) response
    /// wait for the client's delayed ACK (about 40 ms per run on Linux). A Unix
    /// socket file is removed afterwards.
    ///
    /// Both kinds go through one [`axum::serve`] (one copy of the HTTP stack in
    /// the binary, not one per socket type).
    ///
    /// @throws std::io::Error - When the server fails.
    pub async fn serve(
        self,
        app: axum::Router,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        #[cfg(unix)]
        let socket_file = match &self {
            Self::Unix { path, .. } => Some(path.clone()),
            Self::Tcp(_) => None,
        };
        let result = axum::serve(self, app).with_graceful_shutdown(shutdown).await;
        #[cfg(unix)]
        if let Some(path) = socket_file {
            let _removed = std::fs::remove_file(&path);
        }
        result
    }
}

/// An accepted connection of a [`Listener`].
#[derive(Debug)]
pub enum Connection {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

/// Pause after an accept error that is not about one connection (out of file
/// descriptors...), as axum's own listeners do, instead of spinning.
const ACCEPT_ERROR_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

async fn accept_error(e: std::io::Error) {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
    if !matches!(e.kind(), ConnectionRefused | ConnectionAborted | ConnectionReset) {
        eprintln!("warning: accept error: {e}");
        tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
    }
}

impl axum::serve::Listener for Listener {
    type Io = Connection;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let accepted = match self {
                Self::Tcp(listener) => tokio::net::TcpListener::accept(listener).await.map(|(tcp, _)| {
                    if let Err(e) = tcp.set_nodelay(true) {
                        eprintln!("warning: cannot set TCP_NODELAY: {e}");
                    }
                    Connection::Tcp(tcp)
                }),
                #[cfg(unix)]
                Self::Unix { listener, .. } => {
                    tokio::net::UnixListener::accept(listener).await.map(|(unix, _)| Connection::Unix(unix))
                }
            };
            match accepted {
                Ok(connection) => return (connection, ()),
                Err(e) => accept_error(e).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(())
    }
}

macro_rules! each_connection {
    ($self:ident, $stream:ident => $body:expr) => {
        match $self.get_mut() {
            Connection::Tcp($stream) => $body,
            #[cfg(unix)]
            Connection::Unix($stream) => $body,
        }
    };
}

impl tokio::io::AsyncRead for Connection {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        each_connection!(self, stream => std::pin::Pin::new(stream).poll_read(cx, buf))
    }
}

impl tokio::io::AsyncWrite for Connection {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        each_connection!(self, stream => std::pin::Pin::new(stream).poll_write(cx, buf))
    }

    fn poll_write_vectored(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        each_connection!(self, stream => std::pin::Pin::new(stream).poll_write_vectored(cx, bufs))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        each_connection!(self, stream => std::pin::Pin::new(stream).poll_flush(cx))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        each_connection!(self, stream => std::pin::Pin::new(stream).poll_shutdown(cx))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn unix_addresses_are_recognized() {
        assert_eq!(unix_path("unix:/run/a.sock"), Some(PathBuf::from("/run/a.sock")));
        assert_eq!(unix_path("unix:"), None);
        assert_eq!(unix_path("127.0.0.1:8080"), None);
    }

    #[tokio::test]
    async fn binds_a_private_unix_socket_and_replaces_a_stale_one() {
        let dir = std::env::temp_dir().join(format!("graphrt-listen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.sock");
        let addr = format!("unix:{}", path.display());
        let first = Listener::bind(&addr).await.unwrap();
        assert!(first.is_local() && !first.is_browser_reachable());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(first);
        // The socket file outlives a crashed server; binding again replaces it.
        assert!(Listener::bind(&addr).await.is_ok());
        let not_a_socket = dir.join("file");
        std::fs::write(&not_a_socket, "x").unwrap();
        assert!(Listener::bind(&format!("unix:{}", not_a_socket.display())).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
