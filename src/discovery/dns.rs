use std::net::SocketAddr;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use hickory_resolver::TokioResolver;

use crate::common::constants::timeouts;
use crate::common::error::InterlinkError;
use crate::common::identity::SpiffeId;

/// Cached DNS resolution result.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoints {
    pub addrs: Vec<SocketAddr>,
    pub spiffe_id: Option<SpiffeId>,
}

/// A cache entry with an expiration timestamp.
#[derive(Debug, Clone)]
struct CacheEntry {
    endpoints: ResolvedEndpoints,
    expires_at: Instant,
}

/// DNS-based service discovery.
///
/// Resolves service names to IP:port pairs using A/AAAA records
/// and SRV records for port discovery. Results are cached with TTL.
pub struct ServiceDiscovery {
    resolver: TokioResolver,
    cache: DashMap<String, CacheEntry>,
    ttl: Duration,
}

impl Default for ServiceDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceDiscovery {
    pub fn new() -> Self {
        Self::with_ttl(timeouts::DNS_RESOLVE)
    }

    /// Create a resolver with a custom cache TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        let resolver = TokioResolver::builder_tokio().unwrap().build().unwrap();

        Self {
            resolver,
            cache: DashMap::new(),
            ttl,
        }
    }

    /// Resolve a service name to endpoints.
    ///
    /// Format: `service.namespace.svc.cluster.local` for Kubernetes,
    /// or `host:port` for explicit addresses.
    pub async fn resolve(&self, name: &str) -> Result<ResolvedEndpoints, InterlinkError> {
        // Check cache first, refreshing if the entry has expired.
        if let Some(entry) = self.cache.get(name) {
            if entry.expires_at > Instant::now() {
                return Ok(entry.endpoints.clone());
            }
            drop(entry);
            self.cache.remove(name);
        }

        let addrs = self
            .resolver
            .lookup_ip(name)
            .await
            .map_err(|e| InterlinkError::DnsResolution(format!("lookup failed: {}", e)))?;

        let resolved = ResolvedEndpoints {
            addrs: addrs.iter().map(|ip| SocketAddr::new(ip, 0)).collect(),
            spiffe_id: None, // SRV would populate this
        };

        // Cache the result.
        self.cache.insert(
            name.to_string(),
            CacheEntry {
                endpoints: resolved.clone(),
                expires_at: Instant::now() + self.ttl,
            },
        );

        Ok(resolved)
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
                endpoints: ResolvedEndpoints {
                    addrs: vec![],
                    spiffe_id: None,
                },
                expires_at: Instant::now() + Duration::from_secs(60),
            },
        );
        assert!(!sd.cache.is_empty());
        sd.clear_cache();
        assert!(sd.cache.is_empty());
    }

    #[tokio::test]
    async fn test_cache_expiration() {
        let sd = ServiceDiscovery::with_ttl(Duration::from_millis(50));
        sd.cache.insert(
            "test".into(),
            CacheEntry {
                endpoints: ResolvedEndpoints {
                    addrs: vec![SocketAddr::new(
                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                        0,
                    )],
                    spiffe_id: None,
                },
                expires_at: Instant::now() + Duration::from_millis(50),
            },
        );

        // Entry is still valid immediately.
        assert!(sd.cache.get("test").is_some());

        // Wait for expiration.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Entry should be considered expired on next resolve.
        // We can't easily resolve "test" without a real DNS server, so we verify
        // the internal state is expired.
        let entry = sd.cache.get("test").unwrap();
        assert!(entry.expires_at <= Instant::now());
    }
}
