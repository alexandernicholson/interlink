use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
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
    /// Path to the leaf certificate (DER or PEM) this proxy presents.
    pub cert_path: Option<String>,
    /// Path to a PEM private key or raw PKCS#8 DER key for this proxy.
    pub key_path: Option<String>,
    /// Default upstream address for when SO_ORIGINAL_DST is unavailable.
    pub default_upstream: Option<String>,
    /// Multiplex proxy-to-proxy connections over shared mTLS tunnels.
    #[serde(default = "crate::proxy::config::default_mux_true")]
    pub mux: bool,
    /// Outbound listener IP; defaults to loopback to keep the relay local.
    #[serde(default = "default_loopback_bind")]
    pub outbound_bind: String,
    /// Metrics listener IP; defaults to loopback.
    #[serde(default = "default_loopback_bind")]
    pub metrics_bind: String,
    /// Admin listener socket address; non-loopback use requires a token.
    #[serde(default = "default_admin_bind")]
    pub admin_bind: String,
    /// Optional bearer token required for authenticated admin operations.
    #[serde(default)]
    pub admin_token: Option<String>,
    /// CIDRs permitted as outbound destinations, parsed by the proxy constructor.
    #[serde(default)]
    pub egress_subnets: Vec<String>,
    /// Maximum lifetime of a multiplexed tunnel in seconds.
    #[serde(default = "default_tunnel_max_age_secs")]
    pub tunnel_max_age_secs: u64,
    /// Maximum concurrent streams on one multiplexed tunnel.
    #[serde(default = "default_mux_max_streams_per_tunnel")]
    pub mux_max_streams_per_tunnel: usize,
}

fn default_loopback_bind() -> String {
    "127.0.0.1".into()
}

fn default_admin_bind() -> String {
    "127.0.0.1:4192".into()
}

fn default_tunnel_max_age_secs() -> u64 {
    3600
}

