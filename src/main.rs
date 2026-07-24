use std::sync::Arc;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use interlink::admin::AdminServer;
use interlink::common::config::Config;
use interlink::common::identity::SpiffeId;
use interlink::discovery::ServiceDiscovery;
use interlink::metrics;
use interlink::policy::PolicyEngine;
use interlink::proxy::outbound::OutboundProxy;
use interlink::proxy::tcp::TcpProxy;
use interlink::proxy::{TlsCredentialPaths, TlsCredentialReloader, TlsHandshake};

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

    // 4. Derive the expected SPIFFE identity and load the first TLS generation.
    let identity = config
        .identity
        .as_ref()
        .and_then(|s| SpiffeId::from_uri(s).ok())
        .unwrap_or_else(|| {
            SpiffeId::try_new(&config.trust_domain, "default", "proxy")
                .expect("trust_domain must not be empty in config")
        });

    let credential_paths = TlsCredentialPaths::from_config(&config);
    info!(
        ca_bundle = %credential_paths.ca_bundle().display(),
        certificate = %credential_paths.certificate().display(),
        private_key = %credential_paths.private_key().display(),
        "loading TLS credentials"
    );
    let tls = Arc::new(
        TlsCredentialReloader::load(credential_paths, identity.clone(), config.mux)
            .await
            .map_err(|error| {
                error!(%error, "failed to load TLS credentials");
                error
            })?,
    );
    let tls_server: Arc<dyn TlsHandshake> = tls.clone();
    let tls_client: Arc<dyn TlsHandshake> = tls.clone();

    // 5. Default-deny except for peers presenting this workload's exact
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

    // 6. Build shutdown signalling.
    let (shutdown_tx, inbound_shutdown) = tokio::sync::watch::channel(false);
    let outbound_shutdown = shutdown_tx.subscribe();
    let admin_shutdown = shutdown_tx.subscribe();

    // 7. Build and start the admin server.
    let admin = Arc::new(
        AdminServer::new()
            .with_shutdown(admin_shutdown)
            .with_reload_handler(tls),
    );
    let admin_handle = admin.spawn();

    // 8. Build and start the inbound TCP proxy.
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

    // 9. Build and start the outbound TCP proxy.
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

    // 10. Kubernetes sends SIGTERM; local runs commonly use SIGINT.
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
