//! Rolling upgrades must keep speaking Yamux v0 in both directions.

use super::*;
use crate::common::identity::CompiledPattern;
use crate::policy::PolicyRule;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

const INTEROP_TIMEOUT: Duration = Duration::from_secs(10);
const PAYLOAD_SIZE: usize = 512 * 1024;

type StockOpen = oneshot::Sender<yamux::Stream>;

fn identity(account: &str) -> SpiffeId {
    SpiffeId::try_new("interop.example", "apps", account).unwrap()
}

fn stock_config() -> yamux::Config {
    let mut config = yamux::Config::default();
    config
        .set_max_connection_receive_window(None)
        .set_max_num_streams(8)
        .set_max_connection_receive_window(Some(8 * mux::STREAM_WINDOW as usize));
    config
}

// JoinSet owns every explicitly spawned task, including backend handlers. Dropping
// the outer test future on panic/timeout aborts its peers; closing the duplex peer
// also wakes the pool's internally owned driver so it can evict itself.
async fn stock_server(io: tokio::io::DuplexStream, reset_seen: mpsc::Sender<()>) {
    let mut conn = yamux::Connection::new(io.compat(), stock_config(), yamux::Mode::Server);
    let mut echoes = JoinSet::new();
    loop {
        tokio::select! {
            result = echoes.join_next(), if !echoes.is_empty() => {
                result.unwrap().unwrap();
            }
            next = futures_util::future::poll_fn(|cx| conn.poll_next_inbound(cx)) => {
                let Some(stream) = next else { return };
                echoes.spawn(echo_after_eof(stream.unwrap().compat(), reset_seen.clone()));
            }
        }
    }
}

async fn stock_client(io: tokio::io::DuplexStream, mut opens: mpsc::Receiver<StockOpen>) {
    let mut conn = yamux::Connection::new(io.compat(), stock_config(), yamux::Mode::Client);
    let mut pending = None;
    futures_util::future::poll_fn(|cx| {
        loop {
            if pending.is_none() {
                match opens.poll_recv(cx) {
                    Poll::Ready(Some(request)) => pending = Some(request),
                    Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Pending => {}
                }
            }
            let Some(request) = pending.take() else { break };
            match conn.poll_new_outbound(cx) {
                Poll::Ready(stream) => {
                    assert!(request.send(stream.unwrap()).is_ok());
                }
                Poll::Pending => {
                    pending = Some(request);
                    break;
                }
            }
        }
        match conn.poll_next_inbound(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(()),
            Poll::Ready(Some(result)) => panic!("unexpected stock-client inbound: {result:?}"),
        }
    })
    .await;
}

async fn stock_open(opens: &mpsc::Sender<StockOpen>) -> tokio_util::compat::Compat<yamux::Stream> {
    let (request, response) = oneshot::channel();
    opens.send(request).await.unwrap();
    response.await.unwrap().compat()
}

async fn echo_backend(listener: TcpListener, reset_seen: mpsc::Sender<()>) {
    let mut echoes = JoinSet::new();
    loop {
        tokio::select! {
            result = echoes.join_next(), if !echoes.is_empty() => {
                result.unwrap().unwrap();
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted.unwrap();
                echoes.spawn(echo_after_eof(stream, reset_seen.clone()));
            }
        }
    }
}

async fn echo_after_eof<S>(mut stream: S, reset_seen: mpsc::Sender<()>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut marker = [0];
    stream.read_exact(&mut marker).await.unwrap();
    stream.write_all(&marker).await.unwrap();
    stream.flush().await.unwrap();
    if marker == [0] {
        // The client drops this acknowledged, still-open stream without FIN.
        // Stock Yamux exposes RST as EOF; the real inbound relay must propagate
        // termination to its TCP backend as well, without affecting siblings.
        assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
        reset_seen.send(()).await.unwrap();
        return;
    }
    let mut payload = Vec::new();
    stream.read_to_end(&mut payload).await.unwrap();
    stream.write_all(b"echo:").await.unwrap();
    stream.write_all(&payload).await.unwrap();
    stream.shutdown().await.unwrap();
}

async fn prime<S>(mut stream: S, marker: u8) -> S
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(&[marker]).await.unwrap();
    stream.flush().await.unwrap();
    let mut ack = [0];
    stream.read_exact(&mut ack).await.unwrap();
    assert_eq!(ack, [marker], "peer must acknowledge this exact stream");
    stream
}

