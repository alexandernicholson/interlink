use std::io::{BufReader, Error as IoError, ErrorKind};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::RootCertStore;
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

    pub fn from_config(config: &Config) -> Self {
        let default_directory = std::env::var_os("INTERLINK_CERT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("interlink-demo"));
        Self::new(
            config
                .ca_bundle_path
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| default_directory.join("ca.der")),
            config
                .cert_path
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| default_directory.join("server.der")),
            config
                .key_path
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| default_directory.join("server.key")),
        )
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

    let certificates = rustls_pemfile::certs(&mut BufReader::new(bytes.as_slice()))
        .collect::<Result<Vec<_>, _>>()?;
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

    rustls_pemfile::private_key(&mut BufReader::new(bytes.as_slice()))?.ok_or_else(|| {
        InterlinkError::Io(IoError::new(
            ErrorKind::InvalidData,
            "PEM file contains no supported private key",
        ))
    })
}

fn contains_pem_header(bytes: &[u8]) -> bool {
    bytes
        .windows(b"-----BEGIN ".len())
        .any(|window| window == b"-----BEGIN ")
}

#[cfg(test)]
mod tests {
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
