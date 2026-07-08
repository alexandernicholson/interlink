use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::Semaphore;

use crate::common::error::InterlinkError;
use crate::common::identity::SpiffeId;

/// Default DNS cache TTL: 30 seconds.
const DNS_CACHE_TTL: Duration = Duration::from_secs(30);

/// Cached DNS resolution result.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoints {
    pub addrs: Vec<SocketAddr>,
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
        let addrs = self
            .inner
            .lookup_ip(name)
            .await
            .map_err(|e| InterlinkError::DnsResolution(format!("lookup failed: {}", e)))?;
        Ok(addrs.iter().map(|ip| SocketAddr::new(ip, 0)).collect())
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
/// | In-flight (leader resolving) | waiter blocks on semaphore | Fresh (leader done) |
pub struct ServiceDiscovery {
    resolver: Arc<dyn DnsResolver>,
    cache: DashMap<String, CacheEntry>,
    in_flight: DashMap<String, Arc<Semaphore>>,
    ttl: Duration,
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
        // Fast path: valid cached entry.
        if let Some(entry) = self.cache.get(name) {
            if !entry.is_expired() {
                if let Some(ref ep) = entry.endpoints {
                    return Ok(ep.clone());
                }
            }
            // Expired: fall through to leader election (triggers refresh).
        }

        // Leader election: Semaphore(1).
        // B9: early returns — no laundering control flow through a binding.
        let sem = self
            .in_flight
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .value()
            .clone();

        if let Ok(_permit) = sem.try_acquire() {
            let result = self.resolve_inner(name).await;
            let endpoints = match &result {
                Ok(ep) => Some(ep.clone()),
                Err(_) => None,
            };
            self.cache.insert(
                name.to_string(),
                CacheEntry {
                    endpoints,
                    expires_at: Instant::now() + self.ttl,
                },
            );
            self.in_flight.remove(name);
            return result;
        }

        // Waiter: wait for leader to finish.
        let _waiter_permit = sem
            .acquire()
            .await
            .map_err(|_| InterlinkError::DnsResolution("semaphore closed during resolve".into()))?;
        self.cache
            .get(name)
            .and_then(|e| e.endpoints.clone())
            .ok_or_else(|| InterlinkError::DnsResolution("lookup failed".into()))
    }

    async fn resolve_inner(&self, name: &str) -> Result<ResolvedEndpoints, InterlinkError> {
        let addrs = self.resolver.lookup(name).await?;
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
}
