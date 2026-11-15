//! Trust-plane security regressions, exercised through public TLS APIs.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use interlink::common::config::Config;
use interlink::common::constants::cert;
use interlink::common::error::InterlinkError;
use interlink::common::identity::SpiffeId;
use interlink::identity::provider::StaticIdentityProvider;
use interlink::proxy::handshake::{TlsClient, TlsServer};
use interlink::proxy::{TlsCredentialPaths, TlsCredentialReloader, TlsHandshake};
use interlink_test_support::{rcgen, CertificateAuthority};
use rcgen::{CertificateParams, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

struct TestCa {
    certificate: rcgen::Certificate,
    key: KeyPair,
}

impl TestCa {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
        let key = KeyPair::generate().unwrap();
        Self {
            certificate: params.self_signed(&key).unwrap(),
            key,
        }
    }

    fn issue(
        &self,
        uris: &[&str],
        before: i64,
        after: i64,
    ) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.subject_alt_names = uris
            .iter()
            .map(|uri| SanType::URI((*uri).try_into().unwrap()))
            .collect();
        params.not_before = time::OffsetDateTime::from_unix_timestamp(before).unwrap();
        params.not_after = time::OffsetDateTime::from_unix_timestamp(after).unwrap();
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let key = KeyPair::generate().unwrap();
        let cert = params
            .signed_by(&key, &self.certificate, &self.key)
            .unwrap();
        (
            cert.der().clone(),
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
    }

    fn provider(&self) -> Arc<StaticIdentityProvider> {
        Arc::new(StaticIdentityProvider::with_ca_bundle(
            SpiffeId::try_new("mesh.local", "default", "proxy").unwrap(),
            vec![self.certificate.der().to_vec()],
        ))
    }
}

async fn client_handshake(
    uris: &[&str],
    before: i64,
    after: i64,
) -> Result<interlink::proxy::handshake::TlsStream, InterlinkError> {
    let ca = TestCa::new();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let (server_cert, server_key) = ca.issue(
        &["spiffe://mesh.local/ns/default/sa/server"],
        now - 60,
        now + 86400,
    );
    let (client_cert, client_key) = ca.issue(uris, before, after);
    let server = TlsServer::new(ca.provider(), server_cert, server_key).unwrap();
    let client = TlsClient::with_client_auth(ca.provider(), client_cert, client_key).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted, _connected) = tokio::join!(
        async {
            let (socket, _) = listener.accept().await.unwrap();
            server.accept(socket).await
        },
        client.connect(address)
    );
    accepted
}

