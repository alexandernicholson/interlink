use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use url::Url;

use crate::common::error::InterlinkError;

/// A SPIFFE identity as defined by the SPIFFE standard.
///
/// Format: spiffe://<trust-domain>/ns/<namespace>/sa/<service-account>
///
/// RFC 5280 §4.2.1.6 mandates this be encoded as a uniformResourceIdentifier
/// in the subjectAltName extension of X.509 certificates.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpiffeId {
    pub trust_domain: String,
    pub namespace: String,
    pub service_account: String,
}

impl SpiffeId {
    pub fn new(
        trust_domain: impl Into<String>,
        namespace: impl Into<String>,
        service_account: impl Into<String>,
    ) -> Self {
        let id = Self {
            trust_domain: trust_domain.into(),
            namespace: namespace.into(),
            service_account: service_account.into(),
        };
        id.validate_segments().expect("invalid SPIFFE ID segments");
        id
    }

    fn validate_segments(&self) -> Result<(), InterlinkError> {
        if self.trust_domain.is_empty() {
            return Err(InterlinkError::Identity(
                "trust domain must not be empty".into(),
            ));
        }
        if self.namespace.is_empty() {
            return Err(InterlinkError::Identity(
                "namespace must not be empty".into(),
            ));
        }
        if self.service_account.is_empty() {
            return Err(InterlinkError::Identity(
                "service account must not be empty".into(),
            ));
        }
        Ok(())
    }

    /// Render as URI: spiffe://trust/ns/foo/sa/bar
    /// Used in X.509 SAN extension per RFC 5280 §4.2.1.6:2030
    pub fn to_uri(&self) -> String {
        format!(
            "spiffe://{}/ns/{}/sa/{}",
            self.trust_domain, self.namespace, self.service_account
        )
    }

    /// Parse from a URI string.
    /// MUST be an absolute URI per RFC 5280 §4.2.1.6:2031-2032.
    pub fn from_uri(uri: &str) -> Result<Self, InterlinkError> {
        let parsed =
            Url::parse(uri).map_err(|e| InterlinkError::Identity(format!("invalid URI: {}", e)))?;

        if parsed.scheme() != "spiffe" {
            return Err(InterlinkError::Identity(format!(
                "expected spiffe:// scheme, got {}",
                parsed.scheme()
            )));
        }

        let trust_domain = parsed
            .host_str()
            .ok_or_else(|| InterlinkError::Identity("missing trust domain in SPIFFE URI".into()))?;

        let segments: Vec<&str> = parsed.path().trim_start_matches('/').split('/').collect();

        if segments.len() != 4 || segments[0] != "ns" || segments[2] != "sa" {
            return Err(InterlinkError::Identity(format!(
                "malformed SPIFFE path: expected /ns/<ns>/sa/<sa>, got {}",
                parsed.path()
            )));
        }

        let id = Self {
            trust_domain: trust_domain.to_string(),
            namespace: segments[1].to_string(),
            service_account: segments[3].to_string(),
        };
        id.validate_segments()?;
        Ok(id)
    }

    /// Check if this identity matches a policy pattern (supports wildcards).
    ///
    /// Patterns use the same SPIFFE URI layout but may contain `*` wildcards
    /// in the namespace and/or service account segments. A `*` matches any
    /// sequence of characters within a single path segment.
    ///
    /// Examples:
    /// - `spiffe://trust/ns/*/sa/*` matches any namespace/service.
    /// - `spiffe://trust/ns/default/sa/web*` matches `web`, `web-api`, etc.
    pub fn matches_pattern(&self, pattern: &str) -> bool {
        let parsed = match Url::parse(pattern) {
            Ok(u) => u,
            Err(_) => return false,
        };

        if parsed.scheme() != "spiffe" {
            return false;
        }

        let pat_trust_domain = match parsed.host_str() {
            Some(h) => h,
            None => return false,
        };

        let segments: Vec<&str> = parsed.path().trim_start_matches('/').split('/').collect();
        if segments.len() != 4 || segments[0] != "ns" || segments[2] != "sa" {
            return false;
        }

        self.trust_domain == pat_trust_domain
            && segment_matches(&self.namespace, segments[1])
            && segment_matches(&self.service_account, segments[3])
    }
}

