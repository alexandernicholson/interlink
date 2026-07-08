use serde::{Deserialize, Serialize};

/// Proxy configuration, loaded from file or environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub trust_domain: String,
    pub identity: Option<String>,
    pub default_upstream: Option<String>,
    pub max_connections: Option<usize>,
    /// Multiplex proxy-to-proxy connections over shared mTLS tunnels
    /// (ALPN-negotiated; falls back to 1:1 relay with non-mux peers).
    #[serde(default = "default_mux_true")]
    pub mux: bool,
}

pub fn default_mux_true() -> bool {
    true
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            trust_domain: "cluster.local".into(),
            identity: None,
            default_upstream: None,
            max_connections: Some(1024),
            mux: true,
        }
    }
}
