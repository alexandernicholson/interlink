use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use rustls::pki_types::CertificateDer;
use time::OffsetDateTime;

use crate::common::constants::cert;
use crate::common::error::InterlinkError;
use crate::common::identity::SpiffeId;

/// RFC 5280 recommends serial numbers be at least 20 octets (160 bits)
/// to minimize collision probability.
const SERIAL_NUMBER_BYTES: usize = 20;

/// A Certificate Authority producing short-lived X.509v3 certificates
/// with SPIFFE identities in the Subject Alternative Name (RFC 5280 §4.2.1.6).
///
/// Per ADR-0002, this CA does not implement CRL or OCSP revocation.
/// Certificates are issued with a 24-hour TTL; expiry is the revocation
/// mechanism. A compromised key has a maximum exposure window of one leaf
/// TTL.
pub struct CertificateAuthority {
    root_key: KeyPair,
    root_cert: Certificate,
    root_cert_der: Vec<u8>,
    _trust_domain: String,
}

impl CertificateAuthority {
    /// Generate a new Ed25519 root key and self-signed CA certificate.
    pub fn new(trust_domain: impl Into<String>) -> Result<Self, InterlinkError> {
        let td = trust_domain.into();

        // Generate Ed25519 key pair via rcgen
        let root_key = KeyPair::generate_for(&rcgen::PKCS_ED25519)
            .map_err(|e| InterlinkError::Identity(format!("root keygen: {}", e)))?;

        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| InterlinkError::Identity(format!("params: {}", e)))?;
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "interlink CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, td.as_str());
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];

        let now = OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + time::Duration::seconds(cert::ROOT_CA_TTL.as_secs() as i64);

        let root_cert = params
            .self_signed(&root_key)
            .map_err(|e| InterlinkError::Identity(format!("self-sign: {}", e)))?;

        Ok(Self {
            root_key,
            root_cert_der: root_cert.der().to_vec(),
            root_cert,
            _trust_domain: td,
        })
    }

    /// Issue a leaf certificate with the given SPIFFE identity.
    pub fn issue_leaf(
        &self,
        identity: &SpiffeId,
    ) -> Result<CertificateDer<'static>, InterlinkError> {
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| InterlinkError::Identity(format!("params: {}", e)))?;
        let uri_str: rcgen::Ia5String = identity.to_uri().as_str().try_into().unwrap();
        params.subject_alt_names = vec![SanType::URI(uri_str)];

        // RFC 5280 §4.2.1.6: subject empty, identity in SAN
        params.distinguished_name = DistinguishedName::new();
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.is_ca = IsCa::ExplicitNoCa;

        let now = OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + time::Duration::seconds(cert::LEAF_TTL.as_secs() as i64);

        params.serial_number = Some(generate_serial_number()?);

        let leaf_key = KeyPair::generate_for(&rcgen::PKCS_ED25519)
            .map_err(|e| InterlinkError::Identity(format!("leaf keygen: {}", e)))?;

        let cert = params
            .signed_by(&leaf_key, &self.root_cert, &self.root_key)
            .map_err(|e| InterlinkError::Identity(format!("sign: {}", e)))?;

        Ok(CertificateDer::from(cert.der().to_vec()))
    }

    /// Issue a leaf certificate with SPIFFE ID and optional subject names,
    /// returning both the DER cert and the PKCS#8 private key.
    ///
    /// Entries in `dns_names` that parse as IP addresses become IP SANs
    /// (C8: an IP `ServerName` never matches a DNS SAN, and mesh peers dial
    /// each other by `ip:port` recovered from SO_ORIGINAL_DST — proxy leaf
    /// certs therefore need IP SANs for the addresses they serve on).
    pub fn issue_leaf_with_key(
        &self,
        identity: &SpiffeId,
        dns_names: &[&str],
    ) -> Result<(CertificateDer<'static>, Vec<u8>), InterlinkError> {
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| InterlinkError::Identity(format!("params: {}", e)))?;
        let uri_str: rcgen::Ia5String = identity
            .to_uri()
            .as_str()
            .try_into()
            .map_err(|e| InterlinkError::Identity(format!("SPIFFE URI SAN: {:?}", e)))?;
        let mut sans: Vec<SanType> = vec![SanType::URI(uri_str)];
        for name in dns_names {
            if let Ok(ip) = name.parse::<std::net::IpAddr>() {
                sans.push(SanType::IpAddress(ip));
            } else {
                let dns_str: rcgen::Ia5String = (*name)
                    .try_into()
                    .map_err(|e| InterlinkError::Identity(format!("DNS SAN: {:?}", e)))?;
                sans.push(SanType::DnsName(dns_str));
            }
        }
        params.subject_alt_names = sans;
        params.distinguished_name = DistinguishedName::new();
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.is_ca = IsCa::ExplicitNoCa;

        let now = OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + time::Duration::seconds(cert::LEAF_TTL.as_secs() as i64);

        params.serial_number = Some(generate_serial_number()?);

        let leaf_key = KeyPair::generate_for(&rcgen::PKCS_ED25519)
            .map_err(|e| InterlinkError::Identity(format!("leaf keygen: {}", e)))?;

        let cert = params
            .signed_by(&leaf_key, &self.root_cert, &self.root_key)
            .map_err(|e| InterlinkError::Identity(format!("sign: {}", e)))?;

        let key_der = leaf_key.serialize_der();
        Ok((CertificateDer::from(cert.der().to_vec()), key_der))
    }

    pub fn root_cert_der(&self) -> &[u8] {
        &self.root_cert_der
    }

    pub fn root_key(&self) -> &KeyPair {
        &self.root_key
    }
}

