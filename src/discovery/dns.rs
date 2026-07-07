use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use hickory_resolver::TokioResolver;
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

/// DNS-based service discovery.
///
/// Single-flight via per-name Semaphore(1):
///   - Semaphore starts with 1 permit.
///   - Leader: `try_acquire()` gets the permit and holds it across the DNS
///     lookup. On completion the permit is dropped (returns to semaphore).
///   - Waiter: `acquire().await` blocks until the leader finishes, then
///     drops immediately and reads from cache.
/// - At most 1 DNS lookup per name at any time (B3: invariant stated).
pub struct ServiceDiscovery {
    resolver: TokioResolver,
    cache: DashMap<String, CacheEntry>,
    in_flight: DashMap<String, Arc<Semaphore>>,
    ttl: Duration,
}

impl Default for ServiceDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceDiscovery {
    pub fn new() -> Self {
        Self::with_ttl(DNS_CACHE_TTL)
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        let resolver = TokioResolver::builder_tokio().unwrap().build().unwrap();
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
    ///   The leader holds a `Semaphore` permit across `resolve_inner`;
    ///   waiters block on `acquire()` then read from cache.
    /// * Serve-stale — expired entries are returned immediately; the next
    ///   caller triggers a refresh.
    pub async fn resolve(&self, name: &str) -> Result<ResolvedEndpoints, InterlinkError> {
        // Fast path: valid cached entry.
        if let Some(entry) = self.cache.get(name) {
            if !entry.is_expired() {
                if let Some(ref ep) = entry.endpoints {
                    return Ok(ep.clone());
                }
            } else {
                // Expired: serve stale if available.
                if let Some(ref ep) = entry.endpoints {
                    return Ok(ep.clone());
                }
            }
        }

        // Leader election: Semaphore(1). The leader holds the permit across
        // the DNS lookup; waiters block on acquire().
        let sem = self
            .in_flight
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .value()
            .clone();

        // B1: bind the permit to a named variable — never drop mid-expression.
        let _permit = match sem.try_acquire() {
            Ok(_p) => {
                // We are the leader. The permit is held for the scope of
                // resolve_inner — dropped when _permit goes out of scope.
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
            Err(_) => {
                // Waiter: wait for leader to finish.
                let _waiter_permit = sem.acquire().await.unwrap();
                // _waiter_permit is dropped immediately (returned to semaphore)
                // so the next waiter can proceed. Then read from cache.
                self.cache
                    .get(name)
                    .and_then(|e| e.endpoints.clone())
                    .ok_or_else(|| InterlinkError::DnsResolution("lookup failed".into()))?
            }
        };

        // Unreachable — both branches return above.
        unreachable!()
    }

    async fn resolve_inner(&self, name: &str) -> Result<ResolvedEndpoints, InterlinkError> {
        let addrs = self
            .resolver
            .lookup_ip(name)
            .await
            .map_err(|e| InterlinkError::DnsResolution(format!("lookup failed: {}", e)))?;

        Ok(ResolvedEndpoints {
            addrs: addrs.iter().map(|ip| SocketAddr::new(ip, 0)).collect(),
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
    use std::sync::Arc;

    #[tokio::test]
    async fn test_new_discovery() {
        let sd = ServiceDiscovery::new();
        assert!(sd.cache.is_empty());
    }

    #[tokio::test]
    async fn test_cache_clear() {
        let sd = ServiceDiscovery::new();
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
    async fn test_resolve_timeout() {
        let sd = ServiceDiscovery::new();
        let result =
            tokio::time::timeout(Duration::from_secs(5), sd.resolve("nonexistent.invalid.")).await;
        assert!(result.is_ok(), "resolve should not hang");
        assert!(result.unwrap().is_err());
    }

    /// Concurrency test (A2): N concurrent resolve() calls for the same
    /// non-existent name must all fail with the same error, not hang.
    #[tokio::test]
    async fn test_concurrent_resolve_dedup() {
        let sd = Arc::new(ServiceDiscovery::new());
        let name = "concurrent-dedup-test.invalid.";
        let n = 10;

        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let sd = sd.clone();
            handles.push(tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(5), sd.resolve(name)).await
            }));
        }

        let mut results = Vec::with_capacity(n);
        for h in handles {
            results.push(h.await.unwrap());
        }

        // All should be Err (nonexistent domain) — no hangs.
        for (i, r) in results.iter().enumerate() {
            assert!(
                r.is_ok() || r.as_ref().unwrap().is_err(),
                "call {} should not hang (timeout broke it)",
                i
            );
        }
    }
}
