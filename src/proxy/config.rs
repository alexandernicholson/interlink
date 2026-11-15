use std::net::{IpAddr, Ipv4Addr};

use crate::common::error::InterlinkError;
use crate::common::identity::SpiffeId;
use serde::{Deserialize, Serialize};

/// Proxy configuration, loaded from file or environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub trust_domain: String,
    /// Required, valid SPIFFE URI; proxy constructors reject absent identities.
    pub identity: Option<String>,
    pub default_upstream: Option<String>,
    pub max_connections: Option<usize>,
    /// Multiplex proxy-to-proxy connections over shared mTLS tunnels
    /// (ALPN-negotiated; falls back to 1:1 relay with non-mux peers).
    #[serde(default = "default_mux_true")]
    pub mux: bool,
    /// Local application-facing listener; loopback by default, not mesh-facing.
    #[serde(default = "default_outbound_bind")]
    pub outbound_bind: IpAddr,
    /// Upstream IP CIDRs allowed for either proxy. Empty allows all destinations.
    #[serde(default)]
    pub egress_subnets: Vec<String>,
    #[serde(default = "default_tunnel_max_age_secs")]
    pub tunnel_max_age_secs: u64,
    #[serde(default = "default_mux_max_streams_per_tunnel")]
    /// Stream cap; configure matching caps on both mesh endpoints.
    pub mux_max_streams_per_tunnel: usize,
}

pub fn default_mux_true() -> bool {
    true
}

fn default_outbound_bind() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn default_tunnel_max_age_secs() -> u64 {
    3600
}

fn default_mux_max_streams_per_tunnel() -> usize {
    64
}

impl ProxyConfig {
    pub(crate) fn local_identity(&self) -> Result<SpiffeId, InterlinkError> {
        let identity = self
            .identity
            .as_deref()
            .ok_or_else(|| InterlinkError::Config("proxy identity is required".into()))?;
        SpiffeId::from_uri(identity)
    }
}

/// Parsed once at construction; the per-connection gate only matches IPs.
pub(crate) struct EgressPolicy(Vec<ipnet::IpNet>);

impl EgressPolicy {
    pub(crate) fn parse(entries: &[String]) -> Result<Self, InterlinkError> {
        entries
            .iter()
            .map(|entry| {
                entry
                    .parse()
                    .map_err(|_| InterlinkError::Config(format!("invalid egress CIDR: {entry:?}")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub(crate) fn allows(&self, ip: IpAddr) -> bool {
        self.0.is_empty() || self.0.iter().any(|subnet| subnet.contains(&ip))
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            trust_domain: "cluster.local".into(),
            identity: None,
            default_upstream: None,
            max_connections: Some(1024),
            mux: true,
            outbound_bind: default_outbound_bind(),
            egress_subnets: Vec::new(),
            tunnel_max_age_secs: default_tunnel_max_age_secs(),
            mux_max_streams_per_tunnel: default_mux_max_streams_per_tunnel(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_defaults_deserialize_without_widening_listener_bind() {
        let config: ProxyConfig = serde_json::from_str(
            r#"{"trust_domain":"test.local","identity":null,"default_upstream":null,"max_connections":4}"#,
        ).unwrap();
        assert!(config.outbound_bind.is_loopback());
        assert!(config.egress_subnets.is_empty());
        assert_eq!(config.tunnel_max_age_secs, 3600);
        assert_eq!(config.mux_max_streams_per_tunnel, 64);
    }

    #[test]
    fn egress_cidrs_match_boundaries_and_address_families() {
        let policy =
            EgressPolicy::parse(&["192.0.2.7/24".into(), "2001:db8::7/64".into()]).unwrap();
        for address in ["192.0.2.0", "192.0.2.255", "2001:db8::", "2001:db8::ffff"] {
            assert!(policy.allows(address.parse().unwrap()), "{address}");
        }
        for address in [
            "192.0.1.255",
            "192.0.3.0",
            "2001:db8:0:1::",
            "::ffff:192.0.2.7",
        ] {
            assert!(!policy.allows(address.parse().unwrap()), "{address}");
        }
        let any_v4 = EgressPolicy::parse(&["0.0.0.0/0".into()]).unwrap();
        assert!(any_v4.allows("203.0.113.255".parse().unwrap()));
        assert!(!any_v4.allows("::1".parse().unwrap()));
        let any_v6 = EgressPolicy::parse(&["::/0".into()]).unwrap();
        assert!(any_v6.allows("::1".parse().unwrap()));
        assert!(!any_v6.allows("127.0.0.1".parse().unwrap()));
    }
}
