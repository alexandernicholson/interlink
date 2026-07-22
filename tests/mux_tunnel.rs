//! Mux tunnel (handshake avoidance) integration tests.
//!
//! Topology per test: local client → OutboundProxy —mTLS(mux)→ TcpProxy → echo.
//! The property under test is *handshake avoidance*: N application connections
//! over one peer cost exactly 1 TLS handshake (counted by a wrapper around the
//! real `TlsClient` — global metrics are shared across tests and unusable for
//! assertions).
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use interlink::common::error::InterlinkError;
use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::identity::ca::CertificateAuthority;
use interlink::policy::PolicyEngine;
use interlink::proxy::config::ProxyConfig;
use interlink::proxy::handshake::{legacy_alpn_protocols, TlsClient, TlsServer, TlsStream};
use interlink::proxy::outbound::OutboundProxy;
use interlink::proxy::tcp::TcpProxy;
use interlink::proxy::TlsHandshake;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct P {
    id: SpiffeId,
    td: TrustDomain,
}
impl IdentityProvider for P {
    fn get_identity(&self) -> Result<SpiffeId, InterlinkError> {
        Ok(self.id.clone())
    }
    fn get_trust_domain(&self) -> &TrustDomain {
        &self.td
    }
}

/// Counts TLS handshakes performed by the wrapped client (A10: the observable
/// signal for "avoided handshakes").
struct CountingHandshake {
    inner: Arc<dyn TlsHandshake>,
    connects: Arc<AtomicUsize>,
}

#[async_trait]
impl TlsHandshake for CountingHandshake {
    async fn connect(&self, addr: SocketAddr) -> Result<TlsStream, InterlinkError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.inner.connect(addr).await
    }
    async fn accept(&self, stream: TcpStream) -> Result<TlsStream, InterlinkError> {
        self.inner.accept(stream).await
    }
}

struct Mesh {
    outbound_port: u16,
    handshakes: Arc<AtomicUsize>,
}

fn pick_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Echo backend that sleeps `hold` before replying, so many streams stay
/// concurrently in-flight (to exercise per-session stream caps).
async fn echo_held(listener: TcpListener, hold: Duration) {
    loop {
        let Ok((mut s, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut data = Vec::new();
            if s.read_to_end(&mut data).await.is_ok() {
                tokio::time::sleep(hold).await;
                let _ = s.write_all(b"echo:").await;
                let _ = s.write_all(&data).await;
            }
            let _ = s.shutdown().await;
        });
    }
}

/// Echo server that reads to EOF, then writes everything back prefixed with
/// "echo:" — deliberately half-close-dependent (C7).
async fn echo_to_eof(listener: TcpListener) {
    loop {
        let Ok((mut s, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut data = Vec::new();
            if s.read_to_end(&mut data).await.is_ok() {
                let _ = s.write_all(b"echo:").await;
                let _ = s.write_all(&data).await;
            }
            let _ = s.shutdown().await;
        });
    }
}

/// Build the two-proxy mesh. `inbound_mux` = false models a pre-mux peer;
/// `outbound_mux` = false models INTERLINK_MUX=false (legacy ALPN offer).
async fn start_mesh_full(td_name: &str, inbound_mux: bool, outbound_mux: bool) -> Mesh {
    start_mesh_cfg(td_name, inbound_mux, outbound_mux, Duration::ZERO).await
}

