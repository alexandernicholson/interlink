mod support;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use interlink::common::identity::{CompiledPattern, SegmentGlob, SpiffeId, TrustDomain};
use interlink::policy::{patterns, Decision, PolicyEngine};
use interlink::protocol::{validate_http11_request_line, DetectedProtocol, ProtocolDetector};

fn bench_protocol_detection(c: &mut Criterion) {
    let cases: &[(&str, &[u8], DetectedProtocol)] = &[
        (
            "http1.1",
            b"GET /api/v1/users HTTP/1.1\r\nHost: example.com\r\n",
            DetectedProtocol::Http11,
        ),
        (
            "http2",
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
            DetectedProtocol::Http2,
        ),
        (
            "tcp",
            b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a",
            DetectedProtocol::Tcp,
        ),
        ("short", b"GET /", DetectedProtocol::Tcp),
        ("uppercase_no_space", b"GETTINGX", DetectedProtocol::Tcp),
        ("short_method", b"GE / HTTP/1.1", DetectedProtocol::Tcp),
        ("long_method", b"OVERLONG / HTTP/1.1", DetectedProtocol::Tcp),
        ("lowercase_method", b"get / HTTP/1.1", DetectedProtocol::Tcp),
    ];

    let mut group = c.benchmark_group("protocol_detection");
    group.throughput(criterion::Throughput::Elements(1));
    for &(name, input, expected) in cases {
        assert_eq!(
            ProtocolDetector::detect(input),
            expected,
            "invalid protocol fixture {name}"
        );
        group.bench_function(name, |b| {
            b.iter(|| black_box(ProtocolDetector::detect(black_box(input))))
        });
    }
    group.finish();
}

fn bench_http11_validation(c: &mut Criterion) {
    let cases: &[(&str, &[u8], bool)] = &[
        ("valid_1.1", b"GET /index.html HTTP/1.1\r", true),
        ("valid_1.0", b"HEAD / HTTP/1.0", true),
        ("missing_target", b"GET", false),
        ("missing_version", b"GET /index.html", false),
        ("invalid_method", b"get / HTTP/1.1", false),
        ("empty_target", b"GET  HTTP/1.1", false),
        ("unsupported_version", b"GET / HTTP/2.0", false),
    ];

    let mut group = c.benchmark_group("http11_validation");
    group.throughput(criterion::Throughput::Elements(1));
    for &(name, input, expected_valid) in cases {
        assert_eq!(
            validate_http11_request_line(input).is_ok(),
            expected_valid,
            "invalid HTTP/1.1 fixture {name}"
        );
        group.bench_function(name, |b| {
            b.iter(|| black_box(validate_http11_request_line(black_box(input))))
        });
    }
    group.finish();
}

