use std::sync::Arc;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use tracing::{error, info, warn};
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

/// Default Tokio worker threads, capped by the CPUs available to the process.
///
/// Two workers measured lower proxy CPU per request and higher bulk throughput
/// than one worker per CPU on the shared-host mesh benchmark: idle workers
/// cost wakeups, cross-worker stealing and per-thread allocator caches.
const DEFAULT_WORKER_THREADS: usize = 2;

/// `INTERLINK_WORKER_THREADS` (clamped to 1..=64), else the default above.
fn worker_threads(configured: Option<&str>, available: usize) -> usize {
    configured
        .and_then(|value| value.parse::<usize>().ok())
        .map(|threads| threads.clamp(1, 64))
        .unwrap_or_else(|| DEFAULT_WORKER_THREADS.min(available.max(1)))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let available = std::thread::available_parallelism().map_or(1, |n| n.get());
    let workers = worker_threads(
        std::env::var("INTERLINK_WORKER_THREADS").ok().as_deref(),
        available,
    );
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?
        .block_on(run(workers))
}

async fn run(worker_threads: usize) -> Result<(), Box<dyn std::error::Error>> {
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
        worker_threads,
        "starting interlinkd"
    );

    // 3. Require a configured, valid identity before starting any listeners.
    let identity = configured_identity(config.identity.as_deref())?;

    // 4. Bind the exporter to the configured address, loopback by default.
    let metrics_bind = std::net::SocketAddr::new(config.metrics_bind.parse()?, config.metrics_port);
    if let Err(e) = metrics::init_metrics_exporter(metrics_bind) {
        error!("failed to start metrics exporter: {}", e);
        // Non-fatal: the proxy can still serve traffic without metrics.
    }

    let credential_paths = TlsCredentialPaths::from_config(&config)?;
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

    // 5. Require the exact workload identity unless the operator explicitly
    // enables (and receives a warning for) the default-deny escape hatch.
    let policy = Arc::new(workload_policy(
        &identity,
        std::env::var("INTERLINK_ALLOW_ALL").as_deref() == Ok("true"),
    )?);
    let discovery = Arc::new(ServiceDiscovery::new()?);

    // 6. Build shutdown signalling.
    let (shutdown_tx, inbound_shutdown) = tokio::sync::watch::channel(false);
    let outbound_shutdown = shutdown_tx.subscribe();
    let admin_shutdown = shutdown_tx.subscribe();

    // 7. Build and start the admin server.
    let admin = Arc::new(
        AdminServer::new()
            .with_bind(config.admin_bind.parse()?)
            .with_token(config.admin_token.clone())
            .with_shutdown(admin_shutdown)
            .with_reload_handler(tls),
    );
    let admin_handle = admin.clone().spawn();

    // 8. Build and start the inbound TCP proxy.
    let inbound_config = config.to_proxy_config()?;
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
    )?
    .with_shutdown(inbound_shutdown);
    let inbound_handle = Arc::new(inbound_proxy).spawn();

    // 9. Build and start the outbound TCP proxy.
    let outbound_config = config.to_proxy_config()?;
    let outbound_proxy = OutboundProxy::new_with_discovery(
        outbound_config,
        config.proxy_outbound_port,
        tls_client,
        policy,
        Some(discovery),
    )?
    .with_shutdown(outbound_shutdown);
    let outbound_handle = Arc::new(outbound_proxy).spawn();
    admin.set_ready(true);

    info!("interlinkd ready");

    // 10. Kubernetes sends SIGTERM; local runs commonly use SIGINT.
    match wait_for_shutdown_signal().await {
        Ok(()) => info!("received shutdown signal"),
        Err(e) => error!("failed to listen for shutdown signal: {}", e),
    }

    // Signal proxies and admin server to stop accepting.
    admin.set_ready(false);
    let _ = shutdown_tx.send(true);
    let _ = inbound_handle.await;
    let _ = outbound_handle.await;
    let _ = admin_handle.await;
    info!("interlinkd shutdown complete");
    Ok(())
}

