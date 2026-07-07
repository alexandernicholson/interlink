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
        .unwrap_or_else(|| SpiffeId::new(&config.trust_domain, "default", "proxy"));

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
    let tls_server = Arc::new(TlsServer::new(
        provider.clone(),
        proxy_cert.clone(),
        proxy_key.clone_key(),
    )?);

    let tls_client = Arc::new(TlsClient::with_client_auth(
        provider.clone(),
        proxy_cert,
        proxy_key,
    )?);

    // 7. Build policy engine and service discovery.
    let mut policy_engine = PolicyEngine::new();
    // Allow all traffic when INTERLINK_ALLOW_ALL is set (benchmark mode).
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
        // Skip service discovery when a static default_upstream is configured.
        if config.default_upstream.is_some() {
            None
        } else {
            Some(discovery.clone())
        },
    )
    .with_shutdown(inbound_shutdown);
    let inbound_handle = Arc::new(inbound_proxy).spawn();

    // 10. Build and start the outbound TCP proxy.
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

    // 11. Wait for shutdown signal.
    match tokio::signal::ctrl_c().await {
        Ok(()) => info!("received shutdown signal"),
        Err(e) => error!("failed to listen for ctrl-c: {}", e),
    }

    // Signal proxies and admin server to stop accepting.
    let _ = shutdown_tx.send(true);
    let _ = inbound_handle.await;
    let _ = outbound_handle.await;
    let _ = admin_handle.await;
    info!("interlinkd shutdown complete");
    Ok(())
}

fn load_ca_bundle(config: &Config) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
    let path = config
        .ca_bundle_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| default_cert_dir().map(|d| d.join("ca.der")))
        .ok_or("INTERLINK_CA_BUNDLE_PATH not set and no default cert dir found")?;

    info!("loading CA bundle from {:?}", path);
    let bytes = std::fs::read(&path)?;
    Ok(vec![bytes])
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
    let cert = std::fs::read(&cert_path)?;
    let key = std::fs::read(&key_path)?;

    Ok((
        CertificateDer::from(cert),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
    ))
}

fn default_cert_dir() -> Option<PathBuf> {
    std::env::var("INTERLINK_CERT_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(std::env::temp_dir().join("interlink-demo")))
}
