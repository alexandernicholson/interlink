use std::sync::Arc;

use async_trait::async_trait;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ServerConfig};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use x509_parser::extensions::{GeneralName, ParsedExtension};

use crate::common::constants::timeouts;

use crate::common::error::InterlinkError;

/// Supported ALPN protocols for negotiated HTTP/1.1 and HTTP/2 forwarding.
fn alpn_protocols() -> Vec<Vec<u8>> {
    vec![b"h2".to_vec(), b"http/1.1".to_vec()]
}
use crate::common::identity::{IdentityProvider, SpiffeId};
use crate::proxy::configure_socket;

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

        let mut config = ClientConfig::builder()
            .with_root_certificates(root_store)
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
        let mut root_store = rustls::RootCertStore::empty();
        for ca in &provider.get_trust_domain().ca_certs {
            root_store
                .add(CertificateDer::from(ca.clone()))
                .map_err(InterlinkError::Tls)?;
        }

        let mut config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(vec![cert], key)
            .map_err(InterlinkError::Tls)?;
        config.alpn_protocols = alpn_protocols();

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

        let host = addr.split(':').next().unwrap_or(addr).to_string();
        let server_name = ServerName::try_from(host.clone()).map_err(|_| {
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
        config.alpn_protocols = alpn_protocols();

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

        let peer_identity = extract_identity_from_tls_stream(&tls_stream)?;

        Ok(TlsStream {
            inner: tls_stream,
            peer_identity,
        })
    }
}

// ─── SPIFFE Identity Extraction ─────────────────────────────────────

/// Extract the SPIFFE identity from a peer's X.509 certificate SAN.
///
/// RFC 5280 §4.2.1.6: The identity is encoded as uniformResourceIdentifier
/// in the Subject Alternative Name extension (OID 2.5.29.17).
fn extract_identity_from_tls_stream(
    stream: &tokio_rustls::TlsStream<TcpStream>,
) -> Result<SpiffeId, InterlinkError> {
    let (_io, state) = stream.get_ref();
    let certs = state
        .peer_certificates()
        .ok_or_else(|| InterlinkError::Identity("no peer certificate".into()))?;

    if certs.is_empty() {
        return Err(InterlinkError::Identity("empty certificate chain".into()));
    }

    // Parse leaf certificate (RFC 5280 §4.1)
    let leaf = x509_parser::parse_x509_certificate(certs[0].as_ref())
        .map_err(|e| InterlinkError::Identity(format!("cert parse: {}", e)))?
        .1;

    // Find SAN extension and extract SPIFFE URI
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
