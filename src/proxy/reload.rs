use std::io::{Error as IoError, ErrorKind};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::RootCertStore;
use rustls_pki_types::pem::PemObject;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::admin::ReloadHandler;
use crate::common::config::Config;
use crate::common::error::InterlinkError;
use crate::common::identity::SpiffeId;
use crate::identity::provider::StaticIdentityProvider;
use crate::proxy::handshake::{
    legacy_alpn_protocols, spiffe_id_from_cert_der, TlsClient, TlsHandshake, TlsServer, TlsStream,
};
use crate::proxy::verify::SpiffeServerVerifier;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlsCredentialPaths {
    ca_bundle: PathBuf,
    certificate: PathBuf,
    private_key: PathBuf,
}

impl TlsCredentialPaths {
    pub fn new(
        ca_bundle: impl Into<PathBuf>,
        certificate: impl Into<PathBuf>,
        private_key: impl Into<PathBuf>,
    ) -> Self {
        Self {
            ca_bundle: ca_bundle.into(),
            certificate: certificate.into(),
            private_key: private_key.into(),
        }
    }

    pub fn from_config(config: &Config) -> Result<Self, InterlinkError> {
        // Config::with_env layers per-file environment overrides first.
        Self::resolve(
            config,
            std::env::var_os("INTERLINK_CERT_DIR").map(PathBuf::from),
        )
    }

    fn resolve(config: &Config, directory: Option<PathBuf>) -> Result<Self, InterlinkError> {
        let resolve = |value: &Option<String>, file: &str| {
            value
                .as_ref()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    directory
                        .as_ref()
                        .filter(|path| !path.as_os_str().is_empty())
                        .map(|directory| directory.join(file))
                })
        };
        let ca_bundle = resolve(&config.ca_bundle_path, "ca.der");
        let certificate = resolve(&config.cert_path, "server.der");
        let private_key = resolve(&config.key_path, "server.key");
        match (ca_bundle, certificate, private_key) {
            (Some(ca), Some(cert), Some(key)) => Ok(Self::new(ca, cert, key)),
            (ca, cert, key) => {
                let missing = [
                    ca.is_none().then_some("ca_bundle_path"),
                    cert.is_none().then_some("cert_path"),
                    key.is_none().then_some("key_path"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
                Err(InterlinkError::Config(format!(
                    "missing TLS credential paths: {missing}; set each path or INTERLINK_CERT_DIR"
                )))
            }
        }
    }

    pub fn ca_bundle(&self) -> &Path {
        &self.ca_bundle
    }

    pub fn certificate(&self) -> &Path {
        &self.certificate
    }

    pub fn private_key(&self) -> &Path {
        &self.private_key
    }
}

struct TlsGeneration {
    client: Arc<TlsClient>,
    server: Arc<TlsServer>,
}

pub struct TlsCredentialReloader {
    paths: TlsCredentialPaths,
    expected_identity: SpiffeId,
    mux_enabled: bool,
    generation: ArcSwap<TlsGeneration>,
    reload_lock: Mutex<()>,
}

impl TlsCredentialReloader {
    pub async fn load(
        paths: TlsCredentialPaths,
        expected_identity: SpiffeId,
        mux_enabled: bool,
    ) -> Result<Self, InterlinkError> {
        let generation =
            Self::load_generation(paths.clone(), expected_identity.clone(), mux_enabled).await?;
        Ok(Self {
            paths,
            expected_identity,
            mux_enabled,
            generation: ArcSwap::from_pointee(generation),
            reload_lock: Mutex::new(()),
        })
    }

    pub async fn reload(&self) -> Result<(), InterlinkError> {
        let _guard = self.reload_lock.lock().await;
        let generation = Self::load_generation(
            self.paths.clone(),
            self.expected_identity.clone(),
            self.mux_enabled,
        )
        .await?;
        self.generation.store(Arc::new(generation));
        Ok(())
    }

    async fn load_generation(
        paths: TlsCredentialPaths,
        expected_identity: SpiffeId,
        mux_enabled: bool,
    ) -> Result<TlsGeneration, InterlinkError> {
        tokio::task::spawn_blocking(move || {
            build_generation(&paths, &expected_identity, mux_enabled)
        })
        .await
        .map_err(|error| {
            InterlinkError::Config(format!("TLS credential loader task failed: {error}"))
        })?
    }
}

#[async_trait]
impl ReloadHandler for TlsCredentialReloader {
    async fn reload(&self) -> Result<(), InterlinkError> {
        TlsCredentialReloader::reload(self).await
    }
}

#[async_trait]
impl TlsHandshake for TlsCredentialReloader {
    async fn connect(&self, address: SocketAddr) -> Result<TlsStream, InterlinkError> {
        let generation = self.generation.load_full();
        generation.client.connect(address).await
    }

    async fn accept(&self, stream: TcpStream) -> Result<TlsStream, InterlinkError> {
        let generation = self.generation.load_full();
        generation.server.accept(stream).await
    }
}

