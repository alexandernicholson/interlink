use std::sync::LazyLock;

use metrics::{counter, gauge, histogram, Counter, Gauge, Histogram};
use metrics_exporter_prometheus::PrometheusBuilder;

use crate::common::constants::ports;

/// Global metrics registry.
pub static INTERLINK_METRICS: LazyLock<InterlinkMetrics> = LazyLock::new(InterlinkMetrics::new);

/// Descriptive metrics for the proxy.
pub struct InterlinkMetrics {
    pub connections_total: Counter,
    pub connection_errors_total: Counter,
    pub connections_active: Gauge,
    pub handshakes_total: Counter,
    pub bytes_total: Counter,
    pub connection_duration: Histogram,
    pub handshake_duration: Histogram,
    pub policy_allowed_total: Counter,
    pub policy_denied_total: Counter,
    pub saturation_rejections_total: Counter,
    pub handshake_full_total: Counter,
    pub handshake_resumed_total: Counter,
    pub mux_tunnels_opened_total: Counter,
    pub mux_streams_total: Counter,
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
            connection_errors_total: counter!("interlink_connection_errors_total"),
            connections_active: gauge!("interlink_connections_active"),
            handshakes_total: counter!("interlink_handshakes_total"),
            bytes_total: counter!("interlink_bytes_total"),
            connection_duration: histogram!("interlink_connection_duration_seconds"),
            handshake_duration: histogram!("interlink_handshake_duration_seconds"),
            policy_allowed_total: counter!("interlink_policy_allowed_total"),
            policy_denied_total: counter!("interlink_policy_denied_total"),
            saturation_rejections_total: counter!("interlink_saturation_rejections_total"),
            handshake_full_total: counter!("interlink_handshake_full_total"),
            handshake_resumed_total: counter!("interlink_handshake_resumed_total"),
            mux_tunnels_opened_total: counter!("interlink_mux_tunnels_opened_total"),
            mux_streams_total: counter!("interlink_mux_streams_total"),
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

/// Record a completed connection with histogram observation.
pub fn record_connection(bytes_up: u64, bytes_down: u64, duration: std::time::Duration) {
    let m = &INTERLINK_METRICS;
    m.connection_duration.record(duration.as_secs_f64());
    m.connections_total.increment(1);
    m.bytes_total.increment(bytes_up + bytes_down);
    m.connections_active.decrement(1);
}

/// Record a failed connection (no histogram — C2: no sentinel values).
pub fn record_connection_failed() {
    let m = &INTERLINK_METRICS;
    m.connections_total.increment(1);
    m.connection_errors_total.increment(1);
    m.connections_active.decrement(1);
}

/// Record the start of a new connection.
pub fn record_connection_start() {
    INTERLINK_METRICS.connections_active.increment(1);
}

/// Record a handshake completion with explicit duration.
pub fn record_handshake(duration: std::time::Duration) {
    let m = &INTERLINK_METRICS;
    m.handshake_duration.record(duration.as_secs_f64());
    m.handshakes_total.increment(1);
}

/// Record a handshake failure.
pub fn record_handshake_error() {
    INTERLINK_METRICS.handshakes_total.increment(1);
}

/// Record whether the handshake was a full TLS handshake or a resumed one.
pub fn record_handshake_kind(resumed: bool) {
    let m = &INTERLINK_METRICS;
    if resumed {
        m.handshake_resumed_total.increment(1);
    } else {
        m.handshake_full_total.increment(1);
    }
}

/// Release a mux tunnel's active-connection slot. The tunnel was counted
/// active by `record_connection_start` at accept; it is transport, not a
/// connection — its streams carry the byte/duration accounting — so it never
/// increments `connections_total` or the duration histogram (C1/C2).
pub fn record_tunnel_closed() {
    INTERLINK_METRICS.connections_active.decrement(1);
}

/// Record establishment of a client-side mux tunnel.
pub fn record_mux_tunnel_opened() {
    INTERLINK_METRICS.mux_tunnels_opened_total.increment(1);
}

/// Record a stream carried over a mux tunnel.
pub fn record_mux_stream() {
    INTERLINK_METRICS.mux_streams_total.increment(1);
}

/// Record a connection rejected due to the saturation limit.
pub fn record_saturation_rejection() {
    INTERLINK_METRICS.saturation_rejections_total.increment(1);
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
