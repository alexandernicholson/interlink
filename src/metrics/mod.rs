use std::sync::LazyLock;

use metrics::{counter, gauge, histogram, Counter, Gauge, Histogram};
use metrics_exporter_prometheus::PrometheusBuilder;

use crate::common::constants::ports;

/// Global metrics registry.
pub static INTERLINK_METRICS: LazyLock<InterlinkMetrics> = LazyLock::new(InterlinkMetrics::new);

/// Descriptive metrics for the proxy.
pub struct InterlinkMetrics {
    pub connections_total: Counter,
    pub connections_active: Gauge,
    pub handshakes_total: Counter,
    pub bytes_total: Counter,
    pub connection_duration: Histogram,
    pub handshake_duration: Histogram,
    pub policy_allowed_total: Counter,
    pub policy_denied_total: Counter,
}

impl Default for InterlinkMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl InterlinkMetrics {
    pub fn new() -> Self {
        Self {
            connections_total: counter!("interlink_connections_total"),
            connections_active: gauge!("interlink_connections_active"),
            handshakes_total: counter!("interlink_handshakes_total"),
            bytes_total: counter!("interlink_bytes_total"),
            connection_duration: histogram!("interlink_connection_duration_seconds"),
            handshake_duration: histogram!("interlink_handshake_duration_seconds"),
            policy_allowed_total: counter!("interlink_policy_allowed_total"),
            policy_denied_total: counter!("interlink_policy_denied_total"),
        }
    }
}

/// Initialize the Prometheus metrics exporter.
pub fn init_metrics_exporter() -> Result<(), Box<dyn std::error::Error>> {
    let addr: std::net::SocketAddr = ([0, 0, 0, 0], ports::METRICS).into();
    let builder = PrometheusBuilder::new();
    builder.with_http_listener(addr).install()?;
    Ok(())
}

use std::time::Instant;

// Thread-local start times to avoid contention on the histogram recording path.
std::thread_local! {
    static CONN_START: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static HANDSHAKE_START: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Record a connection completion.
pub fn record_connection(bytes_up: u64, bytes_down: u64) {
    let m = &INTERLINK_METRICS;
    CONN_START.with(|start| {
        if let Some(t0) = start.take() {
            m.connection_duration.record(t0.elapsed().as_secs_f64());
        }
    });
    m.connections_total.increment(1);
    m.bytes_total.increment(bytes_up + bytes_down);
    m.connections_active.decrement(1);
}

/// Record the start of a new connection.
pub fn record_connection_start() {
    INTERLINK_METRICS.connections_active.increment(1);
    CONN_START.with(|start| {
        start.set(Some(Instant::now()));
    });
}

/// Record a handshake completion.
pub fn record_handshake(_success: bool) {
    let m = &INTERLINK_METRICS;
    HANDSHAKE_START.with(|start| {
        if let Some(t0) = start.take() {
            m.handshake_duration.record(t0.elapsed().as_secs_f64());
        }
    });
    m.handshakes_total.increment(1);
}

/// Record a handshake failure.
pub fn record_handshake_error() {
    let m = &INTERLINK_METRICS;
    HANDSHAKE_START.with(|start| {
        start.take();
    });
    m.handshakes_total.increment(1);
}

/// Record the start of a handshake.
pub fn record_handshake_start() {
    HANDSHAKE_START.with(|start| {
        start.set(Some(Instant::now()));
    });
}

/// Record a policy decision.
pub fn record_policy(decision: &crate::policy::Decision) {
    let m = &INTERLINK_METRICS;
    match decision {
        crate::policy::Decision::Allow => m.policy_allowed_total.increment(1),
        crate::policy::Decision::Deny(_) => m.policy_denied_total.increment(1),
    }
}

/// Test-only counters for verifying metrics behavior.
pub mod test_helpers {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::LazyLock;

    pub static TEST_CONNECTIONS: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));
    pub static TEST_BYTES: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));
    pub static TEST_ALLOWED: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));
    pub static TEST_DENIED: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));
    pub static TEST_HANDSHAKE_ERRORS: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));

    pub fn record_connection() {
        TEST_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bytes(n: u64) {
        TEST_BYTES.fetch_add(n, Ordering::Relaxed);
    }

    pub fn record_policy(allowed: bool) {
        if allowed {
            TEST_ALLOWED.fetch_add(1, Ordering::Relaxed);
        } else {
            TEST_DENIED.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn increment_connections() {
        record_connection();
    }
    pub fn add_bytes(n: u64) {
        record_bytes(n);
    }
    pub fn increment_allowed() {
        TEST_ALLOWED.fetch_add(1, Ordering::Relaxed);
    }
    pub fn increment_denied() {
        TEST_DENIED.fetch_add(1, Ordering::Relaxed);
    }
    pub fn increment_handshake_errors() {
        TEST_HANDSHAKE_ERRORS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn reset() {
        TEST_CONNECTIONS.store(0, Ordering::Relaxed);
        TEST_BYTES.store(0, Ordering::Relaxed);
        TEST_ALLOWED.store(0, Ordering::Relaxed);
        TEST_DENIED.store(0, Ordering::Relaxed);
        TEST_HANDSHAKE_ERRORS.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers as test_metrics;

    #[test]
    fn test_metrics_counters() {
        test_metrics::reset();
        test_metrics::record_connection();
        test_metrics::record_bytes(1024);
        test_metrics::record_policy(true);
        test_metrics::record_policy(false);

        assert_eq!(
            test_metrics::TEST_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            test_metrics::TEST_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            1024
        );
        assert_eq!(
            test_metrics::TEST_ALLOWED.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            test_metrics::TEST_DENIED.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn test_metrics_multiple_increments() {
        test_metrics::reset();
        for _ in 0..100 {
            test_metrics::record_connection();
        }
        assert_eq!(
            test_metrics::TEST_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed),
            100
        );
    }

    #[test]
    fn test_metrics_reset() {
        test_metrics::reset();
        assert_eq!(
            test_metrics::TEST_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        test_metrics::record_connection();
        assert_eq!(
            test_metrics::TEST_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        test_metrics::reset();
        assert_eq!(
            test_metrics::TEST_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}