fn payload(seed: u8) -> Vec<u8> {
    (0..PAYLOAD_SIZE)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

async fn finish<S>(mut stream: S, payload: &[u8])
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.shutdown().await.unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).await.unwrap();
    assert_eq!(
        reply.len(),
        5 + payload.len(),
        "truncated echo or missing EOF"
    );
    assert_eq!(&reply[..5], b"echo:");
    assert_eq!(&reply[5..], payload, "cross-stream corruption");
}

async fn exchange<S>(mut stream: S, payload: &[u8])
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(payload).await.unwrap();
    finish(stream, payload).await;
}

async fn concurrent_exchanges<S>(first: S, mut second: S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let first_payload = payload(17);
    let second_payload = payload(193);
    let (first_done, wait_for_first) = oneshot::channel();
    tokio::join!(
        async {
            exchange(first, &first_payload).await;
            first_done.send(()).unwrap();
        },
        async {
            second.write_all(&second_payload).await.unwrap();
            // The first stream must receive its complete response AND EOF while
            // this sibling's write side is still open. No timing assumption.
            wait_for_first.await.unwrap();
            finish(second, &second_payload).await;
        }
    );
}

#[tokio::test]
async fn interop_native_pool_to_stock_server_large_half_close_reset_and_reuse() {
    timeout(INTEROP_TIMEOUT, async {
        let (native, stock) = tokio::io::duplex(32 * 1024);
        let (reset_seen, mut resets) = mpsc::channel(1);
        let mut tasks = JoinSet::new();
        tasks.spawn(stock_server(stock, reset_seen));
        let pool = MuxPool::new(8);
        let addr = SocketAddr::from(([127, 0, 0, 1], 32101));
        let reserved = pool
            .register_connection(addr, native, Arc::new(identity("stock")))
            .unwrap();
        let tunnel_id = reserved.id;
        let first = prime(reserved.open_stream().await.unwrap(), 1).await;
        let second = prime(
            pool.pick_under_cap(&addr)
                .unwrap()
                .open_stream()
                .await
                .unwrap(),
            2,
        )
        .await;
        let cancelled = prime(
            pool.pick_under_cap(&addr)
                .unwrap()
                .open_stream()
                .await
                .unwrap(),
            0,
        )
        .await;
        drop(cancelled);
        resets.recv().await.unwrap();
        concurrent_exchanges(first, second).await;
        let reused = pool.pick_under_cap(&addr).unwrap();
        assert_eq!(
            reused.id, tunnel_id,
            "follow-up must use the original session"
        );
        let follow_up = prime(reused.open_stream().await.unwrap(), 3).await;
        exchange(follow_up, b"same-session-after-both-streams-finished").await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        while pool.entry_counts().0 != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native pool/stock server interoperability timed out");
}

#[tokio::test]
async fn interop_stock_client_to_native_inbound_large_half_close_reset_and_reuse() {
    timeout(INTEROP_TIMEOUT, async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap();
        let policy = Arc::new(PolicyEngine::new());
        policy.set_namespace_policies(
            "apps",
            vec![PolicyRule {
                source: CompiledPattern::any(),
                destination: CompiledPattern::any(),
                decision: Decision::Allow,
                description: "rolling-version interoperability".into(),
            }],
        );
        let (stock, native) = tokio::io::duplex(32 * 1024);
        let (reset_seen, mut resets) = mpsc::channel(1);
        let mut tasks = JoinSet::new();
        tasks.spawn(echo_backend(listener, reset_seen));
        tasks.spawn(async move {
            serve_connection(
                native,
                upstream,
                identity("stock"),
                identity("native"),
                policy,
                TunnelLimits {
                    max_age: Duration::from_secs(60),
                    max_streams_per_tunnel: 8,
                    total_streams: Arc::new(Semaphore::new(8)),
                },
            )
            .await
            .unwrap();
        });
        let (opens, requests) = mpsc::channel(8);
        tasks.spawn(stock_client(stock, requests));
        let first = prime(stock_open(&opens).await, 1).await;
        let second = prime(stock_open(&opens).await, 2).await;
        let cancelled = prime(stock_open(&opens).await, 0).await;
        drop(cancelled);
        resets.recv().await.unwrap();
        concurrent_exchanges(first, second).await;
        // There is only one stock Connection, and the actual native inbound
        // server must still relay a new stream after both earlier FIN exchanges.
        let follow_up = prime(stock_open(&opens).await, 3).await;
        exchange(follow_up, b"same-session-after-both-streams-finished").await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    })
    .await
    .expect("stock client/native inbound interoperability timed out");
}