fn bench_spiffe_id_parse(c: &mut Criterion) {
    let uri = "spiffe://cluster.local/ns/default/sa/payments";
    let id = SpiffeId::try_new("cluster.local", "default", "payments").unwrap();
    let parse_cases = [
        ("parse", uri, true),
        (
            "parse_uppercase",
            "SPIFFE://CLUSTER.LOCAL/ns/default/sa/payments",
            true,
        ),
        (
            "reject_userinfo",
            "spiffe://user@cluster.local/ns/default/sa/payments",
            false,
        ),
        (
            "reject_port",
            "spiffe://cluster.local:443/ns/default/sa/payments",
            false,
        ),
        (
            "reject_query",
            "spiffe://cluster.local/ns/default/sa/payments?x=1",
            false,
        ),
        (
            "reject_fragment",
            "spiffe://cluster.local/ns/default/sa/payments#x",
            false,
        ),
        (
            "reject_scheme",
            "https://cluster.local/ns/default/sa/payments",
            false,
        ),
        (
            "reject_path",
            "spiffe://cluster.local/default/payments",
            false,
        ),
        (
            "reject_empty",
            "spiffe://cluster.local/ns//sa/payments",
            false,
        ),
        ("reject_too_short", "spi", false),
        ("reject_missing_path", "spiffe://cluster.local", false),
        (
            "reject_missing_trust_domain",
            "spiffe:///ns/default/sa/payments",
            false,
        ),
        (
            "reject_missing_namespace_separator",
            "spiffe://cluster.local/ns/default",
            false,
        ),
        (
            "reject_namespace_query",
            "spiffe://cluster.local/ns/default?x=1",
            false,
        ),
        (
            "reject_missing_service_prefix",
            "spiffe://cluster.local/ns/default/account/payments",
            false,
        ),
    ];

    let mut group = c.benchmark_group("spiffe_id");
    group.throughput(criterion::Throughput::Elements(1));
    for (name, input, expected_valid) in parse_cases {
        assert_eq!(
            SpiffeId::from_uri(input).is_ok(),
            expected_valid,
            "invalid SPIFFE fixture {name}"
        );
        group.bench_function(name, |b| {
            b.iter(|| black_box(SpiffeId::from_uri(black_box(input))))
        });
    }

    group.bench_function("format", |b| b.iter(|| black_box(black_box(&id).to_uri())));
    group.bench_function("display", |b| {
        b.iter(|| black_box(format!("{}", black_box(&id))))
    });
    group.bench_function("from_str", |b| {
        b.iter(|| black_box(black_box(uri).parse::<SpiffeId>()))
    });
    group.bench_function("try_new", |b| {
        b.iter(|| {
            black_box(
                SpiffeId::try_new(
                    black_box("cluster.local"),
                    black_box("default"),
                    black_box("payments"),
                )
                .unwrap(),
            )
        })
    });
    for (name, trust_domain, namespace, service_account) in [
        ("reject_empty_trust_domain", "", "default", "payments"),
        ("reject_empty_namespace", "cluster.local", "", "payments"),
        (
            "reject_empty_service_account",
            "cluster.local",
            "default",
            "",
        ),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let result = SpiffeId::try_new(
                    black_box(trust_domain),
                    black_box(namespace),
                    black_box(service_account),
                );
                assert!(result.is_err());
                black_box(result)
            })
        });
    }
    group.bench_function("trust_domain_new", |b| {
        b.iter(|| black_box(TrustDomain::new(black_box("cluster.local"))))
    });
    group.bench_function("trust_domain_with_ca", |b| {
        b.iter(|| {
            black_box(
                TrustDomain::new(black_box("cluster.local")).with_ca(black_box(vec![1, 2, 3, 4])),
            )
        })
    });
    group.finish();
}

fn bench_memory_copy(c: &mut Criterion) {
    let data = vec![0xABu8; 16384];

    let mut group = c.benchmark_group("memory_copy");
    group.throughput(criterion::Throughput::Bytes(16384));

    let mut probe = vec![0u8; data.len()];
    probe.copy_from_slice(&data);
    assert_eq!(probe, data, "memory-copy fixture must preserve bytes");

    group.bench_function("copy_16kb", |b| {
        b.iter(|| {
            let mut dst = vec![0u8; 16384];
            dst.copy_from_slice(black_box(&data));
            black_box(dst);
        })
    });

    group.finish();
}

