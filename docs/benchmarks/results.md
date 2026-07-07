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

A reproducible multi-mesh benchmark harness lives in [`bench/`](../../bench/). All three meshes were measured on identical workloads (Go echo server, 200 ms fixed delay, 1 KB payload, Fortio load generator). Linkerd and Istio ambient on dedicated kind clusters; interlink locally. Full details in [`bench/results/comparison.md`](../../bench/results/comparison.md).

| Mesh | Profile | Actual RPS | p50 ms | p90 ms | p99 ms | Proxy CPU (avg) | Proxy memory (avg) |
|------|---------|-----------|--------|--------|--------|-----------------|-------------------|
| **interlink** | light 320 | 318.9 | 201.6 | 202.7 | 202.9 | 0.7 %\* | 11.6 MB |
| **interlink** | medium 3,200 | 3,188.9 | 204.7 | 208.4 | 209.2 | 1.6 %\* | 42.2 MB |
| **interlink** | heavy 12,800 | 12,749.0 | 212.8 | 222.8 | 225.1 | 5.0 %\* | 149.8 MB |
| **Linkerd** | light 320 | 319.8 | 207.2 | 212.4 | 213.5 | 38.3 m | 35.5 MB |
| **Linkerd** | medium 3,200 | 3,196.7 | 259.4 | 291.9 | 299.2 | 352.0 m | 237.2 MB |
| **Linkerd** | heavy 12,800 | 11,868.6 | 550.7 | 652.0 | 695.5 | 1,373.5 m | 887.5 MB |
| **Istio ambient** | light 320 | 319.8 | 204.9 | 208.3 | 209.1 | 16.3 m | 12.0 MB |
| **Istio ambient** | medium 3,200 | 3,197.5 | 225.3 | 245.2 | 249.7 | 127.9 m | 81.0 MB |
| **Istio ambient** | heavy 12,800 | 12,786.6 | 265.4 | 319.4 | 347.2 | 487.9 m | 235.6 MB |

\* interlink CPU is % of one core on host; Linkerd and Istio CPU in millicores in Kubernetes.

Proxy overhead (latency above 200 ms base):

| Mesh | Light overhead | Medium overhead | Heavy overhead |
|------|---------------|----------------|----------------|
| interlink | +2.9 ms | +9.2 ms | +25.1 ms |
| Linkerd | +13.5 ms | +99.2 ms | +495.5 ms |
| Istio ambient | +9.1 ms | +49.7 ms | +147.2 ms |

Interlink adds the least latency overhead and consumes the fewest resources. Istio ambient (ztunnel) is roughly 3× more efficient than Linkerd. Linkerd struggles at high connection counts (12,800 RPS / 6,400 conns), losing 7 % throughput.
