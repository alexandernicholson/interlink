//! Descriptor exhaustion must not turn an acceptor into a busy loop. Inbound and
//! outbound proxies share one acceptor (C3); both are driven here.
//!
//! The open-file limit is process-wide, so this file holds a single test.
#![cfg(unix)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use interlink::common::error::InterlinkError;
use interlink::policy::PolicyEngine;
use interlink::proxy::config::ProxyConfig;
use interlink::proxy::handshake::TlsStream;
use interlink::proxy::{OutboundProxy, TcpProxy, TlsHandshake};
use interlink_test_support::DescriptorExhaustion;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

/// Counts ERROR events; the acceptor logs one per failed accept().
struct ErrorEvents(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for ErrorEvents {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if *event.metadata().level() == tracing::Level::ERROR {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Counts connections handed past accept and refuses them, so none stay open.
struct Refusing(Arc<AtomicUsize>);

#[async_trait]
impl TlsHandshake for Refusing {
    async fn connect(&self, _addr: SocketAddr) -> Result<TlsStream, InterlinkError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(InterlinkError::Protocol("test refuses handshakes".into()))
    }

    async fn accept(&self, _stream: TcpStream) -> Result<TlsStream, InterlinkError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(InterlinkError::Protocol("test refuses handshakes".into()))
    }
}

fn config() -> ProxyConfig {
    ProxyConfig {
        trust_domain: "accept.local".into(),
        identity: Some("spiffe://accept.local/ns/default/sa/proxy".into()),
        default_upstream: Some("127.0.0.1:9".into()),
        mux: false,
        ..Default::default()
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_until(what: &str, deadline: Duration, ready: impl Fn() -> bool) {
    tokio::time::timeout(deadline, async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn exhaust_back_off_recover_and_shut_down(
    kind: &str,
    errors: &Arc<AtomicUsize>,
    start: impl FnOnce(u16, Arc<dyn TlsHandshake>, watch::Receiver<bool>) -> JoinHandle<()>,
) {
    let port = free_port();
    let handled = Arc::new(AtomicUsize::new(0));
    let (shutdown, shutdown_rx) = watch::channel(false);
    let task = start(port, Arc::new(Refusing(handled.clone())), shutdown_rx);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    // Binding is asynchronous: the first successful connect is the only probe.
    wait_until(&format!("{kind} listener"), Duration::from_secs(5), || {
        std::net::TcpStream::connect(addr).is_ok()
    })
    .await;
    wait_until(&format!("{kind} probe handled"), Duration::from_secs(5), || {
        handled.load(Ordering::SeqCst) == 1
    })
    .await;
    let handled_before = handled.load(Ordering::SeqCst);
    let errors_before = errors.load(Ordering::SeqCst);
    let exhaustion = DescriptorExhaustion::leave_one_free().unwrap();
    let pending = std::net::TcpStream::connect(addr).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let failed = errors.load(Ordering::SeqCst) - errors_before;
    assert!(failed >= 1, "{kind}: the acceptor never hit EMFILE");
    assert!(
        failed <= 20,
        "{kind}: {failed} accept errors in 600 ms; the acceptor is spinning"
    );
    assert_eq!(handled.load(Ordering::SeqCst), handled_before);

    // Linux keeps the refused connection queued; macOS drops it on EMFILE. A
    // fresh connection proves the acceptor resumes on both.
    drop(exhaustion);
    let fresh = std::net::TcpStream::connect(addr).unwrap();
    wait_until(
        &format!("{kind} to accept once descriptors free up"),
        Duration::from_secs(3),
        || handled.load(Ordering::SeqCst) > handled_before,
    )
    .await;
    drop((pending, fresh));

    // By 1.3 s the acceptor is inside a one-second backoff sleep; shutdown must
    // interrupt it rather than wait the sleep out.
    let exhaustion = DescriptorExhaustion::leave_one_free().unwrap();
    let pending = std::net::TcpStream::connect(addr).unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    shutdown.send(true).unwrap();
    tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .unwrap_or_else(|_| panic!("{kind}: shutdown waited out the accept backoff"))
        .unwrap();
    drop(exhaustion);
    drop(pending);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn descriptor_exhaustion_backs_off_recovers_and_shuts_down() {
    // One acceptor per listener, so the pending connection meets the one
    // whose error rate is measured.
    std::env::set_var("INTERLINK_ACCEPTORS", "1");
    let errors = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(ErrorEvents(errors.clone())),
    )
    .unwrap();

    exhaust_back_off_recover_and_shut_down("inbound", &errors, |port, handshake, shutdown| {
        let proxy = TcpProxy::new_with_port(config(), port, handshake, Arc::new(PolicyEngine::new()))
            .unwrap()
            .with_shutdown(shutdown);
        Arc::new(proxy).spawn()
    })
    .await;
    exhaust_back_off_recover_and_shut_down("outbound", &errors, |port, handshake, shutdown| {
        let proxy =
            OutboundProxy::new_with_port(config(), port, handshake, Arc::new(PolicyEngine::new()))
                .unwrap()
                .with_shutdown(shutdown);
        Arc::new(proxy).spawn()
    })
    .await;
}
