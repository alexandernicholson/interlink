# Benchmark Results

Use the current native-runtime comparison to evaluate Veyrmux against stock Yamux.
The primitive and multi-mesh HTTP results below are historical, not native-runtime recaptures.

## Native Veyrmux versus stock Yamux (2026-11-17)

Two actual interlink proxies and a TCP echo service run in one benchmark process:

```mermaid
flowchart LR
    App[Application connection] --> Outbound[Outbound proxy]
    Outbound -->|mTLS: mux or legacy| Inbound[Inbound proxy]
    Inbound --> Echo[TCP echo service]
```

Each timed transaction opens a new application TCP connection, writes the payload,
half-closes, validates the exact echo, and requires peer EOF within one second.
Fixture setup and teardown are excluded.

| Path | Payload each direction | Stock Yamux, µs | Native Veyrmux, µs | Latency change |
| --- | ---: | ---: | ---: | ---: |
| Mux | 35 B | 253.7203 | 239.0405 | −5.7858% |
| Mux | 16 KiB | 268.7389 | 238.9947 | −11.0681% |
| Legacy control | 35 B | 487.5739 | 490.8302 | +0.6678% |
| Legacy control | 16 KiB | 506.6082 | 508.9794 | +0.4681% |

Legacy controls use neither multiplexer. Values are medians of four independent
block medians per arm, not request percentiles.

### Matched conditions and provenance

- **Host:** Linux/x86-64, eight vCPUs, 3.3 GiB RAM.
- **Build:** Rust 1.100, full LTO, one codegen unit, mimalloc.
- **Runtimes:** stock Yamux `f3ccc5781dab` versus native Veyrmux `bbdf99a9ac40`;
  Veyrmux pin `6a3e9f66fb896cc56dc60696d14f774657f1cd04`.
- **Harness:** identical sources from `61f196597b802afb2259d70598cd0d94337f1fc5`;
  SHA-256 `8a41dde42962b3daa5dfb92b179b869ebdf01d6ac4cad24be22e1cdff6c0d8b6`.
- **Sampling:** fixed ABBAABBA order; 20 samples per block, 100 ms warm-up,
  500 ms measurement. Per-block 95% bootstrap intervals are retained; no aggregate interval is claimed.
- **Isolation:** fresh loopback-only network namespace per block, unprivileged
  benchmark, no global network tuning.

Listener-drop, SYN-retransmission, and kernel-timeout deltas were zero. One data
retransmission in baseline block one is retained.
Shared-namespace attempts reached conntrack saturation at 65,536/65,536 entries;
those attempts, all pilots, and prior generations are excluded.

This measures **serialized loopback connection-lifecycle latency**, not maximum
throughput, request p99, cross-host latency, NAT overhead, or persistent-connection performance.
The earlier −63% proxy CPU result compared multiplexing on versus off; it is not a Veyrmux-versus-Yamux gain.

See the [raw numeric samples, block estimates, source/binary hashes, and methodology](../../bench/results/veyrmux-integration/comparison.json),
[behavioral/build validation](../../bench/results/veyrmux-integration/validation.json),
and [native-integration verification notes](../performance-plan.md#native-veyrmux-integration-2026-11-17).

## Historical primitive benchmarks (2026-07-08)

Platform: Linux x86_64, 16 cores. These measurements predate native Veyrmux integration;
they have not been recaptured for the current runtime.  
Build: `cargo bench --bench proxy` / `cargo bench --bench crypto`

<hr />

### Protocol Detection

| Case | Time |
|------|------|
| HTTP/1.1 (GET) | 0.84 ns |
| HTTP/2 preface | 1.79 ns |
| Raw TCP (fallback) | 1.62 ns |

<hr />

### SPIFFE ID

| Operation | Time |
|-----------|------|
| Parse URI (validated, `SpiffeId::try_new` path) | 174 ns |
| Format URI | 60.3 ns |

Parse includes full component validation — this is the B13 validated constructor, the
only way to build a `SpiffeId`; there is no cheaper unvalidated parse to benchmark.

<hr />

### Memory Copy

| Size | Time | Throughput |
|------|------|------------|
| 16 KB buffer | 134 ns | ~122 GB/s |

<hr />

### Ed25519 Crypto

| Operation | Time | Throughput |
|-----------|------|------------|
| Sign | 13.7 µs | ~73K signs/sec |
| Verify | 25.8 µs | ~39K verifies/sec |

<hr />

### X25519 Key Exchange

| Operation | Time | Throughput |
|-----------|------|------------|
| Key generation | 13.2 µs | ~76K keys/sec |

<hr />

### Policy Engine

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

**Historical — measured 2026-07-08.** These HTTP results are not measurements of
the native Veyrmux runtime. No new Linkerd or Istio measurements are published here.

The [`bench/` harness](../../bench/) used the same 3-node kind topology and workload
in a separate cluster per mesh. interlink and Linkerd used per-pod sidecars;
Istio ambient used node-level ztunnel. These are different deployment architectures,
not identical deployments.

The [generated comparison](../../bench/results/comparison.md) retains raw-input
links and validity gates; [methodology notes](../../lore/benchmark-status.md)
preserve the research history. Each profile ran for 60 seconds; all nine profiles
passed the gates with zero errors:

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

In this historical run, interlink had the lowest p99 overhead
(+4.0/+7.3/+9.9 ms above the 200 ms base) and proxy CPU at these loads.
Its higher memory was traced to allocator high-water retention of 64 KiB relay
buffers under connection churn, not live state; see the
[dated investigation](../../lore/benchmark-status.md#memory-root-cause-2026-07-08-churn-ratcheted-copy-buffers-not-live-state).
The earlier local-interlink versus in-cluster Linkerd/Istio table was replaced
because its conditions did not match.