async fn start_mesh_cfg(
    td_name: &str,
    inbound_mux: bool,
    outbound_mux: bool,
    echo_hold: Duration,
) -> Mesh {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let ca = CertificateAuthority::new(td_name).unwrap();
    let server_id = SpiffeId::try_new(td_name, "default", "backend-proxy").unwrap();
    let client_id = SpiffeId::try_new(td_name, "default", "frontend-proxy").unwrap();
    // IP SAN: the outbound proxy dials the inbound proxy by 127.0.0.1:port.
    let (scert, skey) = ca
        .issue_leaf_with_key(&server_id, &["localhost", "127.0.0.1"])
        .unwrap();
    let (ccert, ckey) = ca
        .issue_leaf_with_key(&client_id, &["localhost", "127.0.0.1"])
        .unwrap();
    let td = TrustDomain::new(td_name).with_ca(ca.root_cert_der().to_vec());

    // Echo backend.
    let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo_listener.local_addr().unwrap();
    if echo_hold.is_zero() {
        tokio::spawn(echo_to_eof(echo_listener));
    } else {
        tokio::spawn(echo_held(echo_listener, echo_hold));
    }

    // Inbound proxy.
    let inbound_port = pick_port();
    let sprov = Arc::new(P {
        id: server_id.clone(),
        td: td.clone(),
    });
    let tls_server = Arc::new(if inbound_mux {
        TlsServer::new(
            sprov.clone(),
            scert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(skey)),
        )
        .unwrap()
    } else {
        TlsServer::new_with_alpn(
            sprov.clone(),
            scert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(skey)),
            legacy_alpn_protocols(),
        )
        .unwrap()
    });
    let inbound_policy = Arc::new(PolicyEngine::new());
    inbound_policy.add_namespace_rule(
        "default",
        interlink::policy::patterns::allow(
            &format!("spiffe://{}/ns/default/sa/*", td_name),
            &format!("spiffe://{}/ns/default/sa/*", td_name),
            "mesh test",
        ),
    );
    let inbound_config = ProxyConfig {
        trust_domain: td_name.into(),
        identity: Some(server_id.to_uri()),
        default_upstream: Some(echo_addr.to_string()),
        max_connections: Some(10000),
        mux: true,
    };
    let inbound = TcpProxy::new_with_port(
        inbound_config,
        inbound_port,
        tls_server,
        inbound_policy.clone(),
    );
    Arc::new(inbound).spawn();

    // Outbound proxy with counting TLS client.
    let outbound_port = pick_port();
    let cprov = Arc::new(P {
        id: client_id.clone(),
        td,
    });
    let tls_client = Arc::new(if outbound_mux {
        TlsClient::with_client_auth(
            cprov,
            ccert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ckey)),
        )
        .unwrap()
    } else {
        TlsClient::with_client_auth_alpn(
            cprov,
            ccert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ckey)),
            legacy_alpn_protocols(),
        )
        .unwrap()
    });
    let handshakes = Arc::new(AtomicUsize::new(0));
    let counting = Arc::new(CountingHandshake {
        inner: tls_client,
        connects: handshakes.clone(),
    });
    let outbound_config = ProxyConfig {
        trust_domain: td_name.into(),
        identity: Some(client_id.to_uri()),
        default_upstream: Some(format!("127.0.0.1:{}", inbound_port)),
        max_connections: Some(10000),
        mux: true,
    };
    let outbound =
        OutboundProxy::new_with_port(outbound_config, outbound_port, counting, inbound_policy);
    Arc::new(outbound).spawn();

    tokio::time::sleep(Duration::from_millis(300)).await;
    Mesh {
        outbound_port,
        handshakes,
    }
}

async fn start_mesh(td_name: &str, inbound_mux: bool) -> Mesh {
    start_mesh_full(td_name, inbound_mux, true).await
}

/// One app connection through the mesh: write payload, half-close, read reply.
async fn roundtrip(port: u16, payload: &[u8]) -> Vec<u8> {
    let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    c.write_all(payload).await.unwrap();
    c.shutdown().await.unwrap();
    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut resp))
        .await
        .expect("roundtrip timed out (half-close lost through mux?)")
        .unwrap();
    resp
}

/// N sequential connections cost exactly 1 handshake (the property).
#[tokio::test]
async fn test_mux_single_handshake_for_n_connections() {
    let mesh = start_mesh("mux-seq.local", true).await;
    for i in 0..5 {
        let payload = format!("hello-{}", i);
        let resp = roundtrip(mesh.outbound_port, payload.as_bytes()).await;
        assert_eq!(resp, format!("echo:{}", payload).as_bytes());
    }
    assert_eq!(
        mesh.handshakes.load(Ordering::SeqCst),
        1,
        "5 connections should share 1 TLS handshake over the mux tunnel"
    );
}

