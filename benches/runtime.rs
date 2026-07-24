mod support;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures_util::future::join_all;
use interlink::admin::{AdminServer, ReloadHandler};
use interlink::common::config::Config;
use interlink::common::error::InterlinkError;
use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::discovery::dns::{DnsResolver, HickoryResolver};
use interlink::discovery::ServiceDiscovery;
use interlink::identity::provider::{KubernetesIdentityProvider, StaticIdentityProvider};
use interlink::metrics;
use interlink::policy::Decision;
use interlink::proxy::config::ProxyConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

struct FakeResolver {
    calls: AtomicUsize,
    delay: Duration,
    fail: bool,
}

impl FakeResolver {
    fn ready(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            delay,
            fail: false,
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            fail: true,
        })
    }
}

#[async_trait]
impl DnsResolver for FakeResolver {
    async fn lookup(&self, _name: &str) -> Result<Vec<SocketAddr>, InterlinkError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.fail {
            Err(InterlinkError::DnsResolution("injected failure".into()))
        } else {
            Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)])
        }
    }
}

struct SwitchableReload {
    fail: AtomicBool,
}

#[async_trait]
impl ReloadHandler for SwitchableReload {
    async fn reload(&self) -> Result<(), InterlinkError> {
        if self.fail.load(Ordering::Relaxed) {
            Err(InterlinkError::Config("measured reload failure".into()))
        } else {
            Ok(())
        }
    }
}

fn bench_discovery_paths(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let hit_resolver = FakeResolver::ready(Duration::ZERO);
    let hit_discovery = Arc::new(ServiceDiscovery::with_resolver(
        hit_resolver,
        Duration::from_secs(30),
    ));
    runtime
        .block_on(hit_discovery.resolve("backend.local"))
        .unwrap();

    let miss_resolver = FakeResolver::ready(Duration::ZERO);
    let miss_discovery = Arc::new(ServiceDiscovery::with_resolver(
        miss_resolver,
        Duration::from_secs(30),
    ));

    let expired_resolver = FakeResolver::ready(Duration::ZERO);
    let expired_discovery = Arc::new(ServiceDiscovery::with_resolver(
        expired_resolver,
        Duration::ZERO,
    ));

    let failing_discovery = Arc::new(ServiceDiscovery::with_resolver(
        FakeResolver::failing(),
        Duration::from_secs(30),
    ));

    let gated_resolver = FakeResolver::ready(Duration::from_millis(1));
    let singleflight_discovery = Arc::new(ServiceDiscovery::with_resolver(
        gated_resolver.clone(),
        Duration::from_secs(30),
    ));

    let mut group = c.benchmark_group("dns_discovery");
    group.bench_function("cache_hit", |b| {
        b.to_async(&runtime).iter(|| async {
            black_box(
                hit_discovery
                    .resolve(black_box("backend.local"))
                    .await
                    .unwrap(),
            )
        })
    });
    group.bench_function("cache_hit_vec_copy_control", |b| {
        b.to_async(&runtime).iter(|| async {
            let resolved = hit_discovery
                .resolve(black_box("backend.local"))
                .await
                .unwrap();
            black_box(resolved.addrs.to_vec())
        })
    });
    group.bench_function("cache_miss", |b| {
        b.to_async(&runtime).iter(|| async {
            miss_discovery.clear_cache();
            black_box(
                miss_discovery
                    .resolve(black_box("backend.local"))
                    .await
                    .unwrap(),
            )
        })
    });
    group.bench_function("expired_refresh", |b| {
        b.to_async(&runtime).iter(|| async {
            black_box(
                expired_discovery
                    .resolve(black_box("backend.local"))
                    .await
                    .unwrap(),
            )
        })
    });
    group.bench_function("lookup_error", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = failing_discovery.resolve(black_box("backend.local")).await;
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("singleflight_8", |b| {
        b.to_async(&runtime).iter(|| async {
            singleflight_discovery.clear_cache();
            let calls_before = gated_resolver.calls.load(Ordering::Relaxed);
            let futures = (0..8).map(|_| {
                let discovery = singleflight_discovery.clone();
                async move { discovery.resolve("backend.local").await }
            });
            let results = join_all(futures).await;
            assert!(results.iter().all(Result::is_ok));
            assert_eq!(
                gated_resolver.calls.load(Ordering::Relaxed) - calls_before,
                1,
                "eight concurrent misses must perform one lookup"
            );
            black_box(results)
        })
    });
    group.bench_function("hickory_new", |b| {
        b.iter(|| black_box(HickoryResolver::new().unwrap()))
    });
    group.bench_function("service_new", |b| {
        b.iter(|| black_box(ServiceDiscovery::new().unwrap()))
    });
    group.finish();
}

fn config_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "interlink-runtime-bench-{}.json",
        std::process::id()
    ))
}

