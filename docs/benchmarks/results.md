# Benchmark Results

Platform: Apple M3 Pro, 12 cores, 36 GB RAM  
Rust: 1.85+  
Build: `cargo bench`

<hr />

## Protocol Detection

| Case | Time | Throughput |
|------|------|------------|
| HTTP/1.1 (GET) | 12.5 ns | 80M detections/sec |
| HTTP/2 preface | 4.2 ns | 238M detections/sec |
| Raw TCP (fallback) | 5.9 ns | 169M detections/sec |

<hr />

## SPIFFE ID

| Operation | Time | Throughput |
|-----------|------|------------|
| Parse URI | 48.9 ns | 20M parses/sec |
| Format URI | 42.1 ns | 24M formats/sec |

<hr />

## Memory Copy

| Size | Time | Throughput |
|------|------|------------|
| 16 KB buffer | 183 ns | 87 GB/s |

<hr />

## Ed25519 Crypto

| Operation | Time | Throughput |
|-----------|------|------------|
| Sign | 12.4 µs | 80K signs/sec |
| Verify | 28.1 µs | 35K verifies/sec |

<hr />

## X25519 Key Exchange

| Operation | Time | Throughput |
|-----------|------|------------|
| Key generation | 15.2 µs | 65K keys/sec |

<hr />

## Policy Engine

| Test | Time |
|------|------|
| 100 rules, 10K evaluations | 8.2 µs per evaluation (avg) |

<hr />

## Service Mesh Comparison

For a reproducible multi-mesh benchmark harness, see [`bench/`](../../bench/). The first set of interlink numbers below were produced locally with Fortio over HTTPS + mTLS (1 KB payload, 200 ms fixed delay). Full details are in [`bench/results/local.md`](../../bench/results/local.md).

| Profile | RPS | Connections | p99 latency | peak CPU | peak memory |
|---------|-----|-------------|-------------|----------|-------------|
| light   | 320 | 160         | 202.9 ms    | 1.4 %    | 12.3 MB     |
| medium  | 3,200 | 1,600     | 209.2 ms    | 2.2 %    | 47.7 MB     |
| heavy   | 12,800 | 6,400    | 225.1 ms    | 6.6 %    | 156.6 MB    |

Comparable published mesh figures:

| Mesh | Proxy memory at moderate load | Proxy CPU at 1,000 RPS |
|------|------------------------------|------------------------|
| **interlink** (measured) | ~43 MB @ 3,200 RPS | <3 % @ 3,200 RPS |
| Linkerd (Buoyant real-world) | 50–180 MB typical | 35–250 mCPU |
| Istio sidecar (Istio docs) | ~60 MB | ~0.20 vCPU |
| Istio ambient ztunnel | ~12 MB | ~0.06 vCPU |

The Linkerd and Istio numbers are from upstream documentation, not yet from our harness. Head-to-head Kubernetes results will be added once the `bench/kubernetes/` workflow is executed in a cluster with Linkerd and Istio ambient installed.
