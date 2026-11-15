use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use crate::common::error::InterlinkError;

/// A pre-compiled glob pattern for a single SPIFFE path segment (namespace or
/// service account). Eliminates `split('*')` per match evaluation.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct SegmentGlob {
    pattern: Box<str>,
    kind: SegmentGlobKind,
}

impl SegmentGlob {
    pub fn new(pattern: &str) -> Self {
        let wildcard_count = pattern.bytes().filter(|&byte| byte == b'*').count();
        if wildcard_count == 0 {
            return Self {
                pattern: pattern.into(),
                kind: SegmentGlobKind::Exact,
            };
        }

        let mut parts = Vec::with_capacity(wildcard_count.saturating_add(1));
        let mut start = 0;
        for (index, _) in pattern.match_indices('*') {
            if start != index {
                parts.push((start, index));
            }
            start = index + 1;
        }
        if start != pattern.len() {
            parts.push((start, pattern.len()));
        }

        let anchored_start = !pattern.starts_with('*');
        let anchored_end = !pattern.ends_with('*');
        let kind = match parts.as_slice() {
            [] => SegmentGlobKind::Any,
            &[(_, end)] if anchored_start => SegmentGlobKind::Prefix { end },
            &[(start, _)] if anchored_end => SegmentGlobKind::Suffix { start },
            &[(start, end)] => SegmentGlobKind::Contains { start, end },
            &[(_, prefix_end), (suffix_start, _)] if anchored_start && anchored_end => {
                SegmentGlobKind::PrefixSuffix {
                    prefix_end,
                    suffix_start,
                }
            }
            _ => SegmentGlobKind::Multi {
                parts: parts.into_boxed_slice(),
                anchored_start,
                anchored_end,
            },
        };

        Self {
            pattern: pattern.into(),
            kind,
        }
    }

    pub fn matches(&self, value: &str) -> bool {
        match &self.kind {
            SegmentGlobKind::Any => true,
            SegmentGlobKind::Exact => value == self.pattern.as_ref(),
            SegmentGlobKind::Prefix { end } => value.starts_with(&self.pattern[..*end]),
            SegmentGlobKind::Suffix { start } => value.ends_with(&self.pattern[*start..]),
            SegmentGlobKind::Contains { start, end } => value.contains(&self.pattern[*start..*end]),
            SegmentGlobKind::PrefixSuffix {
                prefix_end,
                suffix_start,
            } => {
                let prefix = &self.pattern[..*prefix_end];
                let suffix = &self.pattern[*suffix_start..];
                value.len() >= prefix.len() + suffix.len()
                    && value.starts_with(prefix)
                    && value.ends_with(suffix)
            }
            SegmentGlobKind::Multi {
                parts,
                anchored_start,
                anchored_end,
            } => self.matches_multi(value, parts, *anchored_start, *anchored_end),
        }
    }