fn default_mux_max_streams_per_tunnel() -> usize {
    64
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
            default_upstream: None,
            mux: true,
            outbound_bind: default_loopback_bind(),
            metrics_bind: default_loopback_bind(),
            admin_bind: default_admin_bind(),
            admin_token: None,
            egress_subnets: Vec::new(),
            tunnel_max_age_secs: default_tunnel_max_age_secs(),
            mux_max_streams_per_tunnel: default_mux_max_streams_per_tunnel(),
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
        if file_cfg.default_upstream.is_some() {
            self.default_upstream = file_cfg.default_upstream;
        }
        self.mux = file_cfg.mux;
        self.outbound_bind = file_cfg.outbound_bind;
        self.metrics_bind = file_cfg.metrics_bind;
        self.admin_bind = file_cfg.admin_bind;
        if file_cfg.admin_token.is_some() {
            self.admin_token = file_cfg.admin_token;
        }
        self.egress_subnets = file_cfg.egress_subnets;
        self.tunnel_max_age_secs = file_cfg.tunnel_max_age_secs;
        self.mux_max_streams_per_tunnel = file_cfg.mux_max_streams_per_tunnel;

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
        if let Ok(v) = std::env::var("INTERLINK_DEFAULT_UPSTREAM") {
            self.default_upstream = Some(v);
        }
        if let Ok(v) = std::env::var("INTERLINK_MUX") {
            self.mux = v != "false" && v != "0";
        }
        if let Ok(v) = std::env::var("INTERLINK_OUTBOUND_BIND") {
            self.outbound_bind = v;
        }
        if let Ok(v) = std::env::var("INTERLINK_METRICS_BIND") {
            self.metrics_bind = v;
        }
        if let Ok(v) = std::env::var("INTERLINK_ADMIN_BIND") {
            self.admin_bind = v;
        }
        if let Ok(v) = std::env::var("INTERLINK_ADMIN_TOKEN") {
            self.admin_token = Some(v);
        }
        if let Ok(v) = std::env::var("INTERLINK_EGRESS_SUBNETS") {
            self.egress_subnets = v.split(',').map(|entry| entry.trim().to_string()).collect();
        }
        if let Ok(v) = std::env::var("INTERLINK_TUNNEL_MAX_AGE_SECS") {
            self.tunnel_max_age_secs = v.parse().map_err(|e| {
                InterlinkError::Config(format!("INTERLINK_TUNNEL_MAX_AGE_SECS: {e}"))
            })?;
        }
        if let Ok(v) = std::env::var("INTERLINK_MUX_MAX_STREAMS") {
            self.mux_max_streams_per_tunnel = v.parse().map_err(|e| {
                InterlinkError::Config(format!("INTERLINK_MUX_MAX_STREAMS: {e}"))
            })?;
        }
        Ok(self)
    }

    /// Convert the common runtime config into a proxy-specific config.
    pub fn to_proxy_config(&self) -> Result<crate::proxy::config::ProxyConfig, InterlinkError> {
        let outbound_bind = self.outbound_bind.parse::<IpAddr>().map_err(|e| {
            InterlinkError::Config(format!("invalid outbound_bind: {e}"))
        })?;
        Ok(crate::proxy::config::ProxyConfig {
            trust_domain: self.trust_domain.clone(),
            identity: self.identity.clone(),
            default_upstream: self.default_upstream.clone(),
            max_connections: Some(self.max_connections),
            mux: self.mux,
            outbound_bind,
            egress_subnets: self.egress_subnets.clone(),
            tunnel_max_age_secs: self.tunnel_max_age_secs,
            mux_max_streams_per_tunnel: self.mux_max_streams_per_tunnel,
        })
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), InterlinkError> {
        if self.trust_domain.is_empty() {
            return Err(InterlinkError::Config(
                "trust_domain must not be empty".into(),
            ));
        }
        if self
            .trust_domain
            .bytes()
            .any(|byte| !byte.is_ascii() || byte.is_ascii_control())
        {
            return Err(InterlinkError::Config(
                "trust_domain must contain only printable ASCII characters".into(),
            ));
        }
        for (name, bind) in [
            ("outbound_bind", &self.outbound_bind),
            ("metrics_bind", &self.metrics_bind),
        ] {
            bind.parse::<IpAddr>()
                .map_err(|e| InterlinkError::Config(format!("invalid {name}: {e}")))?;
        }
        self.admin_bind
            .parse::<SocketAddr>()
            .map_err(|e| InterlinkError::Config(format!("invalid admin_bind: {e}")))?;
        if self.admin_token.as_ref().is_some_and(|token| token.is_empty()) {
            return Err(InterlinkError::Config("admin_token must not be empty".into()));
        }
        if self.egress_subnets.iter().any(|entry| entry.trim().is_empty()) {
            return Err(InterlinkError::Config("egress_subnets entries must not be empty".into()));
        }
        if self.tunnel_max_age_secs == 0 {
            return Err(InterlinkError::Config("tunnel_max_age_secs must be > 0".into()));
        }
        if self.mux_max_streams_per_tunnel == 0 {
            return Err(InterlinkError::Config("mux_max_streams_per_tunnel must be > 0".into()));
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
        assert_eq!(cfg.outbound_bind, "127.0.0.1");
        assert_eq!(cfg.metrics_bind, "127.0.0.1");
        assert_eq!(cfg.admin_bind, "127.0.0.1:4192");
        assert!(cfg.admin_token.is_none());
        assert!(cfg.egress_subnets.is_empty());
        assert_eq!(cfg.tunnel_max_age_secs, 3600);
        assert_eq!(cfg.mux_max_streams_per_tunnel, 64);
        cfg.validate().unwrap();
    }

    #[test]
    fn test_config_from_env() {
        // Use a non-existent config file path so only env vars are applied.
        std::env::set_var("INTERLINK_CONFIG_FILE", "/nonexistent/interlink.json");
        std::env::set_var("INTERLINK_TRUST_DOMAIN", "env.local");
        std::env::set_var("INTERLINK_MAX_CONNECTIONS", "512");
        std::env::remove_var("INTERLINK_IDENTITY");
        let overrides = [
            ("INTERLINK_OUTBOUND_BIND", "::1"),
            ("INTERLINK_METRICS_BIND", "0.0.0.0"),
            ("INTERLINK_ADMIN_BIND", "[::1]:9192"),
            ("INTERLINK_ADMIN_TOKEN", "secret"),
            ("INTERLINK_EGRESS_SUBNETS", "10.0.0.0/8, ::1/128"),
            ("INTERLINK_TUNNEL_MAX_AGE_SECS", "120"),
            ("INTERLINK_MUX_MAX_STREAMS", "8"),
        ];
        for (key, value) in overrides {
            std::env::set_var(key, value);
        }

        let cfg = Config::load().unwrap();
        assert_eq!(cfg.trust_domain, "env.local");
        assert_eq!(cfg.max_connections, 512);
        assert!(cfg.identity.is_none());
        assert_eq!(cfg.outbound_bind, "::1");
        assert_eq!(cfg.metrics_bind, "0.0.0.0");
        assert_eq!(cfg.admin_bind, "[::1]:9192");
        assert_eq!(cfg.admin_token.as_deref(), Some("secret"));
        assert_eq!(cfg.egress_subnets, ["10.0.0.0/8", "::1/128"]);
        assert_eq!(cfg.tunnel_max_age_secs, 120);
        assert_eq!(cfg.mux_max_streams_per_tunnel, 8);
        for key in ["INTERLINK_TUNNEL_MAX_AGE_SECS", "INTERLINK_MUX_MAX_STREAMS"] {
            let original = std::env::var(key).unwrap();
            std::env::set_var(key, "not-a-number");
            assert!(matches!(Config::default().with_env(), Err(InterlinkError::Config(_))));
            std::env::set_var(key, original);
        }
        for (key, _) in overrides {
            std::env::remove_var(key);
        }

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
    #[test]
    fn config_file_overrides_mux_flag() {
        let path = std::env::temp_dir().join(format!(
            "interlink-config-mux-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let file_cfg = Config {
            mux: false,
            ..Config::default()
        };
        std::fs::write(&path, serde_json::to_vec(&file_cfg).unwrap()).unwrap();

        let loaded = Config::default().with_file(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        assert!(
            !loaded.mux,
            "file-level mux=false must override the default"
        );
    }

    #[test]
    fn config_security_fields_file_overrides_and_serde_defaults() {
        let path = std::env::temp_dir().join(format!(
            "interlink-security-config-{}.json", std::process::id()
        ));
        let file_cfg = Config {
            outbound_bind: "::1".into(),
            metrics_bind: "0.0.0.0".into(),
            admin_bind: "[::1]:9192".into(),
            admin_token: Some("secret".into()),
            egress_subnets: vec!["10.0.0.0/8".into()],
            tunnel_max_age_secs: 120,
            mux_max_streams_per_tunnel: 8,
            ..Config::default()
        };
        std::fs::write(&path, serde_json::to_vec(&file_cfg).unwrap()).unwrap();
        let loaded = Config::default().with_file(&path).unwrap();
        loaded.validate().unwrap();
        assert_eq!(loaded.outbound_bind, "::1");
        assert_eq!(loaded.metrics_bind, "0.0.0.0");
        assert_eq!(loaded.admin_bind, "[::1]:9192");
        assert_eq!(loaded.admin_token.as_deref(), Some("secret"));
        assert_eq!(loaded.egress_subnets, ["10.0.0.0/8"]);
        assert_eq!(loaded.tunnel_max_age_secs, 120);
        assert_eq!(loaded.mux_max_streams_per_tunnel, 8);

        let mut legacy_json = serde_json::to_value(Config::default()).unwrap();
        for key in [
            "outbound_bind", "metrics_bind", "admin_bind", "admin_token",
            "egress_subnets", "tunnel_max_age_secs", "mux_max_streams_per_tunnel",
        ] {
            legacy_json.as_object_mut().unwrap().remove(key);
        }
        std::fs::write(&path, serde_json::to_vec(&legacy_json).unwrap()).unwrap();
        let defaults = Config::default().with_file(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        defaults.validate().unwrap();
        assert_eq!(defaults.outbound_bind, "127.0.0.1");
        assert_eq!(defaults.metrics_bind, "127.0.0.1");
        assert_eq!(defaults.admin_bind, "127.0.0.1:4192");
        assert!(defaults.admin_token.is_none());
        assert!(defaults.egress_subnets.is_empty());
        assert_eq!(defaults.tunnel_max_age_secs, 3600);
        assert_eq!(defaults.mux_max_streams_per_tunnel, 64);
    }

    #[test]
    fn config_rejects_invalid_security_fields() {
        for cfg in [
            Config { outbound_bind: "localhost".into(), ..Config::default() },
            Config { metrics_bind: "127.0.0.1:80".into(), ..Config::default() },
            Config { admin_bind: "127.0.0.1".into(), ..Config::default() },
            Config { admin_token: Some(String::new()), ..Config::default() },
            Config { egress_subnets: vec![" ".into()], ..Config::default() },
            Config { tunnel_max_age_secs: 0, ..Config::default() },
            Config { mux_max_streams_per_tunnel: 0, ..Config::default() },
        ] {
            assert!(matches!(cfg.validate(), Err(InterlinkError::Config(_))));
        }
        for ch in (0u8..=31).chain([127]).map(char::from).chain(['é']) {
            let cfg = Config { trust_domain: format!("trust{ch}.local"), ..Config::default() };
            assert!(matches!(cfg.validate(), Err(InterlinkError::Config(_))));
        }
    }

    #[test]
    fn proxy_conversion_preserves_security_fields_and_missing_identity() {
        let cfg = Config {
            outbound_bind: "::1".into(),
            egress_subnets: vec!["10.0.0.0/8".into()],
            tunnel_max_age_secs: 120,
            mux_max_streams_per_tunnel: 8,
            ..Config::default()
        };
        let proxy = cfg.to_proxy_config().unwrap();
        assert_eq!(proxy.outbound_bind, "::1".parse::<IpAddr>().unwrap());
        assert_eq!(proxy.egress_subnets, cfg.egress_subnets);
        assert_eq!(proxy.tunnel_max_age_secs, 120);
        assert_eq!(proxy.mux_max_streams_per_tunnel, 8);
        assert!(proxy.identity.is_none());
        let invalid = Config { outbound_bind: "bad".into(), ..cfg };
        assert!(matches!(invalid.to_proxy_config(), Err(InterlinkError::Config(_))));
    }
}
