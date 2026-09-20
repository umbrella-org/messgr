//! mTLS termination for `messgr-ingest` (DESIGN.md §4.9, §11.1, T-011
//! decisions 7/9/10). Chain-of-trust validation happens entirely here, at
//! the TLS layer — `rustls`'s client-cert verifier rejects, at the
//! handshake, any certificate that doesn't chain to the configured client
//! CA. `producer::resolve::resolve_producer` (T-006) only ever sees a
//! subject string that has *already* been authenticated this way; it maps
//! that authenticated identity to `(tenant_id, producer_id)` and never
//! re-checks the chain (`producer_cert` stores no fingerprint or public
//! key — see `src/producer/cert_repo.rs`).
//!
//! Server and client-CA material both come from PEM files named by env
//! vars, identically in dev and production — no dev-only branch here. The
//! local dev workflow is `just dev-pki-issue-cert <name> <dir>`, which
//! writes `cert.pem`/`key.pem`/`ca.pem` from the same root CA every
//! producer's own dev certificate already chains to.

use std::fs::File;
use std::io::{self, BufReader};
use std::pin::Pin;
use std::sync::Arc;

use axum::Extension;
use axum::middleware::AddExtension;
use axum_server::accept::Accept;
use axum_server::tls_rustls::RustlsAcceptor;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;
use tower::Layer;

/// The verified peer certificate's subject, inserted into request
/// extensions after the TLS handshake completes (see `bin/ingest.rs`'s
/// client-cert-extracting `Accept` wrapper). Absent on a request means the
/// mTLS layer is mis-wired — `rustls` never completes a handshake without a
/// client certificate once `WebPkiClientVerifier` is configured without
/// `allow_unauthenticated()` (never called here), so this should be
/// unreachable in practice.
#[derive(Debug, Clone)]
pub struct PeerCertSubject(pub String);

#[derive(Debug)]
pub enum MtlsError {
    Io(std::io::Error),
    Rustls(rustls::Error),
    VerifierBuild(rustls::server::VerifierBuilderError),
    NoServerCertificates,
    NoPrivateKey,
    NoClientCaCertificates,
    /// The peer certificate has no Common Name — `subject_from_der`'s
    /// contract requires one (see its own doc comment).
    NoCommonName,
    InvalidCertificate(String),
}

impl std::fmt::Display for MtlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "mTLS I/O error: {err}"),
            Self::Rustls(err) => write!(f, "rustls configuration error: {err}"),
            Self::VerifierBuild(err) => {
                write!(f, "client cert verifier build error: {err}")
            }
            Self::NoServerCertificates => {
                write!(f, "server certificate PEM file has no certificates")
            }
            Self::NoPrivateKey => write!(f, "server key PEM file has no private key"),
            Self::NoClientCaCertificates => {
                write!(f, "client CA PEM file has no certificates")
            }
            Self::NoCommonName => write!(f, "peer certificate has no Common Name"),
            Self::InvalidCertificate(err) => {
                write!(f, "invalid peer certificate: {err}")
            }
        }
    }
}

impl std::error::Error for MtlsError {}

impl From<std::io::Error> for MtlsError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<rustls::Error> for MtlsError {
    fn from(err: rustls::Error) -> Self {
        Self::Rustls(err)
    }
}

impl From<rustls::server::VerifierBuilderError> for MtlsError {
    fn from(err: rustls::server::VerifierBuilderError) -> Self {
        Self::VerifierBuild(err)
    }
}

fn read_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, MtlsError> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(MtlsError::from)
}

fn read_private_key(path: &str) -> Result<PrivateKeyDer<'static>, MtlsError> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?.ok_or(MtlsError::NoPrivateKey)
}

/// Builds the server's `rustls::ServerConfig`: the server's own identity
/// (`cert_pem_path`/`key_pem_path`) plus a client-cert verifier that
/// requires every connecting client to present a certificate chaining to
/// `client_ca_pem_path` (mandatory client auth — `WebPkiClientVerifier`'s
/// builder defaults to requiring authentication unless
/// `allow_unauthenticated()` is called, which it never is here).
pub fn load_server_config(
    cert_pem_path: &str,
    key_pem_path: &str,
    client_ca_pem_path: &str,
) -> Result<ServerConfig, MtlsError> {
    // `rustls::ServerConfig::builder()` auto-detects the process-default
    // crypto provider only when exactly one of `ring`/`aws-lc-rs` is
    // compiled in. A test binary that also links a `ring`-backed HTTP
    // client (e.g. `reqwest`'s `rustls-tls` feature) unifies both into the
    // same build, breaking that auto-detection — so pin one explicitly
    // here rather than relying on whatever the final binary happens to
    // link. A no-op if some other code already installed a provider first.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    let server_certs = read_certs(cert_pem_path)?;
    if server_certs.is_empty() {
        return Err(MtlsError::NoServerCertificates);
    }
    let server_key = read_private_key(key_pem_path)?;

    let client_ca_certs = read_certs(client_ca_pem_path)?;
    if client_ca_certs.is_empty() {
        return Err(MtlsError::NoClientCaCertificates);
    }
    let mut roots = RootCertStore::empty();
    for cert in client_ca_certs {
        roots.add(cert).map_err(MtlsError::from)?;
    }

    let verifier = WebPkiClientVerifier::builder(Arc::new(roots)).build()?;

    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(server_certs, server_key)?;

    Ok(config)
}

