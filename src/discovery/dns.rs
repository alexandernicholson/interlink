// B8: No panic paths in connection-handling code — DNS is reachable from handle_connection.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::Semaphore;

use crate::common::constants::timeouts;
use crate::common::error::InterlinkError;
use crate::common::identity::SpiffeId;

/// Default DNS cache TTL: 30 seconds.
const DNS_CACHE_TTL: Duration = Duration::from_secs(30);

/// Cached DNS resolution result.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoints {
    pub addrs: Arc<[SocketAddr]>,
    pub spiffe_id: Option<SpiffeId>,
}

/// A cache entry with expiration.
struct CacheEntry {
    endpoints: Option<ResolvedEndpoints>,
    expires_at: Instant,
}

/// Abstraction over a DNS resolver (B11). Production uses `TokioResolver`;
/// tests use a counting fake to verify single-flight.
#[async_trait::async_trait]
pub trait DnsResolver: Send + Sync {
    async fn lookup(&self, name: &str) -> Result<Vec<SocketAddr>, InterlinkError>;
}

/// Production resolver backed by hickory (TokioResolver).
pub struct HickoryResolver {
    inner: hickory_resolver::TokioResolver,
}

#[async_trait::async_trait]
impl DnsResolver for HickoryResolver {
    async fn lookup(&self, name: &str) -> Result<Vec<SocketAddr>, InterlinkError> {
        let (host, port) = name
            .rsplit_once(':')
            .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
            .filter(|(host, port)| !host.is_empty() && *port != 0)
            .ok_or_else(|| {
                InterlinkError::DnsResolution(
                    "DNS endpoint requires an explicit nonzero port; SRV discovery is unsupported"
                        .into(),
                )
            })?;
        let addrs = self
            .inner
            .lookup_ip(host)
            .await
            .map_err(|e| InterlinkError::DnsResolution(format!("lookup failed: {}", e)))?;
        Ok(addrs.iter().map(|ip| SocketAddr::new(ip, port)).collect())
    }
}

impl HickoryResolver {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: hickory_resolver::TokioResolver::builder_tokio()?.build()?,
        })
    }
}

/// DNS-based service discovery with single-flight and refresh-on-expiry.
///
/// ## Cache lifecycle (B10):
/// | State | Reader action | Exit to |
/// |-------|--------------|---------|
/// | Empty (no entry) | → leader election, lookup | Fresh or Error |
/// | Fresh (expires_at > now) | return cached endpoints | Expired (after TTL) |
/// | Expired (expires_at ≤ now) | fall through → leader election, lookup | Fresh or Error |
/// | In-flight (leader resolving) | waiter blocks on semaphore | Fresh, Error, or retry after cancellation |
pub struct ServiceDiscovery {
    resolver: Arc<dyn DnsResolver>,
    cache: DashMap<String, CacheEntry>,
    in_flight: DashMap<String, Arc<Semaphore>>,
    ttl: Duration,
}

/// Removing the exact election entry on every exit (including cancellation)
/// allows a new leader; closing its semaphore wakes all existing followers.
struct FlightGuard<'a> {
    flights: &'a DashMap<String, Arc<Semaphore>>,
    name: &'a str,
    semaphore: Arc<Semaphore>,
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.flights
            .remove_if(self.name, |_, sem| Arc::ptr_eq(sem, &self.semaphore));
        self.semaphore.close();
    }
}

