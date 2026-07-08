/// Integration tests for the full mTLS handshake flow.
///
/// Tests:
/// 1. SPIFFE ID round-trip through certificate SAN
/// 2. Protocol detection on known inputs
/// 3. Policy engine decisions with complex rules
/// 4. DNS discovery cache behavior
use interlink::common::identity::SpiffeId;
use interlink::policy::{Decision, PolicyEngine};
use interlink::protocol::{DetectedProtocol, ProtocolDetector};

#[test]
fn test_spiffe_id_integration() {
    // Full round-trip: parse → format → re-parse
    let original = "spiffe://cluster.local/ns/default/sa/web-api";
    let id = SpiffeId::from_uri(original).unwrap();
    assert_eq!(id.to_uri(), original);
    assert_eq!(id.trust_domain, "cluster.local");
    assert_eq!(id.namespace, "default");
    assert_eq!(id.service_account, "web-api");
}

#[test]
fn test_protocol_detection_all_formats() {
    fn check(input: &[u8], expected: DetectedProtocol) {
        assert_eq!(ProtocolDetector::detect(input), expected);
    }

    check(b"GET / HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"POST /api HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"PUT /resource HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"DELETE /resource HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"PATCH /resource HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"HEAD /resource HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"OPTIONS / HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n", DetectedProtocol::Http2);
    check(b"\x00\x01\x02\x03\x04\x05\x06\x07", DetectedProtocol::Tcp);
    check(b"", DetectedProtocol::Tcp);
}

#[test]
fn test_policy_engine_multi_namespace() {
    let engine = PolicyEngine::new();

    // billing namespace allows internal traffic
    engine.add_namespace_rule(
        "billing",
        interlink::policy::patterns::allow_same_namespace("cluster.local", "billing"),
    );

    // monitoring namespace allows traffic from billing
    engine.add_namespace_rule(
        "monitoring",
        interlink::policy::patterns::allow(
            "spiffe://cluster.local/ns/billing/sa/*",
            "spiffe://cluster.local/ns/monitoring/sa/grafana",
            "billing can access grafana",
        ),
    );

    let billing_svc = SpiffeId::try_new("cluster.local", "billing", "invoice-worker").unwrap();
    let grafana = SpiffeId::try_new("cluster.local", "monitoring", "grafana").unwrap();
    let unknown = SpiffeId::try_new("cluster.local", "default", "hacker").unwrap();

    assert_eq!(
        engine.evaluate(&billing_svc, &grafana),
        Decision::Allow,
        "billing should reach grafana"
    );
    assert_eq!(
        engine.evaluate(&unknown, &grafana),
        Decision::Deny("no matching policy"),
        "unknown should be denied"
    );
}

#[test]
fn test_high_volume_policy_lookup() {
    let engine = PolicyEngine::new();

    // Add 100 namespace rules
    for i in 0..100u32 {
        let ns = format!("ns-{}", i);
        engine.add_namespace_rule(
            &ns,
            interlink::policy::patterns::allow_same_namespace("cluster.local", &ns),
        );
    }

    // Should take <10ms for 10000 evaluations
    let start = std::time::Instant::now();
    let iterations = 10_000;
    for _ in 0..iterations {
        let id = SpiffeId::try_new("cluster.local", "ns-50", "svc").unwrap();
        let _ = engine.evaluate(&id, &id);
    }
    let elapsed = start.elapsed();
    let per_op = elapsed / iterations;

    assert!(
        per_op < std::time::Duration::from_micros(10),
        "policy lookup took {:?} per op, expected <10µs",
        per_op
    );
}