/// Match a segment against a pattern that may contain `*` wildcards.
fn segment_matches(value: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    // Simple glob-style matching: split pattern on `*` and ensure each part
    // appears in order within the value.
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.is_empty() {
        return value.is_empty();
    }

    let mut rest = value;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(idx) => {
                // The first part must start at the beginning of the value.
                if i == 0 && idx != 0 {
                    return false;
                }
                rest = &rest[idx + part.len()..];
            }
            None => return false,
        }
    }

    // If the pattern did not end with `*`, the last matched part must reach
    // the end of the value.
    if !pattern.ends_with('*') && !rest.is_empty() {
        return false;
    }

    true
}

impl fmt::Display for SpiffeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_uri())
    }
}

impl FromStr for SpiffeId {
    type Err = InterlinkError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_uri(s)
    }
}

/// A trust domain corresponds to the trust root of a SPIFFE identity provider.
///
/// All identities in the same trust domain are verified against the same
/// root CA bundle. This maps to the SPIFFE trust domain concept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustDomain {
    pub name: String,
    pub ca_certs: Vec<Vec<u8>>,
}

impl TrustDomain {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ca_certs: Vec::new(),
        }
    }

    pub fn with_ca(mut self, cert_der: Vec<u8>) -> Self {
        self.ca_certs.push(cert_der);
        self
    }
}

/// The identity provider is the source of truth for local identity.
///
/// In Kubernetes mode, identity is derived from the service account token.
/// In Linux mode, identity is derived from cgroup metadata.
/// In Android mode, identity is derived from the application package name.
pub trait IdentityProvider: Send + Sync {
    fn get_identity(&self) -> Result<SpiffeId, InterlinkError>;
    fn get_trust_domain(&self) -> &TrustDomain;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spiffe_id_roundtrip() {
        let id = SpiffeId::new("cluster.local", "default", "payments");
        let uri = id.to_uri();
        assert_eq!(uri, "spiffe://cluster.local/ns/default/sa/payments");

        let parsed = SpiffeId::from_uri(&uri).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn test_spiffe_id_from_uri() {
        let id = SpiffeId::from_uri("spiffe://example.org/ns/foo/sa/bar").unwrap();
        assert_eq!(id.trust_domain, "example.org");
        assert_eq!(id.namespace, "foo");
        assert_eq!(id.service_account, "bar");
    }

    #[test]
    fn test_spiffe_id_missing_scheme() {
        let result = SpiffeId::from_uri("https://example.org/ns/foo/sa/bar");
        assert!(result.is_err());
    }

    #[test]
    fn test_spiffe_id_malformed_path() {
        let result = SpiffeId::from_uri("spiffe://example.org/foo/bar");
        assert!(result.is_err());
    }

    #[test]
    fn test_spiffe_id_empty_segments() {
        assert!(SpiffeId::from_uri("spiffe:///ns/default/sa/web").is_err());
        assert!(SpiffeId::from_uri("spiffe://example.org/ns//sa/web").is_err());
        assert!(SpiffeId::from_uri("spiffe://example.org/ns/default/sa/").is_err());
    }

    #[test]
    fn test_spiffe_id_wildcard_match() {
        let id = SpiffeId::new("trust", "default", "web-api");
        assert!(id.matches_pattern("spiffe://trust/ns/default/sa/web-api"));
        assert!(id.matches_pattern("spiffe://trust/ns/*/sa/*"));
        assert!(id.matches_pattern("spiffe://trust/ns/default/sa/*"));
        assert!(id.matches_pattern("spiffe://trust/ns/default/sa/web*"));
        assert!(id.matches_pattern("spiffe://trust/ns/default/sa/*api"));
        assert!(!id.matches_pattern("spiffe://trust/ns/other/sa/web-api"));
        assert!(!id.matches_pattern("spiffe://other/ns/default/sa/web-api"));
        assert!(!id.matches_pattern("spiffe://trust/ns/default/sa/web"));
    }

    #[test]
    fn test_display() {
        let id = SpiffeId::new("a.com", "b", "c");
        assert_eq!(format!("{}", id), "spiffe://a.com/ns/b/sa/c");
    }
}
