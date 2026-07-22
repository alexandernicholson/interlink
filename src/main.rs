use std::io::{BufReader, Error as IoError, ErrorKind};
use std::path::PathBuf;
use std::sync::Arc;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use interlink::admin::AdminServer;
use interlink::common::config::Config;
use interlink::common::identity::{IdentityProvider, SpiffeId};
use interlink::discovery::ServiceDiscovery;
use interlink::identity::provider::StaticIdentityProvider;
use interlink::metrics;
use interlink::policy::PolicyEngine;
use interlink::proxy::handshake::{TlsClient, TlsServer};
use interlink::proxy::outbound::OutboundProxy;
use interlink::proxy::tcp::TcpProxy;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("interlinkd v{}", env!("CARGO_PKG_VERSION"));

    // 1. Load runtime configuration.
    let config = Config::load()?;

    // 2. Initialize logging.
    let filter = EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    info!(
        trust_domain = %config.trust_domain,
        inbound_port = config.proxy_inbound_port,
        outbound_port = config.proxy_outbound_port,
        metrics_port = config.metrics_port,
        "starting interlinkd"
    );

    // 3. Initialize metrics exporter.
    if let Err(e) = metrics::init_metrics_exporter() {
        error!("failed to start metrics exporter: {}", e);
        // Non-fatal: the proxy can still serve traffic without metrics.
    }

    // 4. Build identity provider.
    let identity = config
        .identity
        .as_ref()
        .and_then(|s| SpiffeId::from_uri(s).ok())
        .unwrap_or_else(|| {
            SpiffeId::try_new(&config.trust_domain, "default", "proxy")
                .expect("trust_domain must not be empty in config")
        });

    let ca_bundle = load_ca_bundle(&config).map_err(|e| {
        error!("failed to load CA bundle: {}", e);
        e
    })?;

    let provider: Arc<dyn IdentityProvider> = Arc::new(StaticIdentityProvider::with_ca_bundle(
        identity.clone(),
        ca_bundle,
    ));

    // 5. Load leaf certificate and key for the proxy's TLS identity.
    let (proxy_cert, proxy_key) = load_cert_and_key(&config).map_err(|e| {
        error!("failed to load proxy certificate/key: {}", e);
        e
    })?;

    // 6. Build the TLS server (inbound mTLS) and client (outbound mTLS).
    // INTERLINK_MUX=false removes il/mux/1 from the ALPN offer on both
    // sides — the offer *is* the feature flag; once negotiated, mux is
    // always spoken (the wire protocol is committed at the handshake).
    let (tls_server, tls_client): (Arc<TlsServer>, Arc<TlsClient>) = if config.mux {
        (
            Arc::new(TlsServer::new(
                provider.clone(),
                proxy_cert.clone(),
                proxy_key.clone_key(),
            )?),
            Arc::new(TlsClient::with_client_auth(
                provider.clone(),
                proxy_cert,
                proxy_key,
            )?),
        )
    } else {
        (
            Arc::new(TlsServer::new_with_alpn(
                provider.clone(),
                proxy_cert.clone(),
                proxy_key.clone_key(),
                interlink::proxy::handshake::legacy_alpn_protocols(),
            )?),
            Arc::new(TlsClient::with_client_auth_alpn(
                provider.clone(),
                proxy_cert,
                proxy_key,
                interlink::proxy::handshake::legacy_alpn_protocols(),
            )?),
        )
    };

    // 7. Default-deny except for peers presenting this workload's exact
    // SPIFFE identity. Cluster replicas intentionally share one service
    // identity; a certificate signed by the trust bundle is still required.
    let mut policy_engine = PolicyEngine::new();
    let identity_uri = identity.to_uri();
    policy_engine.set_global_policies(vec![interlink::policy::patterns::allow(
        &identity_uri,
        &identity_uri,
        "allow replicas of this workload identity",
    )]);
    // Explicit benchmark escape hatch; never set this in a workload manifest.
    if std::env::var("INTERLINK_ALLOW_ALL").as_deref() == Ok("true") {
        policy_engine.set_default_decision(interlink::policy::Decision::Allow);
    }
    let policy = Arc::new(policy_engine);
    let discovery = Arc::new(ServiceDiscovery::new()?);

    // 8. Build shutdown signalling.
    let (shutdown_tx, inbound_shutdown) = tokio::sync::watch::channel(false);
    let outbound_shutdown = shutdown_tx.subscribe();
    let admin_shutdown = shutdown_tx.subscribe();

    // 9. Build and start the admin server.
    let admin = Arc::new(AdminServer::new().with_shutdown(admin_shutdown));
    let admin_handle = admin.spawn();

    // 10. Build and start the inbound TCP proxy.
    let inbound_config = config.to_proxy_config();
    let inbound_proxy = TcpProxy::new_with_discovery(
        inbound_config,
        config.proxy_inbound_port,
        tls_server,
        policy.clone(),
        if config.default_upstream.is_some() {
            None
        } else {
            Some(discovery.clone())
        },
    )
    .with_shutdown(inbound_shutdown);
    let inbound_handle = Arc::new(inbound_proxy).spawn();

    // 11. Build and start the outbound TCP proxy.
    let outbound_config = config.to_proxy_config();
    let outbound_proxy = OutboundProxy::new_with_discovery(
        outbound_config,
        config.proxy_outbound_port,
        tls_client,
        policy,
        Some(discovery),
    )
    .with_shutdown(outbound_shutdown);
    let outbound_handle = Arc::new(outbound_proxy).spawn();

    info!("interlinkd ready");

    // 12. Kubernetes sends SIGTERM; local runs commonly use SIGINT.
    match wait_for_shutdown_signal().await {
        Ok(()) => info!("received shutdown signal"),
        Err(e) => error!("failed to listen for shutdown signal: {}", e),
    }

    // Signal proxies and admin server to stop accepting.
    let _ = shutdown_tx.send(true);
    let _ = inbound_handle.await;
    let _ = outbound_handle.await;
    let _ = admin_handle.await;
    info!("interlinkd shutdown complete");
    Ok(())
}

