//! HTTPS without a proxy: a TLS listener for `axum::serve` (rustls, ring).
//!
//! Handshakes run in their own tasks (at most [`MAX_HANDSHAKES`] at once,
//! each within [`HANDSHAKE_TIMEOUT`]), so a slow client cannot hold up others.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::connect_info::Connected;
use axum::serve::{IncomingStream, Listener};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use super::error::{ServerError, ServerResult};

/// Handshakes in progress at most.
pub const MAX_HANDSHAKES: usize = 256;
/// The longest a handshake may take.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A TCP listener that hands out TLS streams.
#[derive(Debug)]
pub struct TlsListener {
    local: SocketAddr,
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

/// The rustls server config of a PEM certificate chain and key.
///
/// @throws ServerError::Config - When the files cannot be read or do not hold a certificate and key.
pub fn server_config(cert: &Path, key: &Path) -> ServerResult<rustls::ServerConfig> {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|error| ServerError::Config(format!("tls.cert {}: {error}", cert.display())))?
        .collect::<Result<_, _>>()
        .map_err(|error| ServerError::Config(format!("tls.cert {}: {error}", cert.display())))?;
    if certs.is_empty() {
        return Err(ServerError::Config(format!("tls.cert {} holds no certificate", cert.display())));
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|error| ServerError::Config(format!("tls.key {}: {error}", key.display())))?;
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| ServerError::Config(format!("tls: {error}")))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

impl TlsListener {
    /// Accept TLS connections on `tcp` with `config`.
    pub fn new(tcp: TcpListener, config: rustls::ServerConfig) -> ServerResult<Self> {
        let local = tcp.local_addr().map_err(|error| ServerError::Io(error.to_string()))?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let (tx, ready) = mpsc::channel(64);
        let permits = Arc::new(Semaphore::new(MAX_HANDSHAKES));
        tokio::spawn(async move {
            loop {
                let (stream, addr) = match tcp.accept().await {
                    Ok(accepted) => accepted,
                    Err(_) => {
                        // Out of file descriptors and the like: wait a little, as axum does.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&permits).acquire_owned().await else { return };
                if tx.is_closed() {
                    return;
                }
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Ok(Ok(tls)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        let _closed = tx.send((tls, addr)).await;
                    }
                });
            }
        });
        Ok(Self { local, ready })
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(accepted) => accepted,
            // The accept task ended (the runtime is shutting down): never return.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// The address of the other side of a connection, plain or TLS.
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);

impl Connected<IncomingStream<'_, TcpListener>> for PeerAddr {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

impl Connected<IncomingStream<'_, TlsListener>> for PeerAddr {
    fn connect_info(stream: IncomingStream<'_, TlsListener>) -> Self {
        Self(*stream.remote_addr())
    }
}
