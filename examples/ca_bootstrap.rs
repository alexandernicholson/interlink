//! Shared CA bootstrap for the interlink example services.
//!
//! Generates a root CA, issues server + client certificates,
//! and writes them to temp files so both services can load them.
//!
//! Run first: `cargo run --example ca_bootstrap`

use std::fs;

use interlink::common::identity::SpiffeId;
use interlink::identity::ca::CertificateAuthority;

const TRUST_DOMAIN: &str = "example.local";

fn main() {
    let out = std::env::temp_dir().join("interlink-demo");
    fs::create_dir_all(&out).expect("create demo dir");

    // Generate CA
    let ca = CertificateAuthority::new(TRUST_DOMAIN).expect("CA init");
    fs::write(out.join("ca.der"), ca.root_cert_der()).expect("write CA");

    // Issue server cert (for the backend service)
    let server_id = SpiffeId::new(TRUST_DOMAIN, "default", "backend");
    let (server_cert, server_key) = ca
        .issue_leaf_with_key(&server_id, &["localhost", "backend.example.local"])
        .expect("issue server cert");
    fs::write(out.join("server.der"), &server_cert).expect("write server cert");
    fs::write(out.join("server.key"), &server_key).expect("write server key");

    // Issue client cert (for the frontend service)
    let client_id = SpiffeId::new(TRUST_DOMAIN, "default", "frontend");
    let (client_cert, client_key) = ca
        .issue_leaf_with_key(&client_id, &["localhost", "frontend.example.local"])
        .expect("issue client cert");
    fs::write(out.join("client.der"), &client_cert).expect("write client cert");
    fs::write(out.join("client.key"), &client_key).expect("write client key");

    println!("{}", out.display());
    println!("CA + certs written to: {:?}", out);
    println!("  ca.der       — root CA certificate");
    println!("  server.der   — backend service cert");
    println!("  server.key   — backend service private key");
    println!("  client.der   — frontend service cert");
    println!("  client.key   — frontend service private key");
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
