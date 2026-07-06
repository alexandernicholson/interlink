/// DNS-based service discovery (RFC 1034, RFC 1035).
///
/// Resolves service names to IP addresses. Supports SRV records
/// (RFC 2782) for port discovery and standard A/AAAA lookups.
pub mod dns;

pub use dns::ServiceDiscovery;