    fn matches_multi(
        &self,
        value: &str,
        parts: &[(usize, usize)],
        anchored_start: bool,
        anchored_end: bool,
    ) -> bool {
        let mut first = 0;
        let mut last = parts.len();
        let mut search_start = 0;
        let mut search_end = value.len();

        if anchored_start {
            let (start, end) = parts[0];
            let prefix = &self.pattern[start..end];
            if !value.starts_with(prefix) {
                return false;
            }
            search_start = prefix.len();
            first += 1;
        }

        if anchored_end {
            let (start, end) = parts[last - 1];
            let suffix = &self.pattern[start..end];
            if !value.ends_with(suffix) {
                return false;
            }
            search_end = value.len() - suffix.len();
            last -= 1;
        }

        if search_start > search_end {
            return false;
        }

        for &(start, end) in &parts[first..last] {
            let literal = &self.pattern[start..end];
            let Some(offset) = value[search_start..search_end].find(literal) else {
                return false;
            };
            search_start += offset + literal.len();
        }
        true
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
enum SegmentGlobKind {
    Any,
    Exact,
    Prefix {
        end: usize,
    },
    Suffix {
        start: usize,
    },
    Contains {
        start: usize,
        end: usize,
    },
    PrefixSuffix {
        prefix_end: usize,
        suffix_start: usize,
    },
    Multi {
        parts: Box<[(usize, usize)]>,
        anchored_start: bool,
        anchored_end: bool,
    },
}

struct SpiffeUriParts<'a> {
    trust_domain: &'a str,
    namespace: &'a str,
    service_account: &'a str,
}

fn parse_spiffe_uri<'a>(
    uri: &'a str,
    context: &'static str,
) -> Result<SpiffeUriParts<'a>, InterlinkError> {
    const PREFIX: &[u8] = b"spiffe://";

    let Some(prefix) = uri.as_bytes().get(..PREFIX.len()) else {
        return Err(InterlinkError::Identity(format!(
            "invalid {context}: missing URI scheme separator"
        )));
    };
    if !prefix.eq_ignore_ascii_case(PREFIX) {
        let scheme = uri.split_once("://").map_or(uri, |(scheme, _)| scheme);
        return Err(InterlinkError::Identity(format!(
            "expected spiffe:// scheme, got {scheme}"
        )));
    }

    // `PREFIX` is ASCII, so a matching prefix guarantees this byte offset is
    // also a UTF-8 boundary.
    let remainder = &uri[PREFIX.len()..];
    let Some(authority_end) = remainder.as_bytes().iter().position(|&byte| byte == b'/') else {
        return Err(InterlinkError::Identity(
            "malformed SPIFFE path: expected /ns/<ns>/sa/<sa>, got empty path".into(),
        ));
    };
    let trust_domain = &remainder[..authority_end];
    if trust_domain.is_empty() {
        return Err(InterlinkError::Identity(
            "missing trust domain in SPIFFE URI".into(),
        ));
    }
    if trust_domain.bytes().any(|byte| {
        byte.is_ascii_control()
            || byte.is_ascii_whitespace()
            || matches!(byte, b'@' | b':' | b'[' | b']' | b'\\' | b'?' | b'#')
    }) {
        return Err(InterlinkError::Identity(
            "SPIFFE URI authority must contain only a trust domain".into(),
        ));
    }

    let path = &remainder[authority_end + 1..];
    let Some(path) = path.strip_prefix("ns/") else {
        return Err(InterlinkError::Identity(format!(
            "malformed SPIFFE path: expected /ns/<ns>/sa/<sa>, got /{path}"
        )));
    };
    let Some(namespace_end) = path
        .as_bytes()
        .iter()
        .position(|&byte| matches!(byte, b'/' | b'?' | b'#'))
    else {
        return Err(InterlinkError::Identity(format!(
            "malformed SPIFFE path: expected /ns/<ns>/sa/<sa>, got /ns/{path}"
        )));
    };
    if path.as_bytes()[namespace_end] != b'/' {
        return Err(InterlinkError::Identity(
            "SPIFFE URI must not contain a query or fragment".into(),
        ));
    }

    let namespace = &path[..namespace_end];
    let Some(service_account) = path[namespace_end + 1..].strip_prefix("sa/") else {
        return Err(InterlinkError::Identity(format!(
            "malformed SPIFFE path: expected /ns/<ns>/sa/<sa>, got /ns/{path}"
        )));
    };
    if namespace.is_empty()
        || service_account.is_empty()
        || service_account
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'?' | b'#'))
    {
        return Err(InterlinkError::Identity(format!(
            "malformed SPIFFE path: expected /ns/<ns>/sa/<sa>, got /ns/{path}"
        )));
    }

    Ok(SpiffeUriParts {
        trust_domain,
        namespace,
        service_account,
    })
}

/// A pre-compiled policy pattern that avoids `Url::parse` on every evaluation.
///
/// Parsed once at rule-insertion time.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct CompiledPattern {
    pub trust_domain: String,
    pub namespace: SegmentGlob,
    pub service_account: SegmentGlob,
}

impl CompiledPattern {
    pub fn from_uri(uri: &str) -> Result<Self, InterlinkError> {
        let parts = parse_spiffe_uri(uri, "pattern URI")?;
        Ok(Self {
            trust_domain: parts.trust_domain.to_ascii_lowercase(),
            namespace: SegmentGlob::new(parts.namespace),
            service_account: SegmentGlob::new(parts.service_account),
        })
    }

