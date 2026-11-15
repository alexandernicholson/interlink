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
    port: u16,
}

impl FakeResolver {
    fn ready(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            delay,
            fail: false,
            port: 8080,
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            fail: true,
            port: 0,
        })
    }

    fn zero_port() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            fail: false,
            port: 0,
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
            Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port)])
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

    let zero_port_discovery = Arc::new(ServiceDiscovery::with_resolver(
        FakeResolver::zero_port(),
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
    group.bench_function("zero_port_rejected", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = zero_port_discovery.resolve(black_box("backend.local")).await;
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

    let slow_resolver = FakeResolver::ready(Duration::from_secs(6));
    let timeout_discovery = Arc::new(ServiceDiscovery::with_resolver(
        slow_resolver,
        Duration::from_secs(3600),
    ));
    let cancel_resolver = FakeResolver::ready(Duration::from_millis(1500));
    let cancel_discovery = Arc::new(ServiceDiscovery::with_resolver(
        cancel_resolver,
        Duration::from_secs(3600),
    ));
    let mut dns_limits = c.benchmark_group("dns_limits");
    dns_limits.sample_size(10);
    dns_limits.bench_function("resolve_timeout", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = timeout_discovery.resolve(black_box("timeout.local")).await;
            assert!(result.is_err());
            black_box(result)
        })
    });
    dns_limits.bench_function("cancelled_leader_reresolves", |b| {
        b.to_async(&runtime).iter(|| async {
            cancel_discovery.clear_cache();
            let leader = tokio::spawn({
                let discovery = cancel_discovery.clone();
                async move { discovery.resolve("cancel.local").await }
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            leader.abort();
            let result = cancel_discovery.resolve("cancel.local").await;
            assert!(
                result.is_ok(),
                "cancelled leader must not wedge the election: {result:?}"
            );
            black_box(result)
        })
    });
    dns_limits.finish();
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
    let security_path = std::env::temp_dir().join(format!(
        "interlink-runtime-bench-security-{}.json",
        std::process::id()
    ));
    let security_file_config = Config {
        trust_domain: "bench.local".into(),
        outbound_bind: "127.0.0.1".into(),
        metrics_bind: "127.0.0.1".into(),
        admin_bind: "127.0.0.1:4914".into(),
        admin_token: Some("file-token".into()),
        egress_subnets: vec!["127.0.0.0/8".into()],
        tunnel_max_age_secs: 240,
        mux_max_streams_per_tunnel: 12,
        ..Config::default()
    };
    std::fs::write(
        &security_path,
        serde_json::to_vec(&security_file_config).unwrap(),
    )
    .unwrap();


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
    let invalid_admin_bind = Config {
        admin_bind: "not-an-addr".into(),
        ..Config::default()
    };
    let invalid_outbound_bind = Config {
        outbound_bind: "999.0.0.1".into(),
        ..Config::default()
    };
    let zero_tunnel_age = Config {
        tunnel_max_age_secs: 0,
        ..Config::default()
    };
    let security_fields = Config {
        outbound_bind: "127.0.0.1".into(),
        metrics_bind: "127.0.0.1".into(),
        admin_bind: "127.0.0.1:4912".into(),
        admin_token: Some("bench-token".into()),
        egress_subnets: vec!["127.0.0.0/8".into(), "fd00::/8".into()],
        tunnel_max_age_secs: 120,
        mux_max_streams_per_tunnel: 8,
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
    group.bench_function("load_file_security_fields", |b| {
        b.iter(|| {
            let config = Config::default()
                .with_file(black_box(&security_path))
                .unwrap();
            assert_eq!(config.admin_bind, "127.0.0.1:4914");
            assert_eq!(config.admin_token.as_deref(), Some("file-token"));
            assert_eq!(config.egress_subnets, vec!["127.0.0.0/8".to_string()]);
            assert_eq!(config.tunnel_max_age_secs, 240);
            assert_eq!(config.mux_max_streams_per_tunnel, 12);
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
        b.iter(|| black_box(black_box(&valid).to_proxy_config().unwrap()))
    });
    group.bench_function("reject_invalid_admin_bind", |b| {
        b.iter(|| {
            let result = black_box(&invalid_admin_bind).validate();
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("reject_invalid_outbound_bind", |b| {
        b.iter(|| {
            let result = black_box(&invalid_outbound_bind).validate();
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("reject_zero_tunnel_max_age", |b| {
        b.iter(|| {
            let result = black_box(&zero_tunnel_age).validate();
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("to_proxy_config_security_fields", |b| {
        b.iter(|| {
            let proxy = black_box(&security_fields).to_proxy_config().unwrap();
            assert_eq!(
                proxy.outbound_bind,
                "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
            );
            assert_eq!(proxy.tunnel_max_age_secs, 120);
            assert_eq!(proxy.mux_max_streams_per_tunnel, 8);
            assert_eq!(proxy.egress_subnets.len(), 2);
            black_box(proxy)
        })
    });
    group.bench_function("reject_invalid_metrics_bind", |b| {
        b.iter(|| {
            let config = Config {
                metrics_bind: "not-an-ip".into(),
                ..Config::default()
            };
            let result = black_box(&config).validate();
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("reject_empty_admin_token", |b| {
        b.iter(|| {
            let config = Config {
                admin_token: Some(String::new()),
                ..Config::default()
            };
            let result = black_box(&config).validate();
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("reject_zero_mux_streams", |b| {
        b.iter(|| {
            let config = Config {
                mux_max_streams_per_tunnel: 0,
                ..Config::default()
            };
            let result = black_box(&config).validate();
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("config_serde_defaults", |b| {
        b.iter(|| {
            let config: Config = serde_json::from_str(
                r#"{"trust_domain":"bench.local","identity":null,"proxy_inbound_port":4143,"proxy_outbound_port":4140,"metrics_port":4190,"log_level":"info","max_connections":1024,"ca_bundle_path":null,"cert_path":null,"key_path":null,"default_upstream":null,"mux":true}"#,
            )
            .unwrap();
            assert_eq!(config.outbound_bind, "127.0.0.1");
            assert_eq!(config.metrics_bind, "127.0.0.1");
            assert_eq!(config.admin_bind, "127.0.0.1:4192");
            assert_eq!(config.tunnel_max_age_secs, 3600);
            assert_eq!(config.mux_max_streams_per_tunnel, 64);
            assert!(config.egress_subnets.is_empty());
            black_box(config)
        })
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
    std::env::set_var("INTERLINK_ADMIN_BIND", "127.0.0.1:4913");
    std::env::set_var("INTERLINK_ADMIN_TOKEN", "bench-token");
    std::env::set_var("INTERLINK_OUTBOUND_BIND", "127.0.0.1");
    std::env::set_var("INTERLINK_METRICS_BIND", "127.0.0.1");
    std::env::set_var("INTERLINK_EGRESS_SUBNETS", "127.0.0.0/8,fd00::/8");
    std::env::set_var("INTERLINK_TUNNEL_MAX_AGE_SECS", "90");
    std::env::set_var("INTERLINK_MUX_MAX_STREAMS", "16");
    group.bench_function("with_env_security_fields", |b| {
        b.iter(|| {
            let config = Config::default().with_env().unwrap();
            assert_eq!(config.admin_bind, "127.0.0.1:4913");
            assert_eq!(config.admin_token.as_deref(), Some("bench-token"));
            assert_eq!(config.egress_subnets.len(), 2);
            assert_eq!(config.tunnel_max_age_secs, 90);
            assert_eq!(config.mux_max_streams_per_tunnel, 16);
            black_box(config)
        })
    });
    std::env::remove_var("INTERLINK_ADMIN_BIND");
    std::env::remove_var("INTERLINK_ADMIN_TOKEN");
    std::env::remove_var("INTERLINK_OUTBOUND_BIND");
    std::env::remove_var("INTERLINK_METRICS_BIND");
    std::env::remove_var("INTERLINK_EGRESS_SUBNETS");
    std::env::remove_var("INTERLINK_TUNNEL_MAX_AGE_SECS");
    std::env::remove_var("INTERLINK_MUX_MAX_STREAMS");
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
    std::fs::remove_file(security_path).unwrap();
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
            .with_bind(addr)
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
            .with_bind(unavailable_addr)
            .with_shutdown(unavailable_shutdown_rx),
    );
    let unavailable_task = {
        let _runtime_guard = runtime.enter();
        unavailable_server.spawn()
    };
    let token_port = reserve_port();
    let token_addr = SocketAddr::from(([127, 0, 0, 1], token_port));
    let (token_shutdown_tx, token_shutdown_rx) = watch::channel(false);
    let token_server = Arc::new(
        AdminServer::new()
            .with_bind(token_addr)
            .with_token(Some("bench-token".into()))
            .with_shutdown(token_shutdown_rx)
            .with_reload_handler(reload.clone()),
    );
    let token_task = {
        let _runtime_guard = runtime.enter();
        token_server.clone().spawn()
    };
    let cap_port = reserve_port();
    let cap_addr = SocketAddr::from(([127, 0, 0, 1], cap_port));
    let (cap_shutdown_tx, cap_shutdown_rx) = watch::channel(false);
    let cap_server = Arc::new(
        AdminServer::new()
            .with_bind(cap_addr)
            .with_shutdown(cap_shutdown_rx),
    );
    let cap_task = {
        let _runtime_guard = runtime.enter();
        cap_server.clone().spawn()
    };
    let open_port = reserve_port();
    let open_addr = SocketAddr::from(([127, 0, 0, 1], open_port));
    let (open_shutdown_tx, open_shutdown_rx) = watch::channel(false);
    let open_server = Arc::new(
        AdminServer::new()
            .with_bind(SocketAddr::from(([0, 0, 0, 0], open_port)))
            .with_shutdown(open_shutdown_rx)
            .with_reload_handler(reload.clone()),
    );
    let open_task = {
        let _runtime_guard = runtime.enter();
        open_server.clone().spawn()
    };
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if TcpStream::connect(addr).await.is_ok()
                    && TcpStream::connect(unavailable_addr).await.is_ok()
                    && TcpStream::connect(token_addr).await.is_ok()
                    && TcpStream::connect(cap_addr).await.is_ok()
                    && TcpStream::connect(open_addr).await.is_ok()
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
    group.bench_function("reload_with_valid_token", |b| {
        b.to_async(&runtime).iter(|| async {
            // The shared reload fixture is left failing by the
            // reload_failed case above; this case asserts the authorized
            // success path, so restore it first.
            reload.fail.store(false, Ordering::Relaxed);
            let response = admin_request(
                token_addr,
                b"POST /reload HTTP/1.1\r\nAuthorization: Bearer bench-token\r\n\r\n",
            )
            .await;
            assert!(response.ends_with(b"reload complete"));
            black_box(response)
        })
    });
    group.bench_function("reload_with_wrong_token", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = admin_request(
                token_addr,
                b"POST /reload HTTP/1.1\r\nAuthorization: Bearer not-the-token\r\n\r\n",
            )
            .await;
            assert!(response.starts_with(b"HTTP/1.1 401"));
            black_box(response)
        })
    });
    group.bench_function("reload_missing_token_on_token_server", |b| {
        b.to_async(&runtime).iter(|| async {
            let response =
                admin_request(token_addr, b"POST /reload HTTP/1.1\r\n\r\n").await;
            assert!(response.starts_with(b"HTTP/1.1 401"));
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
    let mut limits_group = c.benchmark_group("admin_limits");
    limits_group.sample_size(10);
    limits_group.bench_function("admin_idle_connection_timeout", |b| {
        b.to_async(&runtime).iter(|| async {
            let mut stream = TcpStream::connect(cap_addr).await.unwrap();
            let mut response = Vec::new();
            let dropped = tokio::time::timeout(
                Duration::from_secs(6),
                stream.read_to_end(&mut response),
            )
            .await;
            assert!(
                dropped.is_ok(),
                "idle admin connection must be dropped by the read deadline"
            );
            black_box(response)
        })
    });
    limits_group.bench_function("admin_connection_cap_exceeded", |b| {
        b.to_async(&runtime).iter(|| async {
            let mut held = Vec::new();
            for _ in 0..64 {
                held.push(TcpStream::connect(cap_addr).await.unwrap());
            }
            let mut extra = TcpStream::connect(cap_addr).await.unwrap();
            let _ = extra.shutdown().await;
            let mut response = Vec::new();
            let _ = tokio::time::timeout(
                Duration::from_millis(500),
                extra.read_to_end(&mut response),
            )
            .await;
            assert!(
                response.is_empty(),
                "connection beyond the admin cap must be dropped without a response"
            );
            drop(held);
            black_box(response)
        })
    });
    limits_group.bench_function("reload_refused_on_non_loopback_without_token", |b| {
        b.to_async(&runtime).iter(|| async {
            // Server is bound to 0.0.0.0 (non-loopback) with no token: reload
            // must be refused even though a handler is attached.
            let response = admin_request(open_addr, b"POST /reload HTTP/1.1\r\n\r\n").await;
            assert!(response.starts_with(b"HTTP/1.1 503"));
            black_box(response)
        })
    });
    limits_group.finish();

    shutdown_tx.send(true).unwrap();
    unavailable_shutdown_tx.send(true).unwrap();
    token_shutdown_tx.send(true).unwrap();
    cap_shutdown_tx.send(true).unwrap();
    open_shutdown_tx.send(true).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), async {
            task.await.unwrap();
            unavailable_task.await.unwrap();
            token_task.await.unwrap();
            cap_task.await.unwrap();
            open_task.await.unwrap();
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
    group.bench_function("init_exporter", |b| {
        b.iter(|| {
            // Driving the exporter init path: a repeated install may be
            // rejected by the metrics runtime — an outcome of this
            // control-plane case, not a bench failure.
            let result = metrics::init_metrics_exporter(black_box(SocketAddr::from((
                [127, 0, 0, 1],
                0,
            ))));
            black_box(result)
        })
    });
    group.finish();
}

criterion_group!(
    name = benches;
    config = support::criterion();
    targets = bench_discovery_paths, bench_config_and_provider_paths, bench_admin_paths,
              bench_metric_paths
);
criterion_main!(benches);
