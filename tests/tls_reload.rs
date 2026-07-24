use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use interlink::common::error::InterlinkError;
use interlink::common::identity::SpiffeId;
use interlink::identity::ca::CertificateAuthority;
use interlink::proxy::handshake::TlsStream;
use interlink::proxy::{TlsCredentialPaths, TlsCredentialReloader, TlsHandshake};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct CredentialFiles {
    directory: PathBuf,
    paths: TlsCredentialPaths,
}

impl CredentialFiles {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "interlink-reload-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        Self {
            paths: TlsCredentialPaths::new(
                directory.join("ca.der"),
                directory.join("tls.crt"),
                directory.join("tls.key"),
            ),
            directory,
        }
    }

    fn install(&self, authority: &CertificateAuthority, identity: &SpiffeId) -> Vec<u8> {
        let (certificate, key) = authority.issue_leaf_with_key(identity, &[]).unwrap();
        self.install_raw(authority.root_cert_der(), certificate.as_ref(), &key);
        certificate.as_ref().to_vec()
    }

    fn install_raw(&self, roots: &[u8], certificate: &[u8], key: &[u8]) {
        fs::write(self.paths.ca_bundle(), roots).unwrap();
        fs::write(self.paths.certificate(), certificate).unwrap();
        fs::write(self.paths.private_key(), key).unwrap();
    }
}

impl Drop for CredentialFiles {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

async fn connect_pair(reloader: Arc<TlsCredentialReloader>) -> (TlsStream, TlsStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_reloader = reloader.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        server_reloader.accept(stream).await.unwrap()
    });
    let client = reloader.connect(address).await.unwrap();
    (client, server.await.unwrap())
}

fn peer_certificate(stream: &TlsStream) -> Vec<u8> {
    stream
        .inner
        .get_ref()
        .1
        .peer_certificates()
        .unwrap()
        .first()
        .unwrap()
        .as_ref()
        .to_vec()
}

fn identity() -> SpiffeId {
    SpiffeId::try_new("reload.test", "workloads", "interlink").unwrap()
}

async fn load(files: &CredentialFiles, identity: &SpiffeId) -> Arc<TlsCredentialReloader> {
    Arc::new(
        TlsCredentialReloader::load(files.paths.clone(), identity.clone(), true)
            .await
            .unwrap(),
    )
}

async fn assert_serves(reloader: Arc<TlsCredentialReloader>, certificate: &[u8]) {
    let (client, _server) = tokio::time::timeout(Duration::from_secs(1), connect_pair(reloader))
        .await
        .expect("TLS handshake timed out");
    assert_eq!(peer_certificate(&client), certificate);
}

#[tokio::test]
async fn valid_rotation_is_atomic_and_existing_streams_survive() {
    let files = CredentialFiles::new("valid");
    let identity = identity();
    let authority = CertificateAuthority::new("reload.test").unwrap();
    let first = files.install(&authority, &identity);
    let reloader = load(&files, &identity).await;
    let (mut old_client, mut old_server) =
        tokio::time::timeout(Duration::from_secs(1), connect_pair(reloader.clone()))
            .await
            .expect("initial TLS handshake timed out");
    assert_eq!(peer_certificate(&old_client), first);

    let second = files.install(&authority, &identity);
    tokio::time::timeout(Duration::from_secs(1), reloader.reload())
        .await
        .expect("credential reload timed out")
        .unwrap();

    old_client.inner.write_all(b"x").await.unwrap();
    let mut byte = [0_u8; 1];
    old_server.inner.read_exact(&mut byte).await.unwrap();
    assert_eq!(byte, *b"x");
    assert_serves(reloader, &second).await;
}

#[tokio::test]
async fn invalid_identity_key_and_trust_reloads_preserve_last_known_good() {
    let files = CredentialFiles::new("rollback");
    let identity = identity();
    let authority = CertificateAuthority::new("reload.test").unwrap();
    let first = files.install(&authority, &identity);
    let reloader = load(&files, &identity).await;

    let wrong_identity = SpiffeId::try_new("reload.test", "workloads", "other").unwrap();
    files.install(&authority, &wrong_identity);
    assert!(matches!(
        reloader.reload().await,
        Err(InterlinkError::Identity(_))
    ));
    assert_serves(reloader.clone(), &first).await;

    let (replacement, _) = authority.issue_leaf_with_key(&identity, &[]).unwrap();
    let (_, wrong_key) = authority.issue_leaf_with_key(&identity, &[]).unwrap();
    files.install_raw(authority.root_cert_der(), replacement.as_ref(), &wrong_key);
    assert!(matches!(
        reloader.reload().await,
        Err(InterlinkError::Tls(_))
    ));
    assert_serves(reloader.clone(), &first).await;

    let untrusted = CertificateAuthority::new("reload.test").unwrap();
    let (untrusted_certificate, untrusted_key) =
        untrusted.issue_leaf_with_key(&identity, &[]).unwrap();
    files.install_raw(
        authority.root_cert_der(),
        untrusted_certificate.as_ref(),
        &untrusted_key,
    );
    assert!(matches!(
        reloader.reload().await,
        Err(InterlinkError::Tls(_))
    ));
    assert_serves(reloader, &first).await;
}

#[tokio::test]
async fn repeated_reload_and_concurrent_handshakes_are_bounded() {
    let files = CredentialFiles::new("concurrent");
    let identity = identity();
    let authority = CertificateAuthority::new("reload.test").unwrap();
    let previous = files.install(&authority, &identity);
    let reloader = load(&files, &identity).await;
    let current = files.install(&authority, &identity);

    let mut reloads = JoinSet::new();
    for _ in 0..8 {
        let reloader = reloader.clone();
        reloads.spawn(async move { reloader.reload().await });
    }

    let mut handshakes = JoinSet::new();
    for _ in 0..16 {
        let reloader = reloader.clone();
        handshakes.spawn(async move {
            let (client, _server) = connect_pair(reloader).await;
            peer_certificate(&client)
        });
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            async {
                while let Some(result) = reloads.join_next().await {
                    result.unwrap().unwrap();
                }
            },
            async {
                while let Some(result) = handshakes.join_next().await {
                    let certificate = result.unwrap();
                    assert!(certificate == previous || certificate == current);
                }
            }
        );
    })
    .await
    .expect("concurrent reloads and TLS handshakes timed out");
    assert_serves(reloader, &current).await;
}
