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
