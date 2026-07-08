#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::Arc;

use async_trait::async_trait;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ServerConfig};
use std::sync::LazyLock;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use x509_parser::extensions::{GeneralName, ParsedExtension};

use crate::common::constants::timeouts;
use crate::common::error::InterlinkError;
use crate::common::identity::{IdentityProvider, SpiffeId};
use crate::proxy::configure_socket;

/// Supported ALPN protocols, most-preferred first: multiplexed proxy-to-proxy
/// tunnels (`il/mux/1`), then HTTP/2 and HTTP/1.1 forwarding. rustls picks the
/// first server-preferred protocol the client offers, so non-interlink clients
/// (which never offer `il/mux/1`) negotiate h2/http1.1 and take the legacy
/// relay path — mixed versions interoperate.
fn alpn_protocols() -> Vec<Vec<u8>> {
    vec![
        crate::proxy::mux::ALPN_MUX.to_vec(),
        b"h2".to_vec(),
        b"http/1.1".to_vec(),
    ]
}

/// Legacy ALPN list without mux — used to model pre-mux peers in tests and to
/// disable tunneling operationally.
pub fn legacy_alpn_protocols() -> Vec<Vec<u8>> {
    vec![b"h2".to_vec(), b"http/1.1".to_vec()]
}

/// TLS 1.3 handshake trait (RFC 8446 §2, Figure 1).
///
/// Every proxy endpoint implements this for mTLS.
/// The CertificateRequest message in the handshake makes this mutual TLS.
#[async_trait]
pub trait TlsHandshake: Send + Sync {
    async fn connect(&self, addr: &str) -> Result<TlsStream, InterlinkError>;
    async fn accept(&self, stream: TcpStream) -> Result<TlsStream, InterlinkError>;
}

/// A TLS 1.3 stream with the peer's SPIFFE identity extracted from the SAN.
pub struct TlsStream {
    pub inner: tokio_rustls::TlsStream<TcpStream>,
    pub peer_identity: SpiffeId,
}

// ─── TLS Client (outbound mTLS) ──────────────────────────────────────

/// Outbound mTLS client. Presents a client certificate and validates
/// the server certificate against the trust domain's CA bundle.
pub struct TlsClient {
    connector: TlsConnector,
    _provider: Arc<dyn IdentityProvider>,
}

impl TlsClient {
    pub fn new(provider: Arc<dyn IdentityProvider>) -> Result<Self, InterlinkError> {
        let mut root_store = rustls::RootCertStore::empty();
        for ca in &provider.get_trust_domain().ca_certs {
            root_store
                .add(CertificateDer::from(ca.clone()))
                .map_err(InterlinkError::Tls)?;
        }

        // SPIFFE peer verification: full RFC 5280 path validation retained;
        // RFC 6125 name matching replaced by SPIFFE X.509-SVID trust-domain
        // authentication (mesh peers are dialed by ephemeral pod/Service IPs
        // that cannot appear in workload certs). rustls names the injection
        // point "dangerous" because a bad verifier can skip validation — this
        // one validates the same chain properties as WebPKI (see verify.rs).
        let verifier = Arc::new(crate::proxy::verify::SpiffeServerVerifier::new(
            root_store,
            provider.get_trust_domain().name.clone(),
        ));
        let mut config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = alpn_protocols();

        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
            _provider: provider,
        })
    }

    /// Create a new TlsClient with a client certificate for mutual TLS.
    pub fn with_client_auth(
        provider: Arc<dyn IdentityProvider>,
        cert: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self, InterlinkError> {
        Self::with_client_auth_alpn(provider, cert, key, alpn_protocols())
    }

    /// As `with_client_auth`, but with an explicit ALPN list (e.g.
    /// `legacy_alpn_protocols()` to disable mux negotiation).
    pub fn with_client_auth_alpn(
        provider: Arc<dyn IdentityProvider>,
        cert: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
        alpn: Vec<Vec<u8>>,
    ) -> Result<Self, InterlinkError> {
        let mut root_store = rustls::RootCertStore::empty();
        for ca in &provider.get_trust_domain().ca_certs {
            root_store
                .add(CertificateDer::from(ca.clone()))
                .map_err(InterlinkError::Tls)?;
        }

        // See TlsClient::new — SPIFFE verifier keeps RFC 5280 path validation
        // and replaces inapplicable RFC 6125 name matching (verify.rs).
        let verifier = Arc::new(crate::proxy::verify::SpiffeServerVerifier::new(
            root_store,
            provider.get_trust_domain().name.clone(),
        ));
        let mut config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(vec![cert], key)
            .map_err(InterlinkError::Tls)?;
        config.alpn_protocols = alpn;

        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
            _provider: provider,
        })
    }
}