fn bench_config_and_provider_paths(c: &mut Criterion) {
    const ENV_KEYS: &[&str] = &[
        "INTERLINK_CONFIG_FILE",
        "INTERLINK_TRUST_DOMAIN",
        "INTERLINK_MAX_CONNECTIONS",
        "INTERLINK_MUX",
        "INTERLINK_IDENTITY",
        "KUBERNETES_NAMESPACE",
        "POD_NAMESPACE",
        "KUBERNETES_SERVICE_ACCOUNT_NAME",
    ];
    let saved_env: Vec<_> = ENV_KEYS
        .iter()
        .map(|key| (*key, std::env::var_os(key)))
        .collect();

    let path = config_path();
    let file_config = Config {
        trust_domain: "bench.local".into(),
        mux: false,
        ..Config::default()
    };
    std::fs::write(&path, serde_json::to_vec(&file_config).unwrap()).unwrap();

    let valid = Config::default();
    let invalid_domain = Config {
        trust_domain: String::new(),
        ..Config::default()
    };
    let invalid_connections = Config {
        max_connections: 0,
        ..Config::default()
    };
    let invalid_identity = Config {
        identity: Some("not-a-spiffe-id".into()),
        ..Config::default()
    };

    let identity = SpiffeId::try_new("bench.local", "default", "service").unwrap();
    let provider = StaticIdentityProvider::with_ca_bundle(identity.clone(), vec![vec![1, 2, 3, 4]]);
    let plain_provider =
        StaticIdentityProvider::new(identity.clone(), TrustDomain::new("bench.local"));

    let mut group = c.benchmark_group("config_provider");
    group.bench_function("config_default", |b| {
        b.iter(|| black_box(Config::default()))
    });
    group.bench_function("proxy_config_default", |b| {
        b.iter(|| black_box(ProxyConfig::default()))
    });
    group.bench_function("proxy_config_serde_default_mux", |b| {
        let json = r#"{"trust_domain":"bench.local","identity":null,"default_upstream":null,"max_connections":1024}"#;
        b.iter(|| {
            let config: ProxyConfig = serde_json::from_str(black_box(json)).unwrap();
            assert!(config.mux);
            black_box(config)
        })
    });
    group.bench_function("load_file", |b| {
        b.iter(|| {
            let config = Config::default().with_file(black_box(&path)).unwrap();
            assert!(!config.mux);
            black_box(config)
        })
    });
    group.bench_function("validate", |b| {
        b.iter(|| {
            black_box(&valid).validate().unwrap();
            black_box(())
        })
    });
    group.bench_function("reject_empty_domain", |b| {
        b.iter(|| {
            let result = black_box(&invalid_domain).validate();
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("reject_zero_connections", |b| {
        b.iter(|| {
            let result = black_box(&invalid_connections).validate();
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("reject_identity", |b| {
        b.iter(|| {
            let result = black_box(&invalid_identity).validate();
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("to_proxy_config", |b| {
        b.iter(|| black_box(black_box(&valid).to_proxy_config()))
    });

    std::env::set_var("INTERLINK_CONFIG_FILE", "/nonexistent/interlink-bench.json");
    std::env::set_var("INTERLINK_TRUST_DOMAIN", "bench.local");
    std::env::set_var("INTERLINK_MAX_CONNECTIONS", "512");
    std::env::set_var("INTERLINK_MUX", "false");
    std::env::set_var(
        "INTERLINK_IDENTITY",
        "spiffe://bench.local/ns/default/sa/service",
    );
    group.bench_function("with_env", |b| {
        b.iter(|| {
            let config = Config::default().with_env().unwrap();
            assert_eq!(config.max_connections, 512);
            assert!(!config.mux);
            black_box(config)
        })
    });
    group.bench_function("load_env", |b| {
        b.iter(|| black_box(Config::load().unwrap()))
    });
    group.bench_function("kubernetes_explicit_identity", |b| {
        b.iter(|| black_box(KubernetesIdentityProvider::new("bench.local").unwrap()))
    });

    std::env::remove_var("INTERLINK_IDENTITY");
    std::env::set_var("KUBERNETES_NAMESPACE", "default");
    std::env::set_var("KUBERNETES_SERVICE_ACCOUNT_NAME", "service");
    group.bench_function("kubernetes_env_fallback", |b| {
        b.iter(|| black_box(KubernetesIdentityProvider::new("bench.local").unwrap()))
    });

    group.bench_function("static_new", |b| {
        b.iter(|| {
            black_box(StaticIdentityProvider::new(
                identity.clone(),
                TrustDomain::new("bench.local"),
            ))
        })
    });
    group.bench_function("static_identity", |b| {
        b.iter(|| black_box(provider.get_identity().unwrap()))
    });
    group.bench_function("static_trust_domain", |b| {
        b.iter(|| black_box(provider.get_trust_domain()))
    });
    group.bench_function("plain_static_identity", |b| {
        b.iter(|| black_box(plain_provider.get_identity().unwrap()))
    });
    group.finish();

    std::fs::remove_file(path).unwrap();
    for (key, value) in saved_env {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

async fn admin_request(addr: SocketAddr, request: &'static [u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    stream.shutdown().await.unwrap();
    let mut response = Vec::with_capacity(128);
    stream.read_to_end(&mut response).await.unwrap();
    response
}

fn reserve_port() -> u16 {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn bench_admin_paths(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let port = reserve_port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let reload = Arc::new(SwitchableReload {
        fail: AtomicBool::new(false),
    });
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = Arc::new(
        AdminServer::new()
            .with_port(port)
            .with_shutdown(shutdown_rx)
            .with_reload_handler(reload.clone()),
    );
    let task = {
        let _runtime_guard = runtime.enter();
        server.clone().spawn()
    };
    let unavailable_port = reserve_port();
    let unavailable_addr = SocketAddr::from(([127, 0, 0, 1], unavailable_port));
    let (unavailable_shutdown_tx, unavailable_shutdown_rx) = watch::channel(false);
    let unavailable_server = Arc::new(
        AdminServer::new()
            .with_port(unavailable_port)
            .with_shutdown(unavailable_shutdown_rx),
    );
    let unavailable_task = {
        let _runtime_guard = runtime.enter();
        unavailable_server.spawn()
    };
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if TcpStream::connect(addr).await.is_ok()
                    && TcpStream::connect(unavailable_addr).await.is_ok()
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });

    let mut group = c.benchmark_group("admin_http");
    group.bench_function("health", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = admin_request(addr, b"GET /healthz HTTP/1.1\r\n\r\n").await;
            assert!(response.starts_with(b"HTTP/1.1 200 OK"));
            black_box(response)
        })
    });

    server.set_ready(false);
    group.bench_function("not_ready", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = admin_request(addr, b"GET /readyz HTTP/1.1\r\n\r\n").await;
            assert!(response.starts_with(b"HTTP/1.1 503"));
            black_box(response)
        })
    });

    server.set_ready(true);
    group.bench_function("ready", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = admin_request(addr, b"GET /readyz HTTP/1.1\r\n\r\n").await;
            assert!(response.starts_with(b"HTTP/1.1 200 OK"));
            black_box(response)
        })
    });
    group.bench_function("reload", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = admin_request(addr, b"POST /reload HTTP/1.1\r\n\r\n").await;
            assert!(response.ends_with(b"reload complete"));
            black_box(response)
        })
    });
    reload.fail.store(true, Ordering::Relaxed);
    group.bench_function("reload_failed", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = admin_request(addr, b"POST /reload HTTP/1.1\r\n\r\n").await;
            assert!(response.ends_with(b"reload failed"));
            black_box(response)
        })
    });
    group.bench_function("reload_unavailable", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = admin_request(unavailable_addr, b"POST /reload HTTP/1.1\r\n\r\n").await;
            assert!(response.ends_with(b"reload unavailable"));
            black_box(response)
        })
    });
    group.bench_function("not_found", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = admin_request(addr, b"GET /missing HTTP/1.1\r\n\r\n").await;
            assert!(response.starts_with(b"HTTP/1.1 404"));
            black_box(response)
        })
    });
    group.finish();

    shutdown_tx.send(true).unwrap();
    unavailable_shutdown_tx.send(true).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), async {
            task.await.unwrap();
            unavailable_task.await.unwrap();
        })
        .await
        .unwrap();
    });
}