fn bench_policy_evaluation(c: &mut Criterion) {
    let namespace_engine = PolicyEngine::new();
    for i in 0..100u32 {
        let ns = format!("ns-{}", i);
        namespace_engine
            .add_namespace_rule(&ns, patterns::allow_same_namespace("cluster.local", &ns));
    }

    let source = SpiffeId::try_new("cluster.local", "ns-50", "svc").unwrap();
    let destination = SpiffeId::try_new("cluster.local", "ns-50", "other").unwrap();
    let default_destination = SpiffeId::try_new("cluster.local", "unconfigured", "other").unwrap();

    let global_engine = PolicyEngine::new();
    global_engine.set_global_policies(vec![patterns::allow(
        "spiffe://cluster.local/ns/ns-50/sa/svc",
        "spiffe://cluster.local/ns/unconfigured/sa/other",
        "global allow",
    )]);

    let namespace_then_global = PolicyEngine::new();
    namespace_then_global.add_namespace_rule(
        "unconfigured",
        patterns::allow(
            "spiffe://cluster.local/ns/other/sa/*",
            "spiffe://cluster.local/ns/unconfigured/sa/*",
            "nonmatching namespace rule",
        ),
    );
    namespace_then_global.set_global_policies(vec![patterns::allow(
        "spiffe://cluster.local/ns/ns-50/sa/svc",
        "spiffe://cluster.local/ns/unconfigured/sa/other",
        "global fallback",
    )]);

    let deny_engine = PolicyEngine::new();
    deny_engine.set_global_policies(vec![patterns::deny(
        "spiffe://cluster.local/ns/ns-50/sa/svc",
        "global deny",
    )]);
    let mut default_allow_engine = PolicyEngine::new();
    default_allow_engine.set_default_decision(Decision::Allow);

    assert!(matches!(
        namespace_engine.evaluate(&source, &destination),
        Decision::Allow
    ));
    assert!(matches!(
        global_engine.evaluate(&source, &default_destination),
        Decision::Allow
    ));
    assert!(matches!(
        namespace_engine.evaluate(&source, &default_destination),
        Decision::Deny(_)
    ));
    assert!(matches!(
        default_allow_engine.evaluate(&source, &default_destination),
        Decision::Allow
    ));
    assert!(matches!(
        namespace_then_global.evaluate(&source, &default_destination),
        Decision::Allow
    ));
    assert!(matches!(
        deny_engine.evaluate(&source, &default_destination),
        Decision::Deny(_)
    ));
    assert!(matches!(
        PolicyEngine::default().evaluate(&source, &default_destination),
        Decision::Deny(_)
    ));

    let mut group = c.benchmark_group("policy_engine");
    group.throughput(criterion::Throughput::Elements(1));
    group.bench_function("evaluate", |b| {
        b.iter(|| black_box(namespace_engine.evaluate(black_box(&source), black_box(&destination))))
    });
    group.bench_function("global_allow", |b| {
        b.iter(|| {
            black_box(global_engine.evaluate(black_box(&source), black_box(&default_destination)))
        })
    });
    group.bench_function("default_deny", |b| {
        b.iter(|| {
            black_box(
                namespace_engine.evaluate(black_box(&source), black_box(&default_destination)),
            )
        })
    });
    group.bench_function("default_allow", |b| {
        b.iter(|| {
            black_box(
                default_allow_engine.evaluate(black_box(&source), black_box(&default_destination)),
            )
        })
    });
    group.bench_function("namespace_miss_global_allow", |b| {
        b.iter(|| {
            black_box(
                namespace_then_global.evaluate(black_box(&source), black_box(&default_destination)),
            )
        })
    });
    group.bench_function("global_deny", |b| {
        b.iter(|| {
            black_box(deny_engine.evaluate(black_box(&source), black_box(&default_destination)))
        })
    });
    group.bench_function("default_constructor", |b| {
        b.iter(|| black_box(PolicyEngine::default()))
    });
    group.finish();
}

