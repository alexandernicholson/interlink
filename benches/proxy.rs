use criterion::{black_box, criterion_group, criterion_main, Criterion};

// Benchmarks for the core proxy hot path.
//
// These measure operations that happen on every connection:
// 1. Protocol detection from raw bytes (first 32 bytes)
// 2. SPIFFE ID parsing from URI string
// 3. Buffer copy throughput (memcpy proxy-path)

fn bench_protocol_detection(c: &mut Criterion) {
    let http11 = b"GET /api/v1/users HTTP/1.1\r\nHost: example.com\r\n";
    let http2 = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    let tcp = b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a";

    let mut group = c.benchmark_group("protocol_detection");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("http1.1", |b| {
        b.iter(|| black_box(interlink::protocol::ProtocolDetector::detect(http11)))
    });

    group.bench_function("http2", |b| {
        b.iter(|| interlink::protocol::ProtocolDetector::detect(black_box(http2)))
    });

    group.bench_function("tcp", |b| {
        b.iter(|| interlink::protocol::ProtocolDetector::detect(black_box(tcp)))
    });

    group.finish();
}

fn bench_spiffe_id_parse(c: &mut Criterion) {
    let uri = "spiffe://cluster.local/ns/default/sa/payments";

    let mut group = c.benchmark_group("spiffe_id");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("parse", |b| {
        b.iter(|| {
            let id = interlink::common::identity::SpiffeId::from_uri(black_box(uri)).unwrap();
            black_box(id);
        })
    });

    group.bench_function("format", |b| {
        let id = interlink::common::identity::SpiffeId::new("cluster.local", "default", "payments");
        b.iter(|| {
            let s = black_box(&id).to_uri();
            black_box(s);
        })
    });

    group.finish();
}

fn bench_memory_copy(c: &mut Criterion) {
    let data = vec![0xABu8; 16384]; // 16 KB buffer

    let mut group = c.benchmark_group("memory_copy");
    group.throughput(criterion::Throughput::Bytes(16384));

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
    use interlink::common::identity::SpiffeId;
    use interlink::policy::{patterns, PolicyEngine};

    let engine = PolicyEngine::new();
    for i in 0..100u32 {
        let ns = format!("ns-{}", i);
        engine.add_namespace_rule(&ns, patterns::allow_same_namespace("cluster.local", &ns));
    }

    let src = SpiffeId::new("cluster.local", "ns-50", "svc");
    let dst = SpiffeId::new("cluster.local", "ns-50", "other");

    let mut group = c.benchmark_group("policy_engine");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("evaluate", |b| {
        b.iter(|| {
            let decision = engine.evaluate(black_box(&src), black_box(&dst));
            black_box(decision);
        })
    });

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .significance_level(0.01)
        .nresamples(100_000)
        .warm_up_time(std::time::Duration::from_millis(500))
        .measurement_time(std::time::Duration::from_secs(3));
    targets = bench_protocol_detection, bench_spiffe_id_parse, bench_memory_copy, bench_policy_evaluation
);
criterion_main!(benches);
