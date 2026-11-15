/// DNS-based service discovery (RFC 1034, RFC 1035).
///
/// Resolves explicit `hostname:port` endpoints via A/AAAA lookups. SRV port
/// discovery (RFC 2782) is unsupported; bare hostnames fail rather than yielding port 0.
pub mod dns;

pub use dns::ServiceDiscovery;
