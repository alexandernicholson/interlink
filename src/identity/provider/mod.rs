use std::collections::HashMap;

use crate::common::error::InterlinkError;
use crate::common::identity::{IdentityProvider, SpiffeId, TrustDomain};

const K8S_SERVICE_ACCOUNT_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
const K8S_NAMESPACE_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

/// Kubernetes-based identity provider.
///
/// Derives the SPIFFE identity from the projected service account token
/// mounted at /var/run/secrets/kubernetes.io/serviceaccount/token.
/// Falls back to environment variables for non-Kubernetes environments.
pub struct KubernetesIdentityProvider {
    trust_domain: TrustDomain,
    identity: SpiffeId,
}

impl KubernetesIdentityProvider {
    pub fn new(trust_domain: impl Into<String>) -> Result<Self, InterlinkError> {
        let td = trust_domain.into();
        let identity = Self::derive_identity(&td)?;

        Ok(Self {
            trust_domain: TrustDomain::new(td),
            identity,
        })
    }

    /// Derive the SPIFFE identity from Kubernetes metadata.
    ///
    /// 1. If `INTERLINK_IDENTITY` is set and valid, use it.
    /// 2. Read namespace from the service account namespace file.
    /// 3. Extract service account name from the JWT `sub` claim
    ///    (`system:serviceaccount:<ns>:<sa>`).
    /// 4. Fall back to environment variables for local development.
    fn derive_identity(trust_domain: &str) -> Result<SpiffeId, InterlinkError> {
        if let Ok(id) = std::env::var("INTERLINK_IDENTITY") {
            if let Ok(id) = SpiffeId::from_uri(&id) {
                return Ok(id);
            }
        }

        let namespace = Self::read_namespace()?;
        let service_account = Self::read_service_account()?;

        SpiffeId::try_new(trust_domain, namespace, service_account)
    }

    fn read_namespace() -> Result<String, InterlinkError> {
        // Prefer the namespace file when running inside Kubernetes.
        if let Ok(ns) = std::fs::read_to_string(K8S_NAMESPACE_PATH) {
            let ns = ns.trim();
            if !ns.is_empty() {
                return Ok(ns.to_string());
            }
        }

        // Fall back to the standard downward-API env var.
        std::env::var("KUBERNETES_NAMESPACE")
            .or_else(|_| std::env::var("POD_NAMESPACE"))
            .map_err(|_| {
                InterlinkError::Identity("could not determine Kubernetes namespace".into())
            })
    }

    fn read_service_account() -> Result<String, InterlinkError> {
        if let Ok(token) = std::fs::read_to_string(K8S_SERVICE_ACCOUNT_TOKEN_PATH) {
            let token = token.trim();
            if let Some(sa) = Self::extract_service_account_from_token(token) {
                return Ok(sa);
            }
        }

        std::env::var("KUBERNETES_SERVICE_ACCOUNT_NAME").map_err(|_| {
            InterlinkError::Identity("could not determine Kubernetes service account".into())
        })
    }

    /// Extract the service account name from a JWT `sub` claim.
    ///
    /// Kubernetes service account tokens have a subject of the form
    /// `system:serviceaccount:<namespace>:<service-account>`.
    fn extract_service_account_from_token(token: &str) -> Option<String> {
        let payload_b64 = token.split('.').nth(1)?;
        let payload_json = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            payload_b64,
        )
        .ok()?;
        let claims: HashMap<String, serde_json::Value> =
            serde_json::from_slice(&payload_json).ok()?;
        let sub = claims.get("sub")?.as_str()?;
        let parts: Vec<&str> = sub.split(':').collect();
        if parts.len() == 4 && parts[0] == "system" && parts[1] == "serviceaccount" {
            Some(parts[3].to_string())
        } else {
            None
        }
    }
}

impl IdentityProvider for KubernetesIdentityProvider {
    fn get_identity(&self) -> Result<SpiffeId, InterlinkError> {
        Ok(self.identity.clone())
    }

    fn get_trust_domain(&self) -> &TrustDomain {
        &self.trust_domain
    }
}

/// A static identity provider with a fixed SPIFFE ID and trust domain.
///
/// Useful for file/certificate-based deployments where the identity is
/// supplied by configuration rather than derived from the environment.
#[derive(Debug, Clone)]
pub struct StaticIdentityProvider {
    identity: SpiffeId,
    trust_domain: TrustDomain,
}

impl StaticIdentityProvider {
    pub fn new(identity: SpiffeId, trust_domain: TrustDomain) -> Self {
        Self {
            identity,
            trust_domain,
        }
    }

    pub fn with_ca_bundle(identity: SpiffeId, ca_bundle: Vec<Vec<u8>>) -> Self {
        let mut trust_domain = TrustDomain::new(identity.trust_domain.clone());
        for cert in ca_bundle {
            trust_domain.ca_certs.push(cert);
        }
        Self {
            identity,
            trust_domain,
        }
    }
}

impl IdentityProvider for StaticIdentityProvider {
    fn get_identity(&self) -> Result<SpiffeId, InterlinkError> {
        Ok(self.identity.clone())
    }

    fn get_trust_domain(&self) -> &TrustDomain {
        &self.trust_domain
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::*;

    #[test]
    fn test_extract_service_account_from_token() {
        // Build a JWT with payload { "sub": "system:serviceaccount:default:my-sa" }.
        let payload = r#"{"sub":"system:serviceaccount:default:my-sa"}"#;
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        let token = format!("header.{}.signature", payload_b64);

        let sa = KubernetesIdentityProvider::extract_service_account_from_token(&token).unwrap();
        assert_eq!(sa, "my-sa");
    }

    #[test]
    fn test_extract_service_account_malformed_sub() {
        let payload = r#"{"sub":"not-a-serviceaccount-sub"}"#;
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        let token = format!("header.{}.signature", payload_b64);

        assert!(KubernetesIdentityProvider::extract_service_account_from_token(&token).is_none());
    }
}