#[async_trait]
impl TlsHandshake for TlsClient {
    async fn connect(&self, addr: &str) -> Result<TlsStream, InterlinkError> {
        let stream = TcpStream::connect(addr).await.map_err(InterlinkError::Io)?;
        if let Err(e) = configure_socket(&stream) {
            tracing::warn!("failed to configure outbound TLS socket {}: {}", addr, e);
        }

        let host = addr.split(':').next().unwrap_or(addr);
        let server_name = ServerName::try_from(host.to_string()).map_err(|_| {
            InterlinkError::Tls(rustls::Error::General("invalid server name".to_string()))
        })?;

        let tls_stream: tokio_rustls::TlsStream<_> = timeout(
            timeouts::TLS_HANDSHAKE,
            self.connector.connect(server_name, stream),
        )
        .await
        .map_err(|_| {
            InterlinkError::Tls(rustls::Error::General(
                "TLS handshake timed out".to_string(),
            ))
        })?
        .map_err(InterlinkError::Io)?
        .into();

        // Record whether this was a full or resumed handshake.
        let is_resumed =
            tls_stream.get_ref().1.handshake_kind() == Some(rustls::HandshakeKind::Resumed);
        crate::metrics::record_handshake_kind(is_resumed);

        let peer_identity = extract_identity_from_tls_stream(&tls_stream)?;

        Ok(TlsStream {
            inner: tls_stream,
            peer_identity,
        })
    }

    async fn accept(&self, _stream: TcpStream) -> Result<TlsStream, InterlinkError> {
        Err(InterlinkError::Tls(rustls::Error::General(
            "client cannot accept connections".to_string(),
        )))
    }
}

// ─── TLS Server (inbound mTLS) ──────────────────────────────────────

/// Inbound mTLS server. Always requests a client certificate and validates
/// the peer's identity against the trust domain.
pub struct TlsServer {
    acceptor: tokio_rustls::TlsAcceptor,
    _provider: Arc<dyn IdentityProvider>,
}

impl TlsServer {
    /// Build a TLS 1.3 server that:
    ///   - Requests client certificates (mTLS)
    ///   - Serves our certificate for the proxy's SPIFFE identity
    ///   - Validates client certs against the trust anchor
    pub fn new(
        provider: Arc<dyn IdentityProvider>,
        server_cert: CertificateDer<'static>,
        server_key: PrivateKeyDer<'static>,
    ) -> Result<Self, InterlinkError> {
        Self::new_with_alpn(provider, server_cert, server_key, alpn_protocols())
    }

    /// As `new`, but with an explicit ALPN list (e.g. `legacy_alpn_protocols()`
    /// to model a pre-mux peer or disable tunneling).
    pub fn new_with_alpn(
        provider: Arc<dyn IdentityProvider>,
        server_cert: CertificateDer<'static>,
        server_key: PrivateKeyDer<'static>,
        alpn: Vec<Vec<u8>>,
    ) -> Result<Self, InterlinkError> {
        let mut root_store = rustls::RootCertStore::empty();
        for ca in &provider.get_trust_domain().ca_certs {
            root_store
                .add(CertificateDer::from(ca.clone()))
                .map_err(InterlinkError::Tls)?;
        }

        // RFC 8446 §4.4.2: Server must send Certificate message
        // The resolver provides our cert when clients connect
        let mut config = ServerConfig::builder()
            .with_client_cert_verifier(
                rustls::server::WebPkiClientVerifier::builder(root_store.into())
                    .build()
                    .map_err(|e| InterlinkError::Tls(rustls::Error::General(format!("{:?}", e))))?,
            )
            .with_single_cert(vec![server_cert], server_key)
            .map_err(InterlinkError::Tls)?;
        config.alpn_protocols = alpn;

        Ok(Self {
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
            _provider: provider,
        })
    }
}

#[async_trait]
impl TlsHandshake for TlsServer {
    async fn connect(&self, _addr: &str) -> Result<TlsStream, InterlinkError> {
        Err(InterlinkError::Tls(rustls::Error::General(
            "server cannot initiate connections".to_string(),
        )))
    }

    async fn accept(&self, stream: TcpStream) -> Result<TlsStream, InterlinkError> {
        let tls_stream: tokio_rustls::TlsStream<_> =
            timeout(timeouts::TLS_HANDSHAKE, self.acceptor.accept(stream))
                .await
                .map_err(|_| {
                    InterlinkError::Tls(rustls::Error::General(
                        "TLS handshake timed out".to_string(),
                    ))
                })?
                .map_err(InterlinkError::Io)?
                .into();

        // Record whether this was a full or resumed handshake.
        let is_resumed =
            tls_stream.get_ref().1.handshake_kind() == Some(rustls::HandshakeKind::Resumed);
        crate::metrics::record_handshake_kind(is_resumed);

        let peer_identity = extract_identity_from_tls_stream(&tls_stream)?;

        Ok(TlsStream {
            inner: tls_stream,
            peer_identity,
        })
    }
}

// ─── SPIFFE Identity Extraction (with LRU cache) ──────────────────