    /// Match any SPIFFE ID (wildcard trust domain, namespace, and service account).
    pub fn any() -> Self {
        Self {
            trust_domain: String::new(),
            namespace: SegmentGlob::new("*"),
            service_account: SegmentGlob::new("*"),
        }
    }
}

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
    /// Validate and construct a SPIFFE ID, including compile-time-known inputs.
    pub(crate) fn new(
        trust_domain: impl Into<String>,
        namespace: impl Into<String>,
        service_account: impl Into<String>,
    ) -> Result<Self, InterlinkError> {
        let mut id = Self {
            trust_domain: trust_domain.into(),
            namespace: namespace.into(),
            service_account: service_account.into(),
        };
        id.validate_segments()?;
        id.trust_domain.make_ascii_lowercase();
        Ok(id)
    }

    /// Validate and construct — use for config/runtime data.
    pub fn try_new(
        trust_domain: impl Into<String>,
        namespace: impl Into<String>,
        service_account: impl Into<String>,
    ) -> Result<Self, InterlinkError> {
        Self::new(trust_domain, namespace, service_account)
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
        for (name, segment) in [
            ("trust domain", &self.trust_domain),
            ("namespace", &self.namespace),
            ("service account", &self.service_account),
        ] {
            if segment
                .bytes()
                .any(|byte| !byte.is_ascii() || byte.is_ascii_control())
            {
                return Err(InterlinkError::Identity(format!(
                    "{name} must contain only printable ASCII characters"
                )));
            }
        }
        Ok(())
    }

    /// Render as URI: spiffe://trust/ns/foo/sa/bar
    /// Used in X.509 SAN extension per RFC 5280 §4.2.1.6:2030
    pub fn to_uri(&self) -> String {
        let mut uri = String::with_capacity(
            17 + self.trust_domain.len() + self.namespace.len() + self.service_account.len(),
        );
        uri.push_str("spiffe://");
        uri.push_str(&self.trust_domain);
        uri.push_str("/ns/");
        uri.push_str(&self.namespace);
        uri.push_str("/sa/");
        uri.push_str(&self.service_account);
        uri
    }

    /// Parse from a URI string.
    /// MUST be an absolute URI per RFC 5280 §4.2.1.6:2031-2032.
    pub fn from_uri(uri: &str) -> Result<Self, InterlinkError> {
        let parts = parse_spiffe_uri(uri, "URI")?;
        Self::try_new(
            parts.trust_domain,
            parts.namespace,
            parts.service_account,
        )
    }

    /// Borrow a log-safe display wrapper without allocating.
    pub fn sanitized(&self) -> SanitizedSpiffeId<'_> {
        SanitizedSpiffeId(self)
    }

    /// Check if this identity matches a policy pattern (supports wildcards).
    ///
    /// Patterns use the same SPIFFE URI layout but may contain `*` wildcards
    /// in the namespace and/or service account segments.
    pub fn matches_pattern(&self, pattern: &str) -> bool {
        match CompiledPattern::from_uri(pattern) {
            Ok(cp) => self.matches_compiled(&cp),
            Err(_) => false,
        }
    }

    /// Match against a pre-compiled pattern (avoids `Url::parse` overhead).
    pub fn matches_compiled(&self, pattern: &CompiledPattern) -> bool {
        (pattern.trust_domain.is_empty() || self.trust_domain == pattern.trust_domain)
            && pattern.namespace.matches(&self.namespace)
            && pattern.service_account.matches(&self.service_account)
    }
}

impl fmt::Display for SpiffeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("spiffe://")?;
        f.write_str(&self.trust_domain)?;
        f.write_str("/ns/")?;
        f.write_str(&self.namespace)?;
        f.write_str("/sa/")?;
        f.write_str(&self.service_account)
    }
}

/// Defense-in-depth display for identities from mutable or deserialized data.
pub struct SanitizedSpiffeId<'a>(&'a SpiffeId);