/// Builds a server-only `rustls::ServerConfig`: the server's own identity,
/// no client-cert verifier (T-047: `messgr-webhook` authenticates a caller
/// by its per-tenant signature, not by mTLS — see `load_server_config`'s own
/// doc comment for the mTLS case this deliberately skips).
pub fn load_plain_server_config(
    cert_pem_path: &str,
    key_pem_path: &str,
) -> Result<ServerConfig, MtlsError> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    let server_certs = read_certs(cert_pem_path)?;
    if server_certs.is_empty() {
        return Err(MtlsError::NoServerCertificates);
    }
    let server_key = read_private_key(key_pem_path)?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(server_certs, server_key)?;

    Ok(config)
}

/// Renders a peer certificate's identity as `CN=<common-name>` — the exact
/// string `producer::register::register_producer`'s `cert_subject` argument
/// must equal for a producer to resolve. Deliberately just the Common Name,
/// not the full `Display`-rendered DN (whose exact separator/escaping rules
/// aren't something a human typing `--cert-subject` at registration time
/// should have to reproduce byte-for-byte). Every certificate this system
/// mints (dev PKI's `producer-dev` role, `allow_any_name = true`) carries
/// exactly one CN and nothing else, so this is not a lossy simplification
/// for this codebase's own certificates — only for a hypothetical
/// multi-attribute subject from a third-party CA, which is out of scope
/// until this system needs one.
pub fn subject_from_der(der: &[u8]) -> Result<String, MtlsError> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|err| MtlsError::InvalidCertificate(err.to_string()))?;
    let common_name = cert
        .subject()
        .iter_common_name()
        .next()
        .ok_or(MtlsError::NoCommonName)?
        .as_str()
        .map_err(|err| MtlsError::InvalidCertificate(err.to_string()))?;
    Ok(format!("CN={common_name}"))
}

/// Wraps `axum_server`'s `RustlsAcceptor` and, after every successful
/// handshake, extracts the peer certificate's subject and attaches it to
/// the request as a `PeerCertSubject` extension — `axum-server`'s own
/// documented mechanism for exactly this (its `examples/rustls_session.rs`
/// does the same thing for SNI hostnames via `Extension(..).layer(service)`).
/// Shared by `messgr-ingest` itself and `tests/ingest.rs`, so the test
/// exercises the real accept path rather than a reimplementation of it.
#[derive(Clone)]
pub struct ClientCertAcceptor {
    inner: RustlsAcceptor,
}

impl ClientCertAcceptor {
    pub fn new(inner: RustlsAcceptor) -> Self {
        Self { inner }
    }
}

impl<I, S> Accept<I, S> for ClientCertAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    type Service = AddExtension<S, PeerCertSubject>;
    type Future = Pin<
        Box<
            dyn std::future::Future<Output = io::Result<(Self::Stream, Self::Service)>>
                + Send,
        >,
    >;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let acceptor = self.inner.clone();
        Box::pin(async move {
            let (stream, service) = acceptor.accept(stream, service).await?;
            let (_, server_conn) = stream.get_ref();
            let peer_cert = server_conn
                .peer_certificates()
                .and_then(|certs| certs.first())
                .ok_or_else(|| {
                    io::Error::other(
                        "no peer certificate after a required-client-auth handshake",
                    )
                })?;
            let subject = subject_from_der(peer_cert.as_ref()).map_err(|err| {
                io::Error::new(io::ErrorKind::InvalidData, err.to_string())
            })?;
            let service = Extension(PeerCertSubject(subject)).layer(service);
            Ok((stream, service))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_server_cert_file_is_a_clean_error() {
        let result = load_server_config(
            "/nonexistent/cert.pem",
            "/nonexistent/key.pem",
            "/nonexistent/ca.pem",
        );
        assert!(matches!(result, Err(MtlsError::Io(_))));
    }
}