#[tokio::test]
async fn inbound_rejects_foreign_trust_domain_but_accepts_local_domain() {
    timeout(Duration::from_secs(10), async {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let error = client_handshake(
            &["spiffe://foreign.local/ns/default/sa/client"],
            now - 60,
            now + 3600,
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("trust domain"), "{error}");
        let accepted = client_handshake(
            &["spiffe://mesh.local/ns/default/sa/client"],
            now - 60,
            now + 3600,
        )
        .await
        .unwrap();
        assert_eq!(accepted.peer_identity.trust_domain, "mesh.local");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inbound_requires_exactly_one_uri_san() {
    timeout(Duration::from_secs(10), async {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let uri = "spiffe://mesh.local/ns/default/sa/client";
        for uris in [vec![], vec![uri, uri], vec!["https://example.invalid", uri]] {
            let error = client_handshake(&uris, now - 60, now + 3600)
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("exactly one URI SAN"), "{error}");
        }
        assert!(client_handshake(&[uri], now - 60, now + 3600).await.is_ok());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn expired_and_not_yet_valid_peer_certificates_are_rejected() {
    timeout(Duration::from_secs(10), async {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let skew = cert::CLOCK_SKEW_TOLERANCE.as_secs() as i64;
        let uri = ["spiffe://mesh.local/ns/default/sa/client"];
        assert!(client_handshake(&uri, now - 2 * skew, now - skew - 60)
            .await
            .is_err());
        assert!(client_handshake(&uri, now + skew + 60, now + 2 * skew)
            .await
            .is_err());
        assert!(client_handshake(&uri, now - 60, now + 60).await.is_ok());
    })
    .await
    .unwrap();
}

struct CredentialDirectory(PathBuf);
impl CredentialDirectory {
    fn new(name: &str) -> Self {
        let directory = Self(
            std::env::temp_dir().join(format!("interlink-security-{name}-{}", std::process::id())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        directory
    }
    fn paths(&self) -> TlsCredentialPaths {
        TlsCredentialPaths::new(
            self.0.join("ca.der"),
            self.0.join("cert.der"),
            self.0.join("key.der"),
        )
    }
}
impl Drop for CredentialDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[tokio::test]
async fn mismatched_key_reload_keeps_previous_generation_serving() {
    timeout(Duration::from_secs(10), async {
        let ca = CertificateAuthority::new("reload.local").unwrap();
        let id = SpiffeId::try_new("reload.local", "default", "proxy").unwrap();
        let (certificate, key) = ca.issue_leaf_with_key(&id, &[]).unwrap();
        let (_, wrong_key) = ca.issue_leaf_with_key(&id, &[]).unwrap();
        let directory = CredentialDirectory::new("key-match");
        let paths = directory.paths();
        std::fs::write(paths.ca_bundle(), ca.root_cert_der()).unwrap();
        std::fs::write(paths.certificate(), certificate.as_ref()).unwrap();
        std::fs::write(paths.private_key(), key).unwrap();
        let reloader = Arc::new(
            TlsCredentialReloader::load(paths.clone(), id.clone(), false)
                .await
                .unwrap(),
        );
        std::fs::write(paths.private_key(), wrong_key).unwrap();
        assert!(matches!(
            reloader.reload().await,
            Err(InterlinkError::Tls(rustls::Error::InconsistentKeys(
                rustls::InconsistentKeys::KeyMismatch
            )))
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = reloader.clone();
        let expected = id.clone();
        let accepted = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = server.accept(stream).await.unwrap();
            assert_eq!(*tls.peer_identity, expected);
            tls.inner.write_all(b"old generation").await.unwrap();
        });
        let mut tls = reloader.connect(address).await.unwrap();
        assert_eq!(*tls.peer_identity, id);
        assert_eq!(
            tls.inner.get_ref().1.peer_certificates().unwrap()[0],
            certificate
        );
        let mut bytes = [0; 14];
        tls.inner.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"old generation");
        accepted.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn near_expiry_peer_supports_full_and_resumed_mesh_handshakes() {
    timeout(Duration::from_secs(10), async {
        let ca = TestCa::new();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let uri = ["spiffe://mesh.local/ns/default/sa/proxy"];
        let (certificate, key) = ca.issue(&uri, now - 60, now + 60);
        let server = TlsServer::new(ca.provider(), certificate.clone(), key.clone_key()).unwrap();
        let client = TlsClient::with_client_auth(ca.provider(), certificate, key).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for expected in [rustls::HandshakeKind::Full, rustls::HandshakeKind::Resumed] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut tls = server.accept(socket).await.unwrap();
                assert_eq!(tls.inner.get_ref().1.handshake_kind(), Some(expected));
                tls.inner.write_all(b"x").await.unwrap();
                let mut byte = [0];
                tls.inner.read_exact(&mut byte).await.unwrap();
            }
        });
        for expected in [rustls::HandshakeKind::Full, rustls::HandshakeKind::Resumed] {
            let mut tls = client.connect(address).await.unwrap();
            assert_eq!(tls.inner.get_ref().1.handshake_kind(), Some(expected));
            assert_eq!(tls.peer_identity.to_uri(), uri[0]);
            let mut byte = [0];
            // RFC 8446 §4.6.1: tickets arrive after the handshake; drive reads.
            tls.inner.read_exact(&mut byte).await.unwrap();
            tls.inner.write_all(b"x").await.unwrap();
        }
        task.await.unwrap();
    })
    .await
    .unwrap();
}

#[test]
fn credential_paths_fail_closed_or_resolve_explicit_environment_configuration() {
    const CHILD: &str = "INTERLINK_TEST_TLS_PATH_SCENARIO";
    if let Ok(scenario) = std::env::var(CHILD) {
        let config = Config::default().with_env().unwrap();
        match scenario.as_str() {
            "missing" => assert!(TlsCredentialPaths::from_config(&config)
                .unwrap_err()
                .to_string()
                .contains("ca_bundle_path, cert_path, key_path")),
            "directory" => assert_eq!(
                TlsCredentialPaths::from_config(&config).unwrap(),
                TlsCredentialPaths::new(
                    "/operator/ca.der",
                    "/operator/server.der",
                    "/operator/server.key"
                )
            ),
            "explicit" => assert_eq!(
                TlsCredentialPaths::from_config(&config).unwrap(),
                TlsCredentialPaths::new("explicit-ca", "explicit-cert", "explicit-key")
            ),
            _ => panic!("unknown child scenario"),
        }
        return;
    }
    for scenario in ["missing", "directory", "explicit"] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "credential_paths_fail_closed_or_resolve_explicit_environment_configuration",
            ])
            .env(CHILD, scenario)
            .env_remove("INTERLINK_CERT_DIR")
            .env_remove("INTERLINK_CA_BUNDLE_PATH")
            .env_remove("INTERLINK_CERT_PATH")
            .env_remove("INTERLINK_KEY_PATH");
        if scenario != "missing" {
            command.env("INTERLINK_CERT_DIR", "/operator");
        }
        if scenario == "explicit" {
            command
                .env("INTERLINK_CA_BUNDLE_PATH", "explicit-ca")
                .env("INTERLINK_CERT_PATH", "explicit-cert")
                .env("INTERLINK_KEY_PATH", "explicit-key");
        }
        assert!(command.status().unwrap().success());
    }
}
