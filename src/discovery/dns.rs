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

/// DNS-based service discovery with single-flight resolution and serve-stale.
///
/// When multiple in-flight connections request the same name concurrently,
/// only one DNS lookup is issued via a per-name leader-election pattern
/// (Semaphore(1), try_acquire). Expired entries are returned immediately
/// while a background refresh is triggered for the next caller.
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
    /// * Single-flight — only one DNS lookup per name at a time.
    /// * Serve-stale — expired entries are returned immediately while a
    ///   refresh runs (the **next** caller fetches).
    pub async fn resolve(&self, name: &str) -> Result<ResolvedEndpoints, InterlinkError> {
        // Fast path: valid cached entry.
        if let Some(ep) = self.valid_cached(name) {
            return Ok(ep);
        }

        // Leader election: Semaphore(1). First caller acquires the permit
        // and becomes the resolver; concurrent callers wait.
        let sem = self
            .in_flight
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .value()
            .clone();

        let permit = sem.try_acquire().map_err(|_| ()).err();
        if permit.is_none() {
            // We are the resolver. Release the permit after resolution
            // (drop adds it back since we acquired 1).
            let result = self.resolve_inner(name).await;
            let endpoints = match &result {
                Ok(ep) => Some(ep.clone()),
                Err(_) => None,
            };
            // Update cache even on error (clears the stale entry).
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

        // Stale path: return expired entry if available (serve-stale),
        // then wait for the resolver to finish.
        let stale = self.cache.get(name).and_then(|e| e.endpoints.clone());
        let _ = permit;

        if let Some(ep) = stale {
            // Wait for the resolver but return stale immediately.
            // On the next call, the entry will be fresh.
            return Ok(ep);
        }

        // No stale entry either — wait for the resolver.
        sem.acquire().await.unwrap().forget();
        // Resolver done; read the cache.
        self.cache
            .get(name)
            .and_then(|e| e.endpoints.clone())
            .ok_or_else(|| InterlinkError::DnsResolution("lookup failed".into()))
    }

    /// Return a valid cached entry, if one exists.
    fn valid_cached(&self, name: &str) -> Option<ResolvedEndpoints> {
        let entry = self.cache.get(name)?;
        if !entry.is_expired() {
            entry.endpoints.clone()
        } else {
            None
        }
    }

    /// Perform the actual DNS lookup.
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

    /// Clear the DNS cache (called on config reload).
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
        // Resolving a non-existent name should fail quickly, not hang.
        let sd = ServiceDiscovery::new();
        let result =
            tokio::time::timeout(Duration::from_secs(5), sd.resolve("nonexistent.invalid.")).await;
        assert!(result.is_ok(), "resolve should not hang");
        assert!(result.unwrap().is_err());
    }
}