fn configured_identity(
    identity: Option<&str>,
) -> Result<SpiffeId, interlink::common::error::InterlinkError> {
    use interlink::common::error::InterlinkError;
    let identity =
        identity.ok_or_else(|| InterlinkError::Config("INTERLINK_IDENTITY must be set".into()))?;
    SpiffeId::from_uri(identity)
        .map_err(|error| InterlinkError::Config(format!("invalid INTERLINK_IDENTITY: {error}")))
}

fn workload_policy(
    identity: &SpiffeId,
    allow_all: bool,
) -> Result<PolicyEngine, interlink::common::error::InterlinkError> {
    let mut policy = PolicyEngine::new();
    let identity_uri = identity.to_uri();
    policy.set_global_policies(vec![interlink::policy::patterns::allow(
        &identity_uri,
        &identity_uri,
        "allow replicas of this workload identity",
    )?]);
    if allow_all {
        warn!("INTERLINK_ALLOW_ALL=true: default-deny authorization is DISABLED; all authenticated mesh identities are allowed");
        metrics::record_allow_all_enabled();
        policy.set_default_decision(interlink::policy::Decision::Allow);
    }
    Ok(policy)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// mimalloc's `no_thp` build never advises transparent huge pages
    /// (`MADV_HUGEPAGE`, the `hg` VmFlag). On madvise-mode hosts that advice
    /// multiplied the mesh benchmark's retained anonymous RSS.
    #[cfg(target_os = "linux")]
    #[test]
    fn allocator_never_advises_transparent_huge_pages() {
        let heap = std::hint::black_box(vec![1u8; 8 << 20]);
        let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
        drop(heap);
        let advised: Vec<_> = smaps
            .lines()
            .filter(|line| line.starts_with("VmFlags:"))
            .filter(|line| line.split_whitespace().any(|flag| flag == "hg"))
            .collect();
        assert!(advised.is_empty(), "huge-page advice: {advised:?}");
    }

    #[test]
    fn startup_allow_all_requires_explicit_enable() {
        let identity =
            configured_identity(Some("spiffe://example.local/ns/apps/sa/proxy")).unwrap();
        let outsider =
            configured_identity(Some("spiffe://example.local/ns/apps/sa/outsider")).unwrap();
        let policy = workload_policy(&identity, false).unwrap();
        assert!(matches!(
            policy.evaluate(&outsider, &identity),
            interlink::policy::Decision::Deny(_)
        ));
        assert!(matches!(
            policy.evaluate(&identity, &identity),
            interlink::policy::Decision::Allow
        ));
        let policy = workload_policy(&identity, true).unwrap();
        assert!(matches!(
            policy.evaluate(&outsider, &identity),
            interlink::policy::Decision::Allow
        ));
    }

    #[test]
    fn startup_requires_explicit_valid_identity() {
        for identity in [None, Some(""), Some("not-a-spiffe-id")] {
            let error = configured_identity(identity).unwrap_err();
            assert!(error.to_string().contains("INTERLINK_IDENTITY"));
        }
        let identity = "spiffe://example.local/ns/production/sa/proxy";
        assert_eq!(
            configured_identity(Some(identity)).unwrap().to_uri(),
            identity
        );
    }

    #[test]
    fn worker_threads_default_to_two_within_available_cpus() {
        assert_eq!(worker_threads(None, 8), 2);
        assert_eq!(worker_threads(None, 1), 1);
        assert_eq!(worker_threads(None, 0), 1);
        assert_eq!(worker_threads(Some("not-a-number"), 8), 2);
        assert_eq!(worker_threads(Some("6"), 2), 6, "explicit counts may exceed the default");
        assert_eq!(worker_threads(Some("0"), 8), 1);
        assert_eq!(worker_threads(Some("1000"), 8), 64);
    }
}