fn bench_metric_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics");
    group.bench_function("connection_success", |b| {
        b.iter(|| {
            metrics::record_connection_start();
            metrics::record_connection(black_box(64), black_box(128), Duration::from_micros(10));
        })
    });
    group.bench_function("connection_failure", |b| {
        b.iter(|| {
            metrics::record_connection_start();
            metrics::record_connection_failed();
        })
    });
    group.bench_function("handshake_success", |b| {
        b.iter(|| metrics::record_handshake(black_box(Duration::from_micros(100))))
    });
    group.bench_function("handshake_failure", |b| {
        b.iter(metrics::record_handshake_error)
    });
    group.bench_function("handshake_full", |b| {
        b.iter(|| metrics::record_handshake_kind(black_box(false)))
    });
    group.bench_function("handshake_resumed", |b| {
        b.iter(|| metrics::record_handshake_kind(black_box(true)))
    });
    group.bench_function("policy_allow", |b| {
        b.iter(|| metrics::record_policy(black_box(&Decision::Allow)))
    });
    group.bench_function("policy_deny", |b| {
        let decision = Decision::Deny("benchmark");
        b.iter(|| metrics::record_policy(black_box(&decision)))
    });
    group.bench_function("saturation", |b| {
        b.iter(metrics::record_saturation_rejection)
    });
    group.bench_function("mux_tunnel", |b| {
        b.iter(|| {
            metrics::record_connection_start();
            metrics::record_mux_tunnel_opened();
            metrics::record_tunnel_closed();
        })
    });
    group.bench_function("mux_stream", |b| b.iter(metrics::record_mux_stream));
    group.finish();
}

criterion_group!(
    name = benches;
    config = support::criterion();
    targets = bench_discovery_paths, bench_config_and_provider_paths, bench_admin_paths,
              bench_metric_paths
);
criterion_main!(benches);
