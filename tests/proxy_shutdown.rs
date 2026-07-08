//! R26/R27 regression tests (tenth review round).
//!
//! R27: `with_shutdown(rx)` — the documented shutdown API — must stop the
//! accept loops (it was a silent no-op while acceptors polled a private flag).
//! R26: acceptor bind/listen failure must not panic (process abort under
//! `panic = "abort"`); zero bindable acceptors means run() returns with an
//! error log instead of running headless.
use std::sync::Arc;
use std::time::Duration;

use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::identity::ca::CertificateAuthority;
use interlink::policy::PolicyEngine;
use interlink::proxy::config::ProxyConfig;
use interlink::proxy::handshake::TlsServer;
use interlink::proxy::tcp::TcpProxy;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

struct P {
    id: SpiffeId,
    td: TrustDomain,
}
impl IdentityProvider for P {
    fn get_identity(&self) -> Result<SpiffeId, interlink::common::error::InterlinkError> {
        Ok(self.id.clone())
    }
    fn get_trust_domain(&self) -> &TrustDomain {
        &self.td
    }
}

fn make_proxy(port: u16) -> TcpProxy {
    let ca = CertificateAuthority::new("shutdown-test.local").unwrap();
    let id = SpiffeId::try_new("shutdown-test.local", "default", "proxy").unwrap();
    let (cert, key) = ca.issue_leaf_with_key(&id, &["localhost"]).unwrap();
    let td = TrustDomain::new("shutdown-test.local").with_ca(ca.root_cert_der().to_vec());
    let tls_server = Arc::new(
        TlsServer::new(
            Arc::new(P { id: id.clone(), td }),
            cert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .unwrap(),
    );
    let config = ProxyConfig {
        trust_domain: "shutdown-test.local".into(),
        identity: Some(id.to_uri()),
        default_upstream: Some("127.0.0.1:9".into()),
        max_connections: Some(4),
        mux: true,
    };
    TcpProxy::new_with_port(config, port, tls_server, Arc::new(PolicyEngine::new()))
}

/// R27: sending on the watch channel passed to `with_shutdown` must terminate
/// `run()` (all acceptors + graceful shutdown) promptly.
#[tokio::test]
async fn test_watch_shutdown_stops_proxy() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (tx, rx) = tokio::sync::watch::channel(false);
    let proxy = Arc::new(make_proxy(port).with_shutdown(rx));
    let handle = proxy.spawn();

    // Let the acceptors start.
    tokio::time::sleep(Duration::from_millis(300)).await;

    tx.send(true).unwrap();
    let done = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(
        done.is_ok(),
        "run() did not stop within 5s of the watch shutdown signal"
    );
}

/// R27 edge: a channel signalled *before* run() starts must also stop it
/// (wait_shutdown checks the current value, not just future changes).
#[tokio::test]
async fn test_watch_shutdown_already_signalled() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (tx, rx) = tokio::sync::watch::channel(false);
    tx.send(true).unwrap();
    let proxy = Arc::new(make_proxy(port).with_shutdown(rx));
    let done = tokio::time::timeout(Duration::from_secs(5), proxy.spawn()).await;
    assert!(done.is_ok(), "pre-signalled shutdown did not stop run()");
}

/// R26: when no acceptor can bind (privileged port as non-root), run() must
/// return promptly — no panic, no headless proxy.
#[tokio::test]
async fn test_bind_failure_returns_instead_of_panicking() {
    // Root can bind port 1; skip there (CI containers occasionally run as root).
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: running as root, privileged bind would succeed");
        return;
    }
    let proxy = Arc::new(make_proxy(1));
    let done = tokio::time::timeout(Duration::from_secs(5), proxy.spawn()).await;
    let join = done.expect("run() should return promptly when nothing can bind");
    assert!(join.is_ok(), "run() panicked on bind failure: {:?}", join);
}
