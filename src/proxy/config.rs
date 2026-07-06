use serde::{Deserialize, Serialize};

/// Proxy configuration, loaded from file or environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub trust_domain: String,
    pub identity: Option<String>,
    pub default_upstream: Option<String>,
    pub max_connections: Option<usize>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            trust_domain: "cluster.local".into(),
            identity: None,
            default_upstream: None,
            max_connections: Some(1024),
        }
    }
}
