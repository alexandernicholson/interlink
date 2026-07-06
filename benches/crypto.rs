use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ring::signature::KeyPair;

fn bench_ring_ed25519_sign(c: &mut Criterion) {
    let rng = ring::rand::SystemRandom::new();
    let seed: [u8; 32] = ring::rand::generate(&rng).unwrap().expose();
    let key = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
    let msg = b"benchmark message for ed25519 signing";

    let mut group = c.benchmark_group("crypto_ed25519");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("sign", |b| {
        b.iter(|| {
            let sig = key.sign(black_box(msg));
            black_box(sig);
        })
    });

    group.finish();
}

fn bench_ring_ed25519_verify(c: &mut Criterion) {
    let rng = ring::rand::SystemRandom::new();
    let seed: [u8; 32] = ring::rand::generate(&rng).unwrap().expose();
    let key = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
    let msg = b"benchmark message for ed25519 verification";
    let sig = key.sign(msg);
    let public_key = ring::signature::UnparsedPublicKey::new(
        &ring::signature::ED25519,
        key.public_key().as_ref(),
    );

    let mut group = c.benchmark_group("crypto_ed25519");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("verify", |b| {
        b.iter(|| {
            let result = public_key.verify(black_box(msg), black_box(sig.as_ref()));
            let _ = black_box(result);
        })
    });

    group.finish();
}

fn bench_x25519_keygen(c: &mut Criterion) {
    let rng = ring::rand::SystemRandom::new();

    let mut group = c.benchmark_group("crypto_x25519");
    group.throughput(criterion::Throughput::Elements(1));

    group.bench_function("keygen", |b| {
        b.iter(|| {
            let private =
                ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::X25519, &rng)
                    .unwrap();
            let public = private.compute_public_key().unwrap();
            black_box((private, public));
        })
    });

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .warm_up_time(std::time::Duration::from_millis(500))
        .measurement_time(std::time::Duration::from_secs(3));
    targets = bench_ring_ed25519_sign, bench_ring_ed25519_verify, bench_x25519_keygen
);
criterion_main!(benches);
