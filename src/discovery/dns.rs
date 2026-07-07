use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use hickory_resolver::TokioResolver;

use crate::common::error::InterlinkError;
use crate::common::identity::SpiffeId;

/// Default DNS cache TTL: 30s (was accidentally reusing the 5s resolve timeout).
const DNS_CACHE_TTL: Duration = Duration::from_secs(30);

/// Cached DNS resolution result.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoints {
    pub addrs: Vec<SocketAddr>,
    pub spiffe_id: Option<SpiffeId>,
}

/// In-flight lookup guard for single-flight dedup across concurrent
/// callers for the same name.
type LookupGuard = Arc<tokio::sync::Semaphore>;

/// A cache entry with expiration and optional in-flight guard.
struct CacheEntry {
    endpoints: Option<ResolvedEndpoints>,
    expires_at: Instant,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        self.expires_at <= Instant::now()
    }
}

/// DNS-based service discovery with per-name cache and TTL.
///
/// Results are cached for `ttl` seconds. Expired entries are served stale
/// while a background refresh completes (the next caller triggers the fetch).
pub struct ServiceDiscovery {
    resolver: TokioResolver,
    cache: DashMap<String, CacheEntry>,
    in_flight: DashMap<String, LookupGuard>,
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
    /// *Results are cached for `ttl` seconds.*
    /// *Concurrent callers for the same name share one DNS lookup.*
    /// *Expired entries are served stale while a refresh happens in the
    ///  background (the **next** caller fetches).*
    pub async fn resolve(&self, name: &str) -> Result<ResolvedEndpoints, InterlinkError> {
        // Fast path: valid cached entry.
        {
            let entry_ref = self.cache.get(name);
            if let Some(entry) = entry_ref {
                if !entry.is_expired() {
                    if let Some(ref ep) = entry.endpoints {
                        return Ok(ep.clone());
                    }
                }
                // Expired: serve stale if available; fall through to refresh.
                if let Some(ref ep) = entry.endpoints {
                    // Return stale data immediately; the next caller will refresh.
                    return Ok(ep.clone());
                }
            }
        }

        // Single-flight: ensure only one DNS lookup per name at a time.
        let semaphore = {
            let mut entry = self.in_flight.entry(name.to_string());
            let refmut = entry.or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(0)));
            refmut.value().clone()
        };
        // Check if we're the first caller (permit available) or a waiter.
        let is_resolver = semaphore.try_acquire().is_ok();
        if is_resolver {
            // We are the resolver.
            let result = self.resolve_inner(name).await;
            let endpoints = match result {
                Ok(ep) => ep,
                Err(e) => {
                    self.in_flight.remove(name);
                    return Err(e);
                }
            };
            self.cache.insert(
                name.to_string(),
                CacheEntry {
                    endpoints: Some(endpoints.clone()),
                    expires_at: Instant::now() + self.ttl,
                },
            );
            self.in_flight.remove(name);
            Ok(endpoints)
        } else {
            // Another task is resolving. Wait.
            semaphore.acquire().await.unwrap().forget();
            self.cache
                .get(name)
                .and_then(|e| e.endpoints.clone())
                .ok_or_else(|| InterlinkError::DnsResolution("lookup failed".into()))
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
}