fn build_generation(
    paths: &TlsCredentialPaths,
    expected_identity: &SpiffeId,
    mux_enabled: bool,
) -> Result<TlsGeneration, InterlinkError> {
    let roots = parse_certificates(read_file(paths.ca_bundle(), "CA bundle")?)?;
    let certificates = parse_certificates(read_file(paths.certificate(), "certificate")?)?;
    let mut certificates = certificates.into_iter();
    let certificate = certificates.next().ok_or_else(|| {
        InterlinkError::Config("certificate file contains no certificates".into())
    })?;
    let intermediates: Vec<_> = certificates.collect();
    let private_key = parse_private_key(read_file(paths.private_key(), "private key")?)?;

    // Use rustls's SPKI comparison, but unlike from_der do not accept Unknown.
    // A generation must prove possession of the leaf's key before publication.
    let crypto = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let certified_key = rustls::sign::CertifiedKey::from_der(
        vec![certificate.clone()],
        private_key.clone_key(),
        &crypto,
    )?;
    certified_key.keys_match()?;

    let actual_identity = spiffe_id_from_cert_der(certificate.as_ref())?;
    if &actual_identity != expected_identity {
        return Err(InterlinkError::Identity(format!(
            "certificate identity '{}' does not match configured identity '{}'",
            actual_identity.to_uri(),
            expected_identity.to_uri()
        )));
    }

    let mut root_store = RootCertStore::empty();
    for root in &roots {
        root_store.add(root.clone())?;
    }
    let verifier = SpiffeServerVerifier::new(root_store, expected_identity.trust_domain.clone());
    let server_name = ServerName::try_from("reload.invalid")
        .map_err(|error| InterlinkError::Config(format!("validation name: {error}")))?;
    verifier.verify_server_cert(
        &certificate,
        &intermediates,
        &server_name,
        &[],
        UnixTime::now(),
    )?;

    let provider = Arc::new(StaticIdentityProvider::with_ca_bundle(
        expected_identity.clone(),
        roots.iter().map(|root| root.as_ref().to_vec()).collect(),
    ));
    let (server, client) = if mux_enabled {
        (
            TlsServer::new(
                provider.clone(),
                certificate.clone(),
                private_key.clone_key(),
            )?,
            TlsClient::with_client_auth(provider, certificate, private_key)?,
        )
    } else {
        (
            TlsServer::new_with_alpn(
                provider.clone(),
                certificate.clone(),
                private_key.clone_key(),
                legacy_alpn_protocols(),
            )?,
            TlsClient::with_client_auth_alpn(
                provider,
                certificate,
                private_key,
                legacy_alpn_protocols(),
            )?,
        )
    };

    Ok(TlsGeneration {
        client: Arc::new(client),
        server: Arc::new(server),
    })
}

fn read_file(path: &Path, description: &str) -> Result<Vec<u8>, InterlinkError> {
    std::fs::read(path).map_err(|error| {
        InterlinkError::Io(IoError::new(
            error.kind(),
            format!("read {description} '{}': {error}", path.display()),
        ))
    })
}

fn parse_certificates(bytes: Vec<u8>) -> Result<Vec<CertificateDer<'static>>, InterlinkError> {
    if !contains_pem_header(&bytes) {
        if bytes.is_empty() {
            return Err(InterlinkError::Io(IoError::new(
                ErrorKind::InvalidData,
                "DER certificate file is empty",
            )));
        }
        return Ok(vec![CertificateDer::from(bytes)]);
    }

    // PemObject preserves typed-section filtering and bundle order.
    let certificates = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| InterlinkError::Config(format!("certificate PEM: {error}")))?;
    if certificates.is_empty() {
        return Err(InterlinkError::Io(IoError::new(
            ErrorKind::InvalidData,
            "PEM file contains no certificates",
        )));
    }
    Ok(certificates)
}

fn parse_private_key(bytes: Vec<u8>) -> Result<PrivateKeyDer<'static>, InterlinkError> {
    if !contains_pem_header(&bytes) {
        if bytes.is_empty() {
            return Err(InterlinkError::Io(IoError::new(
                ErrorKind::InvalidData,
                "DER private-key file is empty",
            )));
        }
        return Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bytes)));
    }

    // Like rustls-pemfile::private_key, select the first supported key section
    // (PKCS#1, PKCS#8, or SEC1), skipping other section kinds.
    PrivateKeyDer::from_pem_slice(&bytes)
        .map_err(|error| InterlinkError::Config(format!("private-key PEM: {error}")))
}

