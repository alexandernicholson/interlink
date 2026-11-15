//! Shared CA bootstrap for the interlink example services.
//!
//! Generates a root CA, issues server + client certificates,
//! and writes them into an owner-only directory for both services to load.
//!
//! Run first: `cargo run --example ca_bootstrap`

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use interlink::common::identity::SpiffeId;
use interlink_test_support::CertificateAuthority;

const TRUST_DOMAIN: &str = "example.local";

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    // Refuse existing paths (including symlinks) instead of trusting a shared
    // temp-directory entry or briefly creating publicly accessible material.
    fs::DirBuilder::new().mode(0o700).create(path)
}

fn write_private_key(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

fn main() {
    let out = std::env::temp_dir().join("interlink-demo");
    create_private_directory(&out)
        .expect("create private demo dir (remove old demo material first)");

    // Generate CA
    let ca = CertificateAuthority::new(TRUST_DOMAIN).expect("CA init");
    fs::write(out.join("ca.der"), ca.root_cert_der()).expect("write CA");

    // Issue server cert (for the backend service)
    let server_id = SpiffeId::try_new(TRUST_DOMAIN, "default", "backend").expect("valid SPIFFE ID");
    let (server_cert, server_key) = ca
        .issue_leaf_with_key(
            &server_id,
            &["localhost", "127.0.0.1", "backend.example.local"],
        )
        .expect("issue server cert");
    fs::write(out.join("server.der"), &server_cert).expect("write server cert");
    write_private_key(&out.join("server.key"), &server_key).expect("write server key");

    // Issue client cert (for the frontend service)
    let client_id =
        SpiffeId::try_new(TRUST_DOMAIN, "default", "frontend").expect("valid SPIFFE ID");
    let (client_cert, client_key) = ca
        .issue_leaf_with_key(
            &client_id,
            &["localhost", "127.0.0.1", "frontend.example.local"],
        )
        .expect("issue client cert");
    fs::write(out.join("client.der"), &client_cert).expect("write client cert");
    write_private_key(&out.join("client.key"), &client_key).expect("write client key");

    // Also emit PEM versions for tools like curl and fortio that expect PEM.
    fn der_to_pem(label: &str, der: &[u8]) -> String {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use std::fmt::Write;
        let b64 = STANDARD.encode(der);
        let mut pem = String::new();
        writeln!(pem, "-----BEGIN {}-----", label).unwrap();
        for chunk in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(chunk).unwrap());
            pem.push('\n');
        }
        writeln!(pem, "-----END {}-----", label).unwrap();
        pem
    }
    fs::write(
        out.join("ca.pem"),
        der_to_pem("CERTIFICATE", ca.root_cert_der()),
    )
    .expect("write CA PEM");
    fs::write(
        out.join("server.pem"),
        der_to_pem("CERTIFICATE", &server_cert),
    )
    .expect("write server PEM");
    write_private_key(
        &out.join("server-key.pem"),
        der_to_pem("PRIVATE KEY", &server_key).as_bytes(),
    )
    .expect("write server key PEM");
    fs::write(
        out.join("client.pem"),
        der_to_pem("CERTIFICATE", &client_cert),
    )
    .expect("write client PEM");
    write_private_key(
        &out.join("client-key.pem"),
        der_to_pem("PRIVATE KEY", &client_key).as_bytes(),
    )
    .expect("write client key PEM");

    println!("{}", out.display());
    println!("CA + certs written to: {:?}", out);
    println!("  ca.der       — root CA certificate");
    println!("  server.der   — backend service cert");
    println!("  server.key   — backend service private key");
    println!("  client.der   — frontend service cert");
    println!("  client.key   — frontend service private key");
    println!("Cleanup: remove demo credentials from {}", out.display());
    println!(
        "  rm -rf -- '{}'",
        out.display().to_string().replace('\'', "'\\''")
    );
    println!();
    println!("Trust domain: {}", TRUST_DOMAIN);
    println!(
        "Server SPIFFE ID: spiffe://{}/ns/default/sa/backend",
        TRUST_DOMAIN
    );
    println!(
        "Client SPIFFE ID: spiffe://{}/ns/default/sa/frontend",
        TRUST_DOMAIN
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn demo_keys_and_directory_are_owner_only_and_never_overwritten() {
        let directory =
            std::env::temp_dir().join(format!("interlink-bootstrap-test-{}", std::process::id(),));
        create_private_directory(&directory).unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(create_private_directory(&directory).is_err());
        let key = directory.join("key.pem");
        write_private_key(&key, b"private").unwrap();
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_private_key(&key, b"replacement").is_err());
        assert_eq!(fs::read(&key).unwrap(), b"private");
        fs::remove_dir_all(directory).unwrap();
    }
}