/// Concurrent connections multiplex correctly: distinct payloads come back on
/// the right streams, still one handshake (A2: concurrency test).
#[tokio::test]
async fn test_mux_concurrent_streams() {
    let mesh = start_mesh("mux-conc.local", true).await;
    let mut handles = Vec::new();
    for i in 0..10 {
        let port = mesh.outbound_port;
        handles.push(tokio::spawn(async move {
            let payload = format!("stream-{}-{}", i, "x".repeat(1000 * (i + 1)));
            let resp = roundtrip(port, payload.as_bytes()).await;
            assert_eq!(
                resp,
                format!("echo:{}", payload).as_bytes(),
                "stream {} data corrupted",
                i
            );
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(
        mesh.handshakes.load(Ordering::SeqCst),
        1,
        "10 concurrent connections should share 1 TLS handshake"
    );
}

/// A peer that doesn't offer the mux ALPN falls back to 1:1 relays —
/// N connections, N handshakes, everything still works.
#[tokio::test]
async fn test_mux_fallback_to_legacy_peer() {
    let mesh = start_mesh("mux-legacy.local", false).await;
    for i in 0..3 {
        let payload = format!("legacy-{}", i);
        let resp = roundtrip(mesh.outbound_port, payload.as_bytes()).await;
        assert_eq!(resp, format!("echo:{}", payload).as_bytes());
    }
    assert_eq!(
        mesh.handshakes.load(Ordering::SeqCst),
        3,
        "legacy peer: each connection needs its own handshake"
    );
}

/// Half-close propagates through the entire mux path (C7): the echo backend
/// reads to EOF, so a lost FIN anywhere hangs the roundtrip. The roundtrip
/// helper already depends on this; this test makes the property explicit with
/// a larger-than-one-window payload.
#[tokio::test]
async fn test_mux_half_close_large_payload() {
    let mesh = start_mesh("mux-halfclose.local", true).await;
    let payload = vec![0xA5u8; 512 * 1024];
    let mut expected = b"echo:".to_vec();
    expected.extend_from_slice(&payload);
    let resp = roundtrip(mesh.outbound_port, &payload).await;
    assert_eq!(resp.len(), expected.len(), "payload truncated through mux");
    assert_eq!(resp, expected, "payload corrupted through mux");
}

/// INTERLINK_MUX=false semantics: the client offers only legacy ALPN, so even
/// a mux-capable peer negotiates h2 and both sides speak 1:1 — N handshakes,
/// no protocol mismatch (the bug this test pins: a disabled client must not
/// offer mux and then write raw bytes into a yamux-expecting peer).
#[tokio::test]
async fn test_mux_disabled_client_offers_legacy() {
    let mesh = start_mesh_full("mux-off.local", true, false).await;
    for i in 0..3 {
        let payload = format!("off-{}", i);
        let resp = roundtrip(mesh.outbound_port, payload.as_bytes()).await;
        assert_eq!(resp, format!("echo:{}", payload).as_bytes());
    }
    assert_eq!(
        mesh.handshakes.load(Ordering::SeqCst),
        3,
        "mux disabled: each connection needs its own handshake"
    );
}

/// Reproducer for the K8s finding: many *concurrent* connections through the
/// mux path must all succeed. The single-tunnel design dropped connections at
/// ~200 concurrent in the K8s benchmark (host had headroom), so this drives
/// 300 concurrent roundtrips and asserts zero failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mux_high_concurrency() {
    let mesh = start_mesh("mux-conc-hi.local", true).await;
    let n = 300;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let port = mesh.outbound_port;
        let b = barrier.clone();
        handles.push(tokio::spawn(async move {
            b.wait().await; // release all at once → genuine concurrency
            let payload = format!("c{}", i);
            let mut c = TcpStream::connect(("127.0.0.1", port))
                .await
                .map_err(|e| format!("connect: {e}"))?;
            c.write_all(payload.as_bytes())
                .await
                .map_err(|e| format!("write: {e}"))?;
            c.shutdown().await.map_err(|e| format!("shutdown: {e}"))?;
            let mut resp = Vec::new();
            tokio::time::timeout(Duration::from_secs(20), c.read_to_end(&mut resp))
                .await
                .map_err(|_| "timeout".to_string())
                .and_then(|r| r.map_err(|e| format!("read: {e}")))?;
            if resp != format!("echo:{}", payload).into_bytes() {
                return Err(format!("bad resp len {}", resp.len()));
            }
            Ok::<(), String>(())
        }));
    }
    let mut errs = 0;
    for h in handles {
        if let Err(e) = h.await.unwrap() {
            errs += 1;
            if errs <= 5 {
                eprintln!("conn error: {e}");
            }
        }
    }
    assert_eq!(
        errs, 0,
        "{errs}/{n} concurrent connections failed through the mux tunnel"
    );
}

/// >512 concurrent streams: a single yamux session caps at max_num_streams=512,
/// so a single-tunnel-per-peer design fails opens beyond that. This drives 700
/// concurrent connections through a briefly-held backend to force many streams
/// live at once, exercising the tunnel pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mux_beyond_single_session_cap() {
    let mesh = start_mesh_cfg("mux-cap.local", true, true, Duration::from_millis(600)).await;
    let n = 700;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let port = mesh.outbound_port;
        let b = barrier.clone();
        handles.push(tokio::spawn(async move {
            b.wait().await;
            let payload = format!("cap{}", i);
            let mut c = TcpStream::connect(("127.0.0.1", port))
                .await
                .map_err(|e| format!("connect: {e}"))?;
            c.write_all(payload.as_bytes())
                .await
                .map_err(|e| format!("write: {e}"))?;
            c.shutdown().await.map_err(|e| format!("shutdown: {e}"))?;
            let mut resp = Vec::new();
            tokio::time::timeout(Duration::from_secs(30), c.read_to_end(&mut resp))
                .await
                .map_err(|_| "timeout".to_string())
                .and_then(|r| r.map_err(|e| format!("read: {e}")))?;
            if resp != format!("echo:{}", payload).into_bytes() {
                return Err(format!("bad resp len {}", resp.len()));
            }
            Ok::<(), String>(())
        }));
    }
    let mut errs = 0;
    for h in handles {
        if h.await.unwrap().is_err() {
            errs += 1;
        }
    }
    assert_eq!(
        errs, 0,
        "{errs}/{n} connections failed (single-session cap?)"
    );
}
