// B8: No panic paths in connection-handling code.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

//! SPIFFE-aware server certificate verification for outbound mTLS.
//!
//! ## Why not stock WebPKI verification?
//!
//! rustls's default `WebPkiServerVerifier` does two things:
//!   1. RFC 5280 path validation (chain to a trust anchor, signatures,
//!      validity window).
//!   2. RFC 6125-style server *name* matching: the dialed hostname/IP must
//!      appear in the leaf's SAN.
//!
//! Step 2 is inapplicable to workload-to-workload mTLS in a SPIFFE mesh and
//! actively breaks it: peers are dialed by ephemeral addresses recovered from
//! `SO_ORIGINAL_DST` — pod IPs and Service ClusterIPs that cannot be known at
//! certificate issuance time (the observed failure: meshed Kubernetes Service
//! traffic dials `10.96.x.x`, which no workload certificate can carry). The
//! SPIFFE X.509-SVID specification defines the replacement: a peer is
//! authenticated by the **SPIFFE ID in its URI SAN**, validated against the
//! trust domain; per-identity *authorization* is a separate layer (interlink's
//! default-deny policy engine, evaluated on every connection/stream).
//!
//! ## What this verifier enforces (nothing less than WebPKI minus the name)
//!
//! 1. **Unchanged**: full RFC 5280 path validation via rustls's own
//!    `verify_server_cert_signed_by_trust_anchor` — the same code path
//!    `WebPkiServerVerifier` uses internally (chain to our trust-domain roots,
//!    signature checks with the provider's algorithms, expiry).
//! 2. **Replaced**: instead of name matching, the leaf MUST contain a SPIFFE
//!    URI SAN that parses and whose trust domain equals ours; otherwise the
//!    handshake fails closed (B5). TLS 1.2/1.3 handshake-signature
//!    verification is delegated to the crypto provider unchanged (RFC 8446).
//!
//! This mirrors the inbound direction, where `WebPkiClientVerifier` already
//! performs chain-only validation (client certs have no "server name") and
//! identity/policy is applied after the handshake.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::verify_server_cert_signed_by_trust_anchor;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{DigitallySignedStruct, Error, RootCertStore, SignatureScheme};

/// Server certificate verifier implementing SPIFFE X.509-SVID authentication.
#[derive(Debug)]
pub(crate) struct SpiffeServerVerifier {
    roots: Arc<RootCertStore>,
    trust_domain: String,
    algs: WebPkiSupportedAlgorithms,
}

impl SpiffeServerVerifier {
    pub(crate) fn new(roots: RootCertStore, trust_domain: String) -> Self {
        let algs = rustls::crypto::CryptoProvider::get_default()
            .map(|p| p.signature_verification_algorithms)
            .unwrap_or_else(|| {
                rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms
            });
        Self {
            roots: Arc::new(roots),
            trust_domain,
            algs,
        }
    }
}

impl ServerCertVerifier for SpiffeServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // 1. RFC 5280 path validation — identical to WebPkiServerVerifier.
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.algs.all,
        )?;

        // 2. SPIFFE peer authentication: the leaf must carry a SPIFFE URI SAN
        //    in our trust domain. Fail closed on absence or mismatch (B5).
        let id = crate::proxy::handshake::spiffe_id_from_cert_der(end_entity.as_ref())
            .map_err(|e| Error::General(format!("SPIFFE identity: {e}")))?;
        if id.trust_domain != self.trust_domain {
            return Err(Error::General(format!(
                "peer trust domain '{}' does not match ours '{}'",
                id.trust_domain, self.trust_domain
            )));
        }

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}