async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

fn load_ca_bundle(config: &Config) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
    let path = config
        .ca_bundle_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| default_cert_dir().map(|d| d.join("ca.der")))
        .ok_or("INTERLINK_CA_BUNDLE_PATH not set and no default cert dir found")?;

    info!("loading CA bundle from {:?}", path);
    parse_certificates(std::fs::read(&path)?).map(|certs| {
        certs
            .into_iter()
            .map(|cert| cert.as_ref().to_vec())
            .collect()
    })
}

fn load_cert_and_key(
    config: &Config,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), Box<dyn std::error::Error>> {
    let cert_path = config
        .cert_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| default_cert_dir().map(|d| d.join("server.der")))
        .ok_or("INTERLINK_CERT_PATH not set and no default cert dir found")?;

    let key_path = config
        .key_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| default_cert_dir().map(|d| d.join("server.key")))
        .ok_or("INTERLINK_KEY_PATH not set and no default cert dir found")?;

    info!(
        "loading proxy cert from {:?}, key from {:?}",
        cert_path, key_path
    );
    let cert = parse_certificates(std::fs::read(&cert_path)?)?
        .into_iter()
        .next()
        .ok_or_else(|| IoError::new(ErrorKind::InvalidData, "certificate file is empty"))?;
    let key_bytes = std::fs::read(&key_path)?;
    let key = if contains_pem_header(&key_bytes) {
        rustls_pemfile::private_key(&mut BufReader::new(key_bytes.as_slice()))?.ok_or_else(
            || {
                IoError::new(
                    ErrorKind::InvalidData,
                    "PEM file contains no supported private key",
                )
            },
        )?
    } else {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes))
    };

    Ok((cert, key))
}

fn parse_certificates(
    bytes: Vec<u8>,
) -> Result<Vec<CertificateDer<'static>>, Box<dyn std::error::Error>> {
    if !contains_pem_header(&bytes) {
        return Ok(vec![CertificateDer::from(bytes)]);
    }

    let certs = rustls_pemfile::certs(&mut BufReader::new(bytes.as_slice()))
        .collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(
            IoError::new(ErrorKind::InvalidData, "PEM file contains no certificates").into(),
        );
    }
    Ok(certs)
}

fn contains_pem_header(bytes: &[u8]) -> bool {
    bytes
        .windows(b"-----BEGIN ".len())
        .any(|window| window == b"-----BEGIN ")
}

fn default_cert_dir() -> Option<PathBuf> {
    std::env::var("INTERLINK_CERT_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(std::env::temp_dir().join("interlink-demo")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cert_dir(test_name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "interlink-{test_name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn parses_every_certificate_in_a_pem_bundle() {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let pem = generated.cert.pem();
        let bundle = format!("{pem}\n{pem}");

        let parsed = parse_certificates(bundle.into_bytes()).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].as_ref(), generated.cert.der().as_ref());
        assert_eq!(parsed[1].as_ref(), generated.cert.der().as_ref());
    }

    #[test]
    fn loads_cert_manager_pem_secret_files() {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let dir = temp_cert_dir("pem-secret");
        let ca_path = dir.join("ca.crt");
        let cert_path = dir.join("tls.crt");
        let key_path = dir.join("tls.key");
        std::fs::write(&ca_path, generated.cert.pem()).unwrap();
        std::fs::write(&cert_path, generated.cert.pem()).unwrap();
        std::fs::write(&key_path, generated.key_pair.serialize_pem()).unwrap();
        let config = Config {
            ca_bundle_path: Some(ca_path.to_string_lossy().into_owned()),
            cert_path: Some(cert_path.to_string_lossy().into_owned()),
            key_path: Some(key_path.to_string_lossy().into_owned()),
            ..Config::default()
        };

        let ca_bundle = load_ca_bundle(&config).unwrap();
        let (cert, key) = load_cert_and_key(&config).unwrap();

        assert_eq!(ca_bundle, vec![generated.cert.der().as_ref().to_vec()]);
        assert_eq!(cert.as_ref(), generated.cert.der().as_ref());
        assert_eq!(
            key.secret_der(),
            generated.key_pair.serialize_der().as_slice()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