/// Generate a random 160-bit serial number per RFC 5280 recommendations.
fn generate_serial_number() -> Result<SerialNumber, InterlinkError> {
    let serial_bytes: [u8; SERIAL_NUMBER_BYTES] =
        ring::rand::generate(&ring::rand::SystemRandom::new())
            .map_err(|e| InterlinkError::Identity(format!("rng: {}", e)))?
            .expose();
    Ok(SerialNumber::from_slice(&serial_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::extensions::{GeneralName, ParsedExtension};
    use x509_parser::parse_x509_certificate;

    #[test]
    fn test_debug_leaf() {
        let ca = CertificateAuthority::new("td.local").unwrap();
        let id = SpiffeId::new("td.local", "ns", "sa");
        let cert_der = ca.issue_leaf(&id).unwrap();
        let parsed = parse_x509_certificate(&cert_der).unwrap().1;
        eprintln!("subject: {:?}", parsed.subject());
        eprintln!("issuer: {:?}", parsed.issuer());
        eprintln!("extensions ({}):", parsed.extensions().len());
        for ext in parsed.extensions().iter() {
            eprintln!("  oid={} critical={}", ext.oid, ext.critical);
        }
        let mut found = false;
        for ext in parsed.extensions().iter() {
            if format!("{}", ext.oid) == "2.5.29.17" {
                if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
                    for gn in san.general_names.iter() {
                        eprintln!("  SAN entry: {:?}", gn);
                        if let GeneralName::URI(uri) = gn {
                            eprintln!("  URI: {}", uri);
                            assert_eq!(*uri, "spiffe://td.local/ns/ns/sa/sa");
                            found = true;
                        }
                    }
                }
            }
        }
        assert!(found, "SPIFFE ID not found in SAN");
    }

    #[test]
    fn test_ca_init() {
        let ca = CertificateAuthority::new("td.local").unwrap();
        assert!(!ca.root_cert_der().is_empty());
    }

    #[test]
    fn test_issue_leaf() {
        let ca = CertificateAuthority::new("td.local").unwrap();
        let id = SpiffeId::new("td.local", "ns", "sa");
        let cert = ca.issue_leaf(&id).unwrap();
        assert!(cert.len() > 100);
        let parsed = parse_x509_certificate(cert.as_ref()).unwrap().1;
        assert_eq!(parsed.version().0, 2);
    }

    #[test]
    fn test_leaf_san() {
        let ca = CertificateAuthority::new("td.local").unwrap();
        let id = SpiffeId::new("td.local", "ns", "sa");
        let cert_der = ca.issue_leaf(&id).unwrap();
        let parsed = parse_x509_certificate(&cert_der).unwrap().1;

        let mut found = false;
        for ext in parsed.extensions().iter() {
            if format!("{}", ext.oid) == "2.5.29.17" {
                if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
                    for gn in san.general_names.iter() {
                        if let GeneralName::URI(uri) = gn {
                            assert_eq!(*uri, "spiffe://td.local/ns/ns/sa/sa");
                            found = true;
                        }
                    }
                }
            }
        }
        assert!(found, "SPIFFE ID in SAN");
    }

    #[test]
    fn test_leaf_extensions() {
        let ca = CertificateAuthority::new("td.local").unwrap();
        let id = SpiffeId::new("td.local", "ns", "sa");
        let cert_der = ca.issue_leaf(&id).unwrap();
        let parsed = parse_x509_certificate(&cert_der).unwrap().1;

        let oids: Vec<String> = parsed
            .extensions()
            .iter()
            .map(|e| format!("{}", e.oid))
            .collect();
        assert!(oids.contains(&"2.5.29.17".into()));
        assert!(oids.contains(&"2.5.29.15".into()));
        assert!(oids.contains(&"2.5.29.37".into()));
        assert!(oids.contains(&"2.5.29.19".into()));
    }

    #[test]
    fn test_root_is_self_signed() {
        let ca = CertificateAuthority::new("td.local").unwrap();
        let parsed = parse_x509_certificate(ca.root_cert_der()).unwrap().1;
        assert_eq!(
            format!("{}", parsed.issuer()),
            format!("{}", parsed.subject())
        );
    }

    #[test]
    fn test_leaf_ttl_24h() {
        let ca = CertificateAuthority::new("td.local").unwrap();
        let id = SpiffeId::new("td.local", "ns", "sa");
        let cert_der = ca.issue_leaf(&id).unwrap();
        let parsed = parse_x509_certificate(&cert_der).unwrap().1;
        let ttl =
            parsed.validity().not_after.timestamp() - parsed.validity().not_before.timestamp();
        let expected = cert::LEAF_TTL.as_secs() as i64;
        assert!(
            (expected - 3600..=expected + 3600).contains(&ttl),
            "TTL ~{}s: {}",
            expected,
            ttl
        );
    }
}
