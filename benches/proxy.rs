use criterion::{black_box, criterion_group, criterion_main, Criterion};

use interlink::common::identity::{CompiledPattern, SegmentGlob, SpiffeId};
use interlink::policy::{patterns, PolicyEngine};

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
        let id =
            interlink::common::identity::SpiffeId::try_new("cluster.local", "default", "payments")
                .unwrap();
        b.iter(|| {
            let s = black_box(&id).to_uri();
            black_box(s);
        })
    });

    group.finish();
}

fn bench_memory_copy(c: &mut Criterion) {
    let data = vec![0xABu8; 16384];

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
    let engine = PolicyEngine::new();
    for i in 0..100u32 {
        let ns = format!("ns-{}", i);
        engine.add_namespace_rule(&ns, patterns::allow_same_namespace("cluster.local", &ns));
    }

    let src = SpiffeId::try_new("cluster.local", "ns-50", "svc").unwrap();
    let dst = SpiffeId::try_new("cluster.local", "ns-50", "other").unwrap();

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

fn bench_compiled_pattern(c: &mut Criterion) {
    let id = SpiffeId::try_new("cluster.local", "default", "web-api").unwrap();
    let compiled = CompiledPattern::from_uri("spiffe://cluster.local/ns/default/sa/web*").unwrap();
    let string_pattern = "spiffe://cluster.local/ns/default/sa/web*";

    let mut group = c.benchmark_group("pattern_match");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("compiled", |b| {
        b.iter(|| black_box(id.matches_compiled(black_box(&compiled))))
    });

    group.bench_function("string", |b| {
        b.iter(|| black_box(id.matches_pattern(black_box(string_pattern))))
    });

    group.finish();
}

fn bench_segment_glob(c: &mut Criterion) {
    let glob = SegmentGlob::new("web*api");
    let value = "web-foo-api";

    let mut group = c.benchmark_group("segment_glob");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("match", |b| {
        b.iter(|| black_box(glob.matches(black_box(value))))
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
    targets = bench_protocol_detection, bench_spiffe_id_parse, bench_memory_copy,
              bench_policy_evaluation, bench_compiled_pattern, bench_segment_glob
);
criterion_main!(benches);
