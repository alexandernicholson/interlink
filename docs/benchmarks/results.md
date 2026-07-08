# Benchmark Results

Platform: Linux x86_64, 16 cores (measured 2026-07-08)  
Build: `cargo bench --bench proxy` / `cargo bench --bench crypto`

<hr />

## Protocol Detection

| Case | Time |
|------|------|
| HTTP/1.1 (GET) | 0.84 ns |
| HTTP/2 preface | 1.79 ns |
| Raw TCP (fallback) | 1.62 ns |

<hr />

## SPIFFE ID

| Operation | Time |
|-----------|------|
| Parse URI (validated, `SpiffeId::try_new` path) | 174 ns |
| Format URI | 60.3 ns |

Parse includes full component validation — this is the B13 validated constructor, the
only way to build a `SpiffeId`; there is no cheaper unvalidated parse to benchmark.

<hr />

## Memory Copy

| Size | Time | Throughput |
|------|------|------------|
| 16 KB buffer | 134 ns | ~122 GB/s |

<hr />

## Ed25519 Crypto

| Operation | Time | Throughput |
|-----------|------|------------|
| Sign | 13.7 µs | ~73K signs/sec |
| Verify | 25.8 µs | ~39K verifies/sec |

<hr />

## X25519 Key Exchange

| Operation | Time | Throughput |
|-----------|------|------------|
| Key generation | 13.2 µs | ~76K keys/sec |

<hr />

## Policy Engine

| Test | Time |
|------|------|
| `policy_engine/evaluate` (compiled patterns) | 40.6 ns |
| `pattern_match/compiled` | 25.4 ns |
| `pattern_match/string` (uncompiled fallback) | 216 ns |
| `segment_glob/match` | 19.3 ns |

The original string-matched engine evaluated at 8.2 µs per decision with 100 rules;
compiled patterns replaced it (~200× faster on the hot path).

<hr />

## Service Mesh Comparison

A reproducible multi-mesh benchmark harness lives in [`bench/`](../../bench/). The
published, validity-gated apples-to-apples comparison (all three meshes deployed the
same way on the same 3-node kind topology, interlink as a per-pod sidecar) is
[`bench/results/comparison.md`](../../bench/results/comparison.md); methodology and
history in [`lore/benchmark-status.md`](../../lore/benchmark-status.md).

Measured 2026-07-08 (60 s/profile, 9/9 profiles valid, zero errors):

| Mesh | Profile | p50 ms | p99 ms | Proxy CPU (avg m) | Proxy memory (avg MB) |
|------|---------|--------|--------|-------------------|-----------------------|
| **interlink** | 320 rps | 202.30 | 204.03 | 15.4 | 21.3 |
| **interlink** | 800 rps | 203.97 | 207.34 | 31.8 | 43.7 |
| **interlink** | 1600 rps | 205.15 | 209.87 | 47.7 | 94.1 |
| **Linkerd** | 320 rps | 204.96 | 208.61 | 25.3 | 13.9 |
| **Linkerd** | 800 rps | 209.72 | 218.05 | 67.7 | 26.3 |
| **Linkerd** | 1600 rps | 213.60 | 225.81 | 121.8 | 42.0 |
| **Istio ambient** | 320 rps | 207.37 | 214.04 | 28.1 | 8.3 |
| **Istio ambient** | 800 rps | 204.79 | 209.12 | 56.4 | 11.8 |
| **Istio ambient** | 1600 rps | 207.26 | 213.89 | 92.4 | 18.7 |

interlink adds the least latency overhead (+4.0/+7.3/+9.9 ms p99 above the 200 ms base)
and the least CPU; its higher memory is an allocator high-water effect of 64 KiB relay
buffers under connection churn, not live state (root cause in
`lore/benchmark-status.md`). An earlier table on this page compared interlink measured
*locally* against in-cluster Linkerd/Istio; it was not apples-to-apples and has been
replaced by the gated in-cluster comparison above.
