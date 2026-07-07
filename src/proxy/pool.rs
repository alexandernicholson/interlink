use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::debug;

/// Maximum idle connections per upstream address.
const MAX_IDLE_PER_HOST: usize = 8;

/// How long an idle connection stays in the pool before being closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A pooled idle mTLS stream.
struct Pooled {
    stream: tokio_rustls::TlsStream<TcpStream>,
    added_at: Instant,
}

/// Connection pool for outbound mTLS connections.
///
/// Avoids the cost of a full TCP connect + mTLS handshake when reconnecting
/// to the same upstream within a short window.
pub(crate) struct ConnectionPool {
    inner: Mutex<HashMap<String, Vec<Pooled>>>,
}

impl ConnectionPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Try to borrow an idle mTLS stream for the given upstream address.
    pub async fn checkout(&self, upstream: &str) -> Option<tokio_rustls::TlsStream<TcpStream>> {
        let mut inner = self.inner.lock().await;
        let entries = inner.get_mut(upstream)?;
        let now = Instant::now();
        while let Some(pooled) = entries.pop() {
            if now.duration_since(pooled.added_at) < IDLE_TIMEOUT {
                debug!("reused pooled connection to {}", upstream);
                return Some(pooled.stream);
            }
            drop(pooled); // close on drop
        }
        inner.remove(upstream);
        None
    }

    /// Return a used mTLS stream to the pool for reuse.
    pub async fn checkin(&self, upstream: &str, stream: tokio_rustls::TlsStream<TcpStream>) {
        let mut inner = self.inner.lock().await;
        let entries = inner.entry(upstream.to_string()).or_default();
        if entries.len() < MAX_IDLE_PER_HOST {
            entries.push(Pooled {
                stream,
                added_at: Instant::now(),
            });
        }
    }
}