impl fmt::Display for SanitizedSpiffeId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn segment(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
            let mut start = 0;
            for (offset, ch) in value.char_indices() {
                if ch.is_control() {
                    f.write_str(&value[start..offset])?;
                    f.write_str("\u{fffd}")?;
                    start = offset + ch.len_utf8();
                }
            }
            f.write_str(&value[start..])
        }

        f.write_str("spiffe://")?;
        segment(f, &self.0.trust_domain)?;
        f.write_str("/ns/")?;
        segment(f, &self.0.namespace)?;
        f.write_str("/sa/")?;
        segment(f, &self.0.service_account)
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
        let id = SpiffeId::new("cluster.local", "default", "payments").unwrap();
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
        let id = SpiffeId::new("trust", "default", "web-api").unwrap();
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
    fn segment_glob_covers_all_compiled_shapes() {
        let cases = [
            ("", "", true),
            ("", "anything", false),
            ("*", "anything", true),
            ("exact", "exact", true),
            ("exact", "other", false),
            ("pre*", "prefix", true),
            ("pre*", "other", false),
            ("*suffix", "suffix-suffix", true),
            ("*suffix", "suffix-other", false),
            ("*middle*", "a-middle-z", true),
            ("*middle*", "absent", false),
            ("pre*suffix", "pre-middle-suffix", true),
            ("pre*suffix", "presuffix", true),
            ("pre*suffix", "prefix", false),
            ("a*b*c", "a-1-b-2-c", true),
            ("a*b*c", "a-1-c-2-b", false),
            ("*a*b*", "z-a-1-b-z", true),
            ("*a*b*", "z-b-1-a-z", false),
            ("a**b", "a-middle-b", true),
            ("**", "anything", true),
        ];

        for (pattern, value, expected) in cases {
            assert_eq!(
                SegmentGlob::new(pattern).matches(value),
                expected,
                "pattern {pattern:?}, value {value:?}"
            );
        }
    }

    #[test]
    fn spiffe_uri_rejects_non_identity_components() {
        for uri in [
            "spiffe://user@example.org/ns/foo/sa/bar",
            "spiffe://example.org:443/ns/foo/sa/bar",
            "spiffe://example.org/ns/foo/sa/bar?query",
            "spiffe://example.org/ns/foo/sa/bar#fragment",
            "spiffe://example.org/ns/foo/sa/bar/extra",
        ] {
            assert!(SpiffeId::from_uri(uri).is_err(), "{uri} must be rejected");
            assert!(
                CompiledPattern::from_uri(uri).is_err(),
                "{uri} must be rejected as a pattern"
            );
        }
    }

    #[test]
    fn spiffe_uri_normalizes_scheme_and_trust_domain_case() {
        let id = SpiffeId::from_uri("SPIFFE://EXAMPLE.ORG/ns/foo/sa/bar").unwrap();
        assert_eq!(id.trust_domain, "example.org");
        assert_eq!(id.to_uri(), "spiffe://example.org/ns/foo/sa/bar");
    }

    #[test]
    fn test_display() {
        let id = SpiffeId::new("a.com", "b", "c").unwrap();
        assert_eq!(format!("{}", id), "spiffe://a.com/ns/b/sa/c");
    }

    #[test]
    fn all_constructors_reject_controls_and_non_ascii_in_every_segment() {
        for ch in (0u8..=31).chain([127]).map(char::from).chain(['é', '界']) {
            for segment in 0..3 {
                let mut parts = ["trust.local".to_string(), "default".into(), "web".into()];
                parts[segment].push(ch);
                assert!(SpiffeId::new(&parts[0], &parts[1], &parts[2]).is_err());
                assert!(SpiffeId::try_new(&parts[0], &parts[1], &parts[2]).is_err());
                let uri = format!("spiffe://{}/ns/{}/sa/{}", parts[0], parts[1], parts[2]);
                assert!(SpiffeId::from_uri(&uri).is_err(), "{uri:?}");
            }
        }
        assert!(SpiffeId::try_new("trust.local", "default", "web-1").is_ok());
    }

    #[test]
    fn all_constructors_canonicalize_trust_domain_and_roundtrip() {
        for id in [
            SpiffeId::new("TRUST.Local", "Default", "Web").unwrap(),
            SpiffeId::try_new("TRUST.Local", "Default", "Web").unwrap(),
            SpiffeId::from_uri("spiffe://TRUST.Local/ns/Default/sa/Web").unwrap(),
        ] {
            assert_eq!(id.trust_domain, "trust.local");
            assert_eq!(id.namespace, "Default");
            assert_eq!(id.service_account, "Web");
            assert_eq!(id.to_uri(), "spiffe://trust.local/ns/Default/sa/Web");
            assert_eq!(SpiffeId::from_uri(&id.to_uri()).unwrap(), id);
        }
    }

    #[test]
    fn sanitized_display_replaces_controls_in_all_segments() {
        // Public fields/deserialization can bypass constructors; logs still escape controls.
        let id = SpiffeId {
            trust_domain: "trust\n.local".into(),
            namespace: "de\u{7f}fault".into(),
            service_account: "\0web\r\u{85}".into(),
        };
        assert_eq!(
            id.sanitized().to_string(),
            "spiffe://trust\u{fffd}.local/ns/de\u{fffd}fault/sa/\u{fffd}web\u{fffd}\u{fffd}"
        );
        let valid = SpiffeId::try_new("trust.local", "default", "web").unwrap();
        assert_eq!(valid.sanitized().to_string(), valid.to_uri());
    }
}