impl ServiceDiscovery {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            resolver: Arc::new(HickoryResolver::new()?),
            cache: DashMap::new(),
            in_flight: DashMap::new(),
            ttl: DNS_CACHE_TTL,
        })
    }

    /// Create with a custom resolver (used by tests to inject a counting fake).
    pub fn with_resolver(resolver: Arc<dyn DnsResolver>, ttl: Duration) -> Self {
        Self {
            resolver,
            cache: DashMap::new(),
            in_flight: DashMap::new(),
            ttl,
        }
    }

    /// Resolve a service name to endpoints.
    ///
    /// * Single-flight — at most one DNS lookup per name at a time.
    /// * Stale entries trigger a refresh on the next caller.
    pub async fn resolve(&self, name: &str) -> Result<ResolvedEndpoints, InterlinkError> {
        loop {
            if let Some(entry) = self.cache.get(name) {
                if !entry.is_expired() {
                    if let Some(ep) = &entry.endpoints {
                        return Ok(ep.clone());
                    }
                }
            }

            // One permit per election, held by the leader until completion or
            // cancellation. FlightGuard closes/removes the election and wakes
            // followers on either exit; followers retry against the cache/map.
            let sem = self
                .in_flight
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(1)))
                .value()
                .clone();
            if let Ok(_permit) = sem.try_acquire() {
                let _flight = FlightGuard {
                    flights: &self.in_flight,
                    name,
                    semaphore: sem.clone(),
                };
                // An earlier leader may have populated the cache between the
                // initial cache read and this election.
                if let Some(entry) = self.cache.get(name) {
                    if !entry.is_expired() {
                        if let Some(ep) = &entry.endpoints {
                            return Ok(ep.clone());
                        }
                    }
                }
                let result = tokio::time::timeout(timeouts::DNS_RESOLVE, self.resolve_inner(name))
                    .await
                    .map_err(|_| InterlinkError::DnsResolution("lookup timed out".into()))?;
                if let Ok(ep) = &result {
                    self.cache.insert(
                        name.to_string(),
                        CacheEntry {
                            endpoints: Some(ep.clone()),
                            expires_at: Instant::now() + self.ttl,
                        },
                    );
                }
                return result;
            }
            // Closed means the leader completed or was cancelled, not failure
            // of this request. Re-elect if no successful result was cached.
            let _waiter_permit = sem.acquire().await;
        }
    }

    async fn resolve_inner(&self, name: &str) -> Result<ResolvedEndpoints, InterlinkError> {
        let resolved = self.resolver.lookup(name).await?;
        if resolved.iter().any(|addr| addr.port() == 0) {
            return Err(InterlinkError::DnsResolution(
                "DNS returned an endpoint without a nonzero port".into(),
            ));
        }
        let addrs: Arc<[SocketAddr]> = resolved.into();
        Ok(ResolvedEndpoints {
            addrs,
            spiffe_id: None,
        })
    }

    pub fn clear_cache(&self) {
        self.cache.clear();
    }
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        self.expires_at <= Instant::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A counting fake resolver that tracks how many lookups are issued.
    struct CountingResolver {
        count: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl DnsResolver for CountingResolver {
        async fn lookup(&self, _name: &str) -> Result<Vec<SocketAddr>, InterlinkError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(InterlinkError::DnsResolution("fake failure".into()))
            } else {
                Ok(vec!["127.0.0.1:8080".parse().unwrap()])
            }
        }
    }

    #[tokio::test]
    async fn test_new_discovery() {
        let sd = ServiceDiscovery::new().unwrap();
        assert!(sd.cache.is_empty());
    }

    #[tokio::test]
    async fn test_cache_clear() {
        let sd = ServiceDiscovery::new().unwrap();
        sd.cache.insert(
            "test".into(),
            CacheEntry {
                endpoints: None,
                expires_at: Instant::now() + Duration::from_secs(60),
            },
        );
        assert!(!sd.cache.is_empty());
        sd.clear_cache();
        assert!(sd.cache.is_empty());
    }

    #[tokio::test]
    async fn test_resolve_failure_does_not_hang() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = Arc::new(CountingResolver {
            count: count.clone(),
            fail: true,
        });
        let sd = ServiceDiscovery::with_resolver(resolver, Duration::from_secs(30));

        let result =
            tokio::time::timeout(Duration::from_secs(5), sd.resolve("nonexistent.invalid.")).await;
        assert!(result.is_ok(), "resolve should not hang");
        assert!(result.unwrap().is_err());
    }

    /// A2 + A6 + A7 + A8: N concurrent resolve() calls for the same name
    /// must issue exactly 1 upstream lookup, and all must get the same result.
    /// Uses a CountingResolver to prove the dedup property.
    #[tokio::test]
    async fn test_concurrent_resolve_dedup() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = Arc::new(CountingResolver {
            count: count.clone(),
            fail: false,
        });
        let sd = Arc::new(ServiceDiscovery::with_resolver(
            resolver,
            Duration::from_secs(30),
        ));

        let name = "test.example.";
        let n = 10;

        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let sd = sd.clone();
            handles.push(tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(5), sd.resolve(name))
                    .await
                    .ok()
            }));
        }

        let mut results = Vec::with_capacity(n);
        for h in handles {
            results.push(h.await.unwrap());
        }

        // A7: exactly 1 lookup for N concurrent callers.
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "expected exactly 1 DNS lookup for {} concurrent resolves",
            n
        );

        // All N must get the same success result.
        for r in &results {
            assert!(r.is_some(), "all should complete (no timeout)");
            assert!(
                r.as_ref().unwrap().is_ok(),
                "all concurrent resolves should succeed"
            );
        }
        let first = &results[0].as_ref().unwrap().as_ref().unwrap().addrs;
        for r in &results[1..] {
            assert_eq!(
                r.as_ref().unwrap().as_ref().unwrap().addrs,
                *first,
                "all concurrent resolves should return identical endpoints"
            );
        }
    }

    /// A8: Concurrency test on the *successful* resolve path — the panicking
    /// waiter-success path that killed the process was only triggered by a
    /// successful lookup with concurrent waiters (the error-only test never
    /// exercised it).
    #[tokio::test]
    async fn test_concurrent_resolve_success_path() {
        let count = Arc::new(AtomicUsize::new(0));
        let resolver = Arc::new(CountingResolver {
            count: count.clone(),
            fail: false,
        });
        let sd = Arc::new(ServiceDiscovery::with_resolver(
            resolver,
            Duration::from_secs(30),
        ));

        let name = "success.example.";
        let n = 10;

        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let sd = sd.clone();
            handles.push(tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(5), sd.resolve(name))
                    .await
                    .ok()
            }));
        }

        for h in handles {
            let result = h.await.unwrap();
            assert!(result.is_some(), "should not timeout");
            assert!(result.unwrap().is_ok(), "should succeed");
        }
    }

    /// R15: Waiter-branch test — uses a gated resolver that parks the leader
    /// inside resolve_inner so that concurrent callers are forced into the
    /// semaphore waiter path. Proves the waiter does not panic and returns
    /// the same data as the leader.
    #[tokio::test]
    async fn test_waiter_branch() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let resolver = Arc::new(GatedResolver {
            gate: Arc::new(tokio::sync::Mutex::new(Some(rx))),
        });
        let sd = Arc::new(ServiceDiscovery::with_resolver(
            resolver,
            Duration::from_secs(30),
        ));

        let name = "gated.example.";
        let n = 5;

        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let sd = sd.clone();
            handles.push(tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(5), sd.resolve(name)).await
            }));
        }

        tokio::time::sleep(Duration::from_millis(200)).await;

        let _ = tx.send(true);

        let mut results: Vec<Result<ResolvedEndpoints, InterlinkError>> = Vec::with_capacity(n);
        for h in handles {
            let timeout_result = h.await.unwrap();
            assert!(
                timeout_result.is_ok(),
                "waiter-branch: timeout should not fire (deadlock?)"
            );
            let dns_result: Result<ResolvedEndpoints, InterlinkError> = timeout_result.unwrap();
            assert!(dns_result.is_ok(), "waiter-branch: all should succeed");
            results.push(dns_result);
        }

        let first_ep = results.swap_remove(0).unwrap();
        for r in results {
            let ep = r.unwrap();
            assert_eq!(
                ep.addrs, first_ep.addrs,
                "waiter-branch: all must return same addrs"
            );
        }
    }

    /// A gated resolver that blocks on a watch channel inside lookup().
    struct GatedResolver {
        gate: Arc<tokio::sync::Mutex<Option<tokio::sync::watch::Receiver<bool>>>>,
    }

    #[async_trait::async_trait]
    impl DnsResolver for GatedResolver {
        async fn lookup(&self, _name: &str) -> Result<Vec<SocketAddr>, InterlinkError> {
            let mut rx = self.gate.lock().await.take().unwrap();
            let _ = rx.changed().await;
            Ok(vec!["127.0.0.1:8080".parse().unwrap()])
        }
    }

    /// Gate the first lookup so aborting the leader leaves a real follower
    /// able to acquire a fresh election and complete.
    struct CancelResolver {
        calls: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl DnsResolver for CancelResolver {
        async fn lookup(&self, _name: &str) -> Result<Vec<SocketAddr>, InterlinkError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(vec!["127.0.0.1:8080".parse().unwrap()])
        }
    }

    #[tokio::test]
    async fn leader_cancel_removes_election_and_waiter_reresolves() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let resolver = Arc::new(CancelResolver {
                calls: AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            let sd = Arc::new(ServiceDiscovery::with_resolver(
                resolver.clone(),
                DNS_CACHE_TTL,
            ));
            let leader_sd = sd.clone();
            let leader = tokio::spawn(async move { leader_sd.resolve("cancel.example").await });
            resolver.entered.notified().await;
            let waiter_sd = sd.clone();
            let waiter = tokio::spawn(async move { waiter_sd.resolve("cancel.example").await });
            // Map + leader sem + flight guard + waiter sem proves the follower
            // is parked on this exact election, rather than hitting the cache.
            while Arc::strong_count(sd.in_flight.get("cancel.example").unwrap().value()) < 4 {
                tokio::task::yield_now().await;
            }
            assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
            leader.abort();
            assert!(leader.await.unwrap_err().is_cancelled());
            let result = waiter.await.unwrap().unwrap();
            assert_eq!(result.addrs[0].port(), 8080);
            assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
            assert!(sd.in_flight.is_empty());
            assert!(sd.resolve("cancel.example").await.is_ok());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn actual_lookup_timeout_cleans_election_and_allows_retry() {
        tokio::time::timeout(timeouts::DNS_RESOLVE + Duration::from_secs(3), async {
            let resolver = Arc::new(CancelResolver {
                calls: AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            let sd = ServiceDiscovery::with_resolver(resolver.clone(), DNS_CACHE_TTL);
            let error = sd.resolve("timeout.example").await.unwrap_err();
            assert!(error.to_string().contains("timed out"));
            assert!(sd.in_flight.is_empty());
            assert!(sd.resolve("timeout.example").await.is_ok());
            assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        })
        .await
        .unwrap();
    }

    struct ZeroPortResolver;

    #[async_trait::async_trait]
    impl DnsResolver for ZeroPortResolver {
        async fn lookup(&self, _name: &str) -> Result<Vec<SocketAddr>, InterlinkError> {
            Ok(vec!["127.0.0.1:0".parse().unwrap()])
        }
    }

    #[tokio::test]
    async fn zero_port_endpoints_are_rejected() {
        let sd = ServiceDiscovery::with_resolver(Arc::new(ZeroPortResolver), DNS_CACHE_TTL);
        let error = tokio::time::timeout(Duration::from_secs(1), sd.resolve("bare.example"))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("nonzero port"));
        assert!(sd.cache.is_empty());
    }

    #[tokio::test]
    async fn hickory_rejects_bare_hostname_and_accepts_explicit_port() {
        let resolver = HickoryResolver::new().unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), resolver.lookup("localhost"))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("explicit nonzero port"));
        assert!(resolver.lookup("localhost:0").await.is_err());
        let endpoints =
            tokio::time::timeout(Duration::from_secs(3), resolver.lookup("localhost:8080"))
                .await
                .unwrap()
                .unwrap();
        assert!(!endpoints.is_empty());
        assert!(endpoints.iter().all(|addr| addr.port() == 8080));
    }
}
