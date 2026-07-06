use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::common::error::InterlinkError;

/// Runtime configuration loaded from environment variables or config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub trust_domain: String,
    pub identity: Option<String>,
    pub proxy_inbound_port: u16,
    pub proxy_outbound_port: u16,
    pub metrics_port: u16,
    pub log_level: String,
    pub max_connections: usize,
    /// Path to the CA bundle (DER or PEM) used to validate peer certificates.
    pub ca_bundle_path: Option<String>,
    /// Path to the leaf certificate (DER) this proxy presents.
    pub cert_path: Option<String>,
    /// Path to the leaf private key (PKCS#8) for this proxy.
    pub key_path: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            trust_domain: "cluster.local".into(),
            identity: None,
            proxy_inbound_port: crate::common::constants::ports::INBOUND_PROXY,
            proxy_outbound_port: crate::common::constants::ports::OUTBOUND_PROXY,
            metrics_port: crate::common::constants::ports::METRICS,
            log_level: "info".into(),
            max_connections: 1024,
            ca_bundle_path: None,
            cert_path: None,
            key_path: None,
        }
    }
}

impl Config {
    /// Load configuration using the following precedence (highest first):
    /// 1. Environment variables with the `INTERLINK_` prefix.
    /// 2. Values from the config file at `INTERLINK_CONFIG_FILE` or the default path.
    /// 3. Built-in defaults.
    pub fn load() -> Result<Self, InterlinkError> {
        let mut cfg = Self::default();

        // Layer 1: optional config file.
        let config_file = std::env::var("INTERLINK_CONFIG_FILE")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| Some("/etc/interlink/config.json".to_string()));

        if let Some(path) = config_file {
            if Path::new(&path).exists() {
                cfg = cfg.with_file(&path)?;
            }
        }

        // Layer 2: environment variables override file values.
        cfg = cfg.with_env()?;

        cfg.validate()?;
        Ok(cfg)
    }

    /// Merge values from a JSON config file.
    pub fn with_file(mut self, path: impl AsRef<Path>) -> Result<Self, InterlinkError> {
        let contents = std::fs::read_to_string(path.as_ref())
            .map_err(|e| InterlinkError::Config(format!("failed to read config file: {}", e)))?;
        let file_cfg: Config = serde_json::from_str(&contents)
            .map_err(|e| InterlinkError::Config(format!("invalid config file: {}", e)))?;

        self.trust_domain = file_cfg.trust_domain;
        if file_cfg.identity.is_some() {
            self.identity = file_cfg.identity;
        }
        self.proxy_inbound_port = file_cfg.proxy_inbound_port;
        self.proxy_outbound_port = file_cfg.proxy_outbound_port;
        self.metrics_port = file_cfg.metrics_port;
        self.log_level = file_cfg.log_level;
        self.max_connections = file_cfg.max_connections;
        if file_cfg.ca_bundle_path.is_some() {
            self.ca_bundle_path = file_cfg.ca_bundle_path;
        }
        if file_cfg.cert_path.is_some() {
            self.cert_path = file_cfg.cert_path;
        }
        if file_cfg.key_path.is_some() {
            self.key_path = file_cfg.key_path;
        }

        Ok(self)
    }

    /// Merge values from environment variables.
    pub fn with_env(mut self) -> Result<Self, InterlinkError> {
        if let Ok(v) = std::env::var("INTERLINK_TRUST_DOMAIN") {
            self.trust_domain = v;
        }
        if let Ok(v) = std::env::var("INTERLINK_IDENTITY") {
            self.identity = Some(v);
        }
        if let Ok(v) = std::env::var("INTERLINK_PROXY_INBOUND_PORT") {
            self.proxy_inbound_port = parse_port(&v)?;
        }
        if let Ok(v) = std::env::var("INTERLINK_PROXY_OUTBOUND_PORT") {
            self.proxy_outbound_port = parse_port(&v)?;
        }
        if let Ok(v) = std::env::var("INTERLINK_METRICS_PORT") {
            self.metrics_port = parse_port(&v)?;
        }
        if let Ok(v) = std::env::var("INTERLINK_LOG_LEVEL") {
            self.log_level = v;
        }
        if let Ok(v) = std::env::var("INTERLINK_MAX_CONNECTIONS") {
            self.max_connections = v
                .parse::<usize>()
                .map_err(|e| InterlinkError::Config(format!("INTERLINK_MAX_CONNECTIONS: {}", e)))?;
        }
        if let Ok(v) = std::env::var("INTERLINK_CA_BUNDLE_PATH") {
            self.ca_bundle_path = Some(v);
        }
        if let Ok(v) = std::env::var("INTERLINK_CERT_PATH") {
            self.cert_path = Some(v);
        }
        if let Ok(v) = std::env::var("INTERLINK_KEY_PATH") {
            self.key_path = Some(v);
        }
        Ok(self)
    }

    /// Convert the common runtime config into a proxy-specific config.
    pub fn to_proxy_config(&self) -> crate::proxy::config::ProxyConfig {
        crate::proxy::config::ProxyConfig {
            trust_domain: self.trust_domain.clone(),
            identity: self.identity.clone(),
            default_upstream: None,
            max_connections: Some(self.max_connections),
        }
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), InterlinkError> {
        if self.trust_domain.is_empty() {
            return Err(InterlinkError::Config(
                "trust_domain must not be empty".into(),
            ));
        }
        if self.max_connections == 0 {
            return Err(InterlinkError::Config("max_connections must be > 0".into()));
        }
        if let Some(ref id) = self.identity {
            crate::common::identity::SpiffeId::from_uri(id).map_err(|e| {
                InterlinkError::Config(format!("invalid INTERLINK_IDENTITY: {}", e))
            })?;
        }
        Ok(())
    }
}

fn parse_port(s: &str) -> Result<u16, InterlinkError> {
    s.parse::<u16>()
        .map_err(|e| InterlinkError::Config(format!("invalid port '{}': {}", s, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.trust_domain, "cluster.local");
        assert!(cfg.identity.is_none());
        assert_eq!(cfg.max_connections, 1024);
    }

    #[test]
    fn test_config_from_env() {
        // Use a non-existent config file path so only env vars are applied.
        std::env::set_var("INTERLINK_CONFIG_FILE", "/nonexistent/interlink.json");
        std::env::set_var("INTERLINK_TRUST_DOMAIN", "env.local");
        std::env::set_var("INTERLINK_MAX_CONNECTIONS", "512");
        std::env::remove_var("INTERLINK_IDENTITY");

        let cfg = Config::load().unwrap();
        assert_eq!(cfg.trust_domain, "env.local");
        assert_eq!(cfg.max_connections, 512);
        assert!(cfg.identity.is_none());

        std::env::remove_var("INTERLINK_CONFIG_FILE");
        std::env::remove_var("INTERLINK_TRUST_DOMAIN");
        std::env::remove_var("INTERLINK_MAX_CONNECTIONS");
    }

    #[test]
    fn test_config_invalid_identity() {
        let cfg = Config {
            identity: Some("not-a-spiffe-id".into()),
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_config_empty_trust_domain() {
        let cfg = Config {
            trust_domain: "".into(),
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }
}