fn contains_pem_header(bytes: &[u8]) -> bool {
    bytes
        .windows(b"-----BEGIN ".len())
        .any(|window| window == b"-----BEGIN ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use interlink_test_support::rcgen;

    #[test]
    fn missing_credential_paths_fail_closed_and_name_only_unresolved_paths() {
        let mut config = Config::default();
        let error = TlsCredentialPaths::resolve(&config, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("ca_bundle_path, cert_path, key_path"));
        config.cert_path = Some("explicit-cert".into());
        let error = TlsCredentialPaths::resolve(&config, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("ca_bundle_path, key_path;"));
        assert!(!error.contains("cert_path,"));
    }

    #[test]
    fn credential_directory_resolves_all_paths_and_explicit_paths_win() {
        let mut config = Config::default();
        let directory = PathBuf::from("/operator/credentials");
        assert_eq!(
            TlsCredentialPaths::resolve(&config, Some(directory.clone())).unwrap(),
            TlsCredentialPaths::new(
                directory.join("ca.der"),
                directory.join("server.der"),
                directory.join("server.key")
            ),
        );
        config.ca_bundle_path = Some("explicit-ca".into());
        config.cert_path = Some("explicit-cert".into());
        config.key_path = Some("explicit-key".into());
        assert_eq!(
            TlsCredentialPaths::resolve(&config, Some(directory)).unwrap(),
            TlsCredentialPaths::new("explicit-ca", "explicit-cert", "explicit-key"),
        );
        assert!(TlsCredentialPaths::resolve(&config, None).is_ok());
    }

    #[test]
    fn cert_directory_environment_is_used_without_mutating_parent_environment() {
        const CHILD: &str = "INTERLINK_TEST_CREDENTIAL_PATH_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let paths = TlsCredentialPaths::from_config(&Config::default()).unwrap();
            assert_eq!(
                paths,
                TlsCredentialPaths::new(
                    "/operator/certs/ca.der",
                    "/operator/certs/server.der",
                    "/operator/certs/server.key",
                )
            );
            return;
        }
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "proxy::reload::tests::cert_directory_environment_is_used_without_mutating_parent_environment"])
            .env(CHILD, "1").env("INTERLINK_CERT_DIR", "/operator/certs")
            .status().unwrap();
        assert!(result.success());
    }

    #[test]
    fn pem_filtering_first_key_der_and_malformed_inputs_preserve_semantics() {
        let first = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let second = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mixed = format!(
            "{}\n{}\n{}",
            first.cert.pem(),
            first.key_pair.serialize_pem(),
            second.key_pair.serialize_pem()
        );
        let key = parse_private_key(mixed.as_bytes().to_vec()).unwrap();
        assert_eq!(key.secret_der(), first.key_pair.serialize_der());
        let certs = parse_certificates(mixed.into_bytes()).unwrap();
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0], *first.cert.der());
        assert!(parse_private_key(first.cert.pem().into_bytes()).is_err());
        assert!(parse_certificates(first.key_pair.serialize_pem().into_bytes()).is_err());
        for malformed in [
            b"-----BEGIN CERTIFICATE-----\n%%%\n-----END CERTIFICATE-----".as_slice(),
            b"-----BEGIN CERTIFICATE-----\nYQ==",
        ] {
            assert!(parse_certificates(malformed.to_vec()).is_err());
        }
        assert!(parse_private_key(
            b"-----BEGIN PRIVATE KEY-----\n%%%\n-----END PRIVATE KEY-----".to_vec()
        )
        .is_err());
        assert_eq!(
            parse_certificates(first.cert.der().to_vec()).unwrap(),
            vec![first.cert.der().clone()]
        );
        assert_eq!(
            parse_private_key(first.key_pair.serialize_der())
                .unwrap()
                .secret_der(),
            first.key_pair.serialize_der()
        );
        assert!(parse_certificates(Vec::new()).is_err());
        assert!(parse_private_key(Vec::new()).is_err());
        assert!(matches!(
            parse_private_key(
                b"-----BEGIN RSA PRIVATE KEY-----\nYWJj\n-----END RSA PRIVATE KEY-----".to_vec()
            )
            .unwrap(),
            PrivateKeyDer::Pkcs1(_),
        ));
        assert!(matches!(
            parse_private_key(
                b"-----BEGIN EC PRIVATE KEY-----\nYWJj\n-----END EC PRIVATE KEY-----".to_vec()
            )
            .unwrap(),
            PrivateKeyDer::Sec1(_),
        ));
    }
    use std::path::Path;

    use super::{parse_certificates, parse_private_key, TlsCredentialPaths};

    #[test]
    fn credential_paths_expose_the_configured_files() {
        let paths = TlsCredentialPaths::new("ca", "cert", "key");
        assert_eq!(paths.ca_bundle(), Path::new("ca"));
        assert_eq!(paths.certificate(), Path::new("cert"));
        assert_eq!(paths.private_key(), Path::new("key"));
    }

    #[test]
    fn parses_pem_certificate_bundles_and_private_keys() {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let pem = generated.cert.pem();
        let bundle = format!("{pem}\n{pem}");

        let certificates = parse_certificates(bundle.into_bytes()).unwrap();
        let private_key =
            parse_private_key(generated.key_pair.serialize_pem().into_bytes()).unwrap();

        assert_eq!(certificates.len(), 2);
        assert_eq!(certificates[0].as_ref(), generated.cert.der().as_ref());
        assert_eq!(certificates[1].as_ref(), generated.cert.der().as_ref());
        assert_eq!(
            private_key.secret_der(),
            generated.key_pair.serialize_der().as_slice()
        );
    }
}