fn bench_compiled_pattern(c: &mut Criterion) {
    let id = SpiffeId::try_new("cluster.local", "default", "web-api").unwrap();
    let compiled = CompiledPattern::from_uri("spiffe://cluster.local/ns/default/sa/web*").unwrap();
    let namespace_miss =
        CompiledPattern::from_uri("spiffe://cluster.local/ns/other/sa/web*").unwrap();
    let account_miss =
        CompiledPattern::from_uri("spiffe://cluster.local/ns/default/sa/api*").unwrap();
    let trust_miss = CompiledPattern::from_uri("spiffe://other/ns/default/sa/web*").unwrap();
    let any = CompiledPattern::any();
    let string_pattern = "spiffe://cluster.local/ns/default/sa/web*";
    let malformed_string_pattern = "not-a-spiffe-pattern";

    assert!(id.matches_compiled(&compiled));
    assert!(id.matches_compiled(&any));
    assert!(!id.matches_compiled(&trust_miss));
    assert!(!id.matches_compiled(&namespace_miss));
    assert!(!id.matches_compiled(&account_miss));
    assert!(id.matches_pattern(string_pattern));
    assert!(!id.matches_pattern(malformed_string_pattern));

    let mut group = c.benchmark_group("pattern_match");
    group.throughput(criterion::Throughput::Elements(1));
    group.bench_function("compiled", |b| {
        b.iter(|| black_box(id.matches_compiled(black_box(&compiled))))
    });
    group.bench_function("any", |b| {
        b.iter(|| black_box(id.matches_compiled(black_box(&any))))
    });
    group.bench_function("trust_miss", |b| {
        b.iter(|| black_box(id.matches_compiled(black_box(&trust_miss))))
    });
    group.bench_function("namespace_miss", |b| {
        b.iter(|| black_box(id.matches_compiled(black_box(&namespace_miss))))
    });
    group.bench_function("account_miss", |b| {
        b.iter(|| black_box(id.matches_compiled(black_box(&account_miss))))
    });
    group.bench_function("string", |b| {
        b.iter(|| black_box(id.matches_pattern(black_box(string_pattern))))
    });
    group.bench_function("string_reject_malformed", |b| {
        b.iter(|| black_box(id.matches_pattern(black_box(malformed_string_pattern))))
    });
    group.bench_function("compile_exact", |b| {
        b.iter(|| {
            black_box(
                CompiledPattern::from_uri(black_box(
                    "spiffe://cluster.local/ns/default/sa/web-api",
                ))
                .unwrap(),
            )
        })
    });
    group.bench_function("compile_wildcard", |b| {
        b.iter(|| {
            black_box(
                CompiledPattern::from_uri(black_box("spiffe://cluster.local/ns/*/sa/web*"))
                    .unwrap(),
            )
        })
    });
    group.bench_function("compile_reject_malformed", |b| {
        b.iter(|| {
            let result = CompiledPattern::from_uri(black_box("spiffe://cluster.local/default/web"));
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.finish();
}

fn bench_segment_glob(c: &mut Criterion) {
    let cases = [
        ("match", "web*api", "web-foo-api", true),
        ("any", "*", "anything", true),
        ("exact", "web-api", "web-api", true),
        ("exact_miss", "web-api", "api", false),
        ("empty", "", "", true),
        ("empty_miss", "", "anything", false),
        ("prefix", "web*", "web-api", true),
        ("prefix_miss", "web*", "api-web", false),
        ("suffix", "*api", "api-api", true),
        ("suffix_miss", "*api", "api-web", false),
        ("contains", "*foo*", "web-foo-api", true),
        ("contains_miss", "*foo*", "web-api", false),
        ("prefix_suffix", "web*api", "web-service-api", true),
        ("prefix_suffix_miss", "web*api", "web-service", false),
        ("multi", "web*service*api", "web-foo-service-v1-api", true),
        ("multi_miss", "web*service*api", "web-api-service", false),
        (
            "multi_middle_miss",
            "web*service*api",
            "web-other-api",
            false,
        ),
        (
            "multi_prefix_miss",
            "web*service*api",
            "other-service-api",
            false,
        ),
        ("multi_unanchored", "*web*api*", "x-web-service-api-y", true),
        ("multi_prefix", "web*service*", "web-v1-service-extra", true),
        ("multi_suffix", "*web*api", "x-web-service-api", true),
        ("multi_overlap_miss", "abcd*x*bcde", "abcde", false),
    ];

    let mut group = c.benchmark_group("segment_glob");
    group.throughput(criterion::Throughput::Elements(1));
    for (name, pattern, value, expected) in cases {
        assert_eq!(
            SegmentGlob::new(pattern).matches(value),
            expected,
            "invalid segment-glob fixture {name}"
        );
        let glob = SegmentGlob::new(pattern);
        group.bench_function(name, move |b| {
            b.iter(|| black_box(glob.matches(black_box(value))))
        });
    }
    group.finish();
}

criterion_group!(
    name = benches;
    config = support::criterion();
    targets = bench_protocol_detection, bench_http11_validation, bench_spiffe_id_parse,
              bench_memory_copy, bench_policy_evaluation, bench_compiled_pattern,
              bench_segment_glob
);
criterion_main!(benches);