/// The OID for the Subject Alternative Name extension (2.5.29.17).
/// Constructed once at startup — never in the per-connection hot path (C4).
#[cfg_attr(not(test), allow(clippy::unwrap_used))]
static SAN_OID: LazyLock<x509_parser::asn1_rs::Oid<'static>> =
    LazyLock::new(|| x509_parser::asn1_rs::Oid::from(&[2u64, 5, 29, 17]).unwrap());

/// Concurrent LRU cache keyed by leaf-cert DER bytes.
/// Capacity: 1024 entries. Peak mesh deployments commonly have 50–500 peers,
/// so this avoids re-parsing X.509 certs on repeated connections from the
/// same identity.
static IDENTITY_CACHE: LazyLock<moka::sync::Cache<Vec<u8>, SpiffeId>> = LazyLock::new(|| {
    moka::sync::Cache::builder()
        .max_capacity(1024)
        .name("identity-cache")
        .build()
});

/// Extract the SPIFFE identity from a peer's X.509 certificate SAN.
///
/// Results are cached by the leaf certificate DER bytes so that repeat
/// connections from the same peer skip the X.509 parse entirely.
pub(crate) fn extract_identity_from_tls_stream(
    stream: &tokio_rustls::TlsStream<TcpStream>,
) -> Result<SpiffeId, InterlinkError> {
    let (_io, state) = stream.get_ref();
    let certs = state
        .peer_certificates()
        .ok_or_else(|| InterlinkError::Identity("no peer certificate".into()))?;

    if certs.is_empty() {
        return Err(InterlinkError::Identity("empty certificate chain".into()));
    }

    // Fast path: check LRU cache using the leaf certificate DER bytes.
    let leaf_der: &[u8] = certs[0].as_ref();
    if let Some(cached) = IDENTITY_CACHE.get(leaf_der) {
        return Ok(cached);
    }

    let id = spiffe_id_from_cert_der(leaf_der)?;
    IDENTITY_CACHE.insert(leaf_der.to_vec(), id.clone());
    Ok(id)
}

/// Extract the SPIFFE identity from a leaf certificate's SAN URI
/// (RFC 5280 §4.2.1.6; SPIFFE X.509-SVID: the identity is the URI SAN).
/// Shared by post-handshake identity extraction and the handshake-time
/// SPIFFE server verifier. Uncached — callers cache as appropriate.
pub(crate) fn spiffe_id_from_cert_der(leaf_der: &[u8]) -> Result<SpiffeId, InterlinkError> {
    let leaf = x509_parser::parse_x509_certificate(leaf_der)
        .map_err(|e| InterlinkError::Identity(format!("cert parse: {}", e)))?
        .1;

    for ext in leaf.extensions().iter() {
        if ext.oid == *SAN_OID {
            if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
                for gn in san.general_names.iter() {
                    if let GeneralName::URI(uri) = gn {
                        if let Ok(id) = SpiffeId::from_uri(uri) {
                            return Ok(id);
                        }
                    }
                }
            }
        }
    }
    Err(InterlinkError::Identity(
        "no SPIFFE URI in SAN extension".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_identity_rejects_empty_certs() {
        let err = extract_spiffe_id_from_certs(&[]);
        assert!(err.is_err());
    }

    fn extract_spiffe_id_from_certs(certs: &[CertificateDer]) -> Result<SpiffeId, InterlinkError> {
        if certs.is_empty() {
            return Err(InterlinkError::Identity("empty certificate chain".into()));
        }
        let leaf = x509_parser::parse_x509_certificate(certs[0].as_ref())
            .map_err(|e| InterlinkError::Identity(format!("cert parse: {}", e)))?
            .1;
        for ext in leaf.extensions().iter() {
            if format!("{}", ext.oid) == "2.5.29.17" {
                let parsed = ext.parsed_extension();
                if let ParsedExtension::SubjectAlternativeName(san) = parsed {
                    for gn in san.general_names.iter() {
                        if let GeneralName::URI(uri) = gn {
                            return SpiffeId::from_uri(uri);
                        }
                    }
                }
            }
        }
        Err(InterlinkError::Identity("no URI in SAN".into()))
    }

    #[test]
    fn test_extract_identity_from_real_cert() {
        let ca = crate::identity::ca::CertificateAuthority::new("test.local").unwrap();
        let id = SpiffeId::new("test.local", "ns1", "sa1");
        let cert = ca.issue_leaf(&id).unwrap();

        let result = extract_spiffe_id_from_certs(&[cert]);
        assert_eq!(result.unwrap(), id);
    }

    #[test]
    fn test_extract_identity_rejects_wrong_oid() {
        let ca = crate::identity::ca::CertificateAuthority::new("test.local").unwrap();
        let id = SpiffeId::new("test.local", "ns1", "sa1");
        let cert = ca.issue_leaf(&id).unwrap();
        let parsed = x509_parser::parse_x509_certificate(cert.as_ref())
            .unwrap()
            .1;

        // Should find exactly one SAN
        let san_count = parsed
            .extensions()
            .iter()
            .filter(|e| format!("{}", e.oid) == "2.5.29.17")
            .count();
        assert_eq!(san_count, 1);
    }
}
