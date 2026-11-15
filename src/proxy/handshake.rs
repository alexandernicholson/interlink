#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ServerConfig};
use std::sync::LazyLock;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use x509_parser::extensions::{GeneralName, ParsedExtension};

use crate::common::constants::{cert, timeouts};
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
    async fn connect(&self, addr: SocketAddr) -> Result<TlsStream, InterlinkError>;
    async fn accept(&self, stream: TcpStream) -> Result<TlsStream, InterlinkError>;
}

/// A TLS 1.3 stream with the peer's SPIFFE identity extracted from the SAN.
pub struct TlsStream {
    pub inner: tokio_rustls::TlsStream<TcpStream>,
    pub peer_identity: Arc<SpiffeId>,
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
    async fn connect(&self, addr: SocketAddr) -> Result<TlsStream, InterlinkError> {
        let stream = TcpStream::connect(addr).await.map_err(InterlinkError::Io)?;
        if let Err(e) = configure_socket(&stream) {
            tracing::warn!("failed to configure outbound TLS socket {}: {}", addr, e);
        }

        let server_name = ServerName::IpAddress(addr.ip().into());

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
        let verifier = crate::proxy::verify::SpiffeClientVerifier::new(
            root_store,
            provider.get_trust_domain().name.clone(),
        )?;
        let mut config = ServerConfig::builder()
            .with_client_cert_verifier(Arc::new(verifier))
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
    async fn connect(&self, _addr: SocketAddr) -> Result<TlsStream, InterlinkError> {
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
// The constant ASN.1 OID arcs are valid by construction; no runtime input.
#[cfg_attr(not(test), allow(clippy::unwrap_used))]
static SAN_OID: LazyLock<x509_parser::asn1_rs::Oid<'static>> =
    LazyLock::new(|| x509_parser::asn1_rs::Oid::from(&[2u64, 5, 29, 17]).unwrap());

/// Cache identity and immutable validity bounds together: cache hits must still
/// enforce expiry, including when rustls restores the chain from a session ticket.
struct CachedIdentity {
    identity: Arc<SpiffeId>,
    validity: CertificateValidity,
}

struct CertificateValidity {
    not_before: i64,
    not_after: i64,
}

impl CertificateValidity {
    fn validate_at(&self, now: UnixTime) -> Result<(), InterlinkError> {
        let now = i128::from(now.as_secs());
        let skew = i128::from(cert::CLOCK_SKEW_TOLERANCE.as_secs());
        if i128::from(self.not_before) > now + skew || i128::from(self.not_after) < now - skew {
            return Err(InterlinkError::Identity(
                "peer certificate outside validity window".into(),
            ));
        }
        Ok(())
    }
}

static IDENTITY_CACHE: LazyLock<moka::sync::Cache<Vec<u8>, Arc<CachedIdentity>>> =
    LazyLock::new(|| {
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
) -> Result<Arc<SpiffeId>, InterlinkError> {
    extract_identity_at(stream, UnixTime::now())
}

fn extract_identity_at(
    stream: &tokio_rustls::TlsStream<TcpStream>,
    now: UnixTime,
) -> Result<Arc<SpiffeId>, InterlinkError> {
    let (_io, state) = stream.get_ref();
    let certs = state
        .peer_certificates()
        .ok_or_else(|| InterlinkError::Identity("no peer certificate".into()))?;

    let leaf_der = certs
        .first()
        .ok_or_else(|| InterlinkError::Identity("empty certificate chain".into()))?
        .as_ref();
    if let Some(cached) = IDENTITY_CACHE.get(leaf_der) {
        cached.validity.validate_at(now)?;
        return Ok(cached.identity.clone());
    }

    let (identity, validity) = parse_peer_identity(leaf_der)?;
    validity.validate_at(now)?;
    let cached = Arc::new(CachedIdentity {
        identity: Arc::new(identity),
        validity,
    });
    IDENTITY_CACHE.insert(leaf_der.to_vec(), cached.clone());
    Ok(cached.identity.clone())
}

/// Extract the SPIFFE identity from a leaf certificate's SAN URI
/// (RFC 5280 §4.2.1.6; SPIFFE X.509-SVID: the identity is the URI SAN).
/// Shared by post-handshake identity extraction and the handshake-time
/// SPIFFE server verifier. Uncached — callers cache as appropriate.
pub(crate) fn spiffe_id_from_cert_der(leaf_der: &[u8]) -> Result<SpiffeId, InterlinkError> {
    let (identity, validity) = parse_peer_identity(leaf_der)?;
    validity.validate_at(UnixTime::now())?;
    Ok(identity)
}

fn parse_peer_identity(leaf_der: &[u8]) -> Result<(SpiffeId, CertificateValidity), InterlinkError> {
    let leaf = x509_parser::parse_x509_certificate(leaf_der)
        .map_err(|e| InterlinkError::Identity(format!("cert parse: {}", e)))?
        .1;
    let mut uri = None;
    for ext in leaf.extensions() {
        if ext.oid == *SAN_OID {
            if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
                for name in &san.general_names {
                    if let GeneralName::URI(value) = name {
                        if uri.replace(*value).is_some() {
                            return Err(InterlinkError::Identity(
                                "certificate must contain exactly one URI SAN".into(),
                            ));
                        }
                    }
                }
            }
        }
    }
    let uri = uri.ok_or_else(|| {
        InterlinkError::Identity("certificate must contain exactly one URI SAN".into())
    })?;
    Ok((
        SpiffeId::from_uri(uri)?,
        CertificateValidity {
            not_before: leaf.validity().not_before.timestamp(),
            not_after: leaf.validity().not_after.timestamp(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_certificate_is_rejected() {
        assert!(spiffe_id_from_cert_der(&[]).is_err());
    }

    // Expired-by-design fixture CA/leaf/private key, valid Jan 1–2, 2020.
    // Public test data only: NEVER use these bytes as production trust material.
    const ROOT: &[u8] = include_bytes!("testdata/expired-identity/ca.der");
    const LEAF: &[u8] = include_bytes!("testdata/expired-identity/leaf.der");
    const KEY: &[u8] = include_bytes!("testdata/expired-identity/key.der");

    #[derive(Debug)]
    struct FixtureTime;
    impl rustls::time_provider::TimeProvider for FixtureTime {
        fn current_time(&self) -> Option<UnixTime> {
            Some(at(1_577_880_000)) // 2020-01-01 12:00 UTC
        }
    }

    fn at(seconds: u64) -> UnixTime {
        UnixTime::since_unix_epoch(std::time::Duration::from_secs(seconds))
    }

    #[tokio::test]
    async fn resumed_handshake_rechecks_cached_certificate_validity() {
        use rustls::pki_types::PrivatePkcs8KeyDer;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        timeout(std::time::Duration::from_secs(10), async {
            assert!(
                spiffe_id_from_cert_der(LEAF).is_err(),
                "expired fixture must fail the production wall-clock check"
            );
            let mut roots = rustls::RootCertStore::empty();
            roots.add(CertificateDer::from(ROOT)).unwrap();
            let crypto = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let server_config =
                ServerConfig::builder_with_details(crypto.clone(), Arc::new(FixtureTime))
                    .with_protocol_versions(&[&rustls::version::TLS13])
                    .unwrap()
                    .with_client_cert_verifier(
                        rustls::server::WebPkiClientVerifier::builder(Arc::new(roots.clone()))
                            .build()
                            .unwrap(),
                    )
                    .with_single_cert(
                        vec![CertificateDer::from(LEAF)],
                        PrivatePkcs8KeyDer::from(KEY).into(),
                    )
                    .unwrap();
            let client_config = ClientConfig::builder_with_details(crypto, Arc::new(FixtureTime))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_root_certificates(roots)
                .with_client_auth_cert(
                    vec![CertificateDer::from(LEAF)],
                    PrivatePkcs8KeyDer::from(KEY).into(),
                )
                .unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
            let connector = TlsConnector::from(Arc::new(client_config));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                for expected in [rustls::HandshakeKind::Full, rustls::HandshakeKind::Resumed] {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut tls = acceptor.accept(socket).await.unwrap();
                    assert_eq!(tls.get_ref().1.handshake_kind(), Some(expected));
                    tls.write_all(b"x").await.unwrap();
                    let mut byte = [0];
                    tls.read_exact(&mut byte).await.unwrap();
                }
            });
            IDENTITY_CACHE.invalidate(LEAF);
            for expected in [rustls::HandshakeKind::Full, rustls::HandshakeKind::Resumed] {
                let socket = TcpStream::connect(address).await.unwrap();
                let mut tls: tokio_rustls::TlsStream<_> = connector
                    .connect(ServerName::try_from("localhost").unwrap(), socket)
                    .await
                    .unwrap()
                    .into();
                assert_eq!(tls.get_ref().1.handshake_kind(), Some(expected));
                let skew = cert::CLOCK_SKEW_TOLERANCE.as_secs();
                let inside = extract_identity_at(&tls, at(1_577_880_000)).unwrap();
                assert_eq!(
                    inside.to_uri(),
                    "spiffe://fixture.local/ns/default/sa/proxy"
                );
                assert!(IDENTITY_CACHE.get(LEAF).is_some());
                assert!(extract_identity_at(&tls, at(1_577_836_800 - skew - 1)).is_err());
                assert!(extract_identity_at(&tls, at(1_577_923_200 + skew + 1)).is_err());
                assert!(extract_identity_at(&tls, at(1_577_836_800 - skew)).is_ok());
                assert!(extract_identity_at(&tls, at(1_577_923_200 + skew)).is_ok());
                let mut byte = [0];
                // RFC 8446 §4.6.1: process the post-handshake session tickets.
                tls.read_exact(&mut byte).await.unwrap();
                tls.write_all(b"x").await.unwrap();
            }
            task.await.unwrap();
        })
        .await
        .unwrap();
    }
}
