# interlink Local Benchmark Results

> **Note:** Historical capture: **2026-07-07**, direct `TlsServer` HTTP echo
> with a **200 ms application delay**, not a full proxy pair or native-mux
> measurement. Numbers below are preserved from that capture.
> Read the [latest native Veyrmux comparison](../../docs/benchmarks/results.md)
> and its [raw comparison data](veyrmux-integration/comparison.json) for the
> matched runtime A/B.

Captured with `bench/local/run.sh` and `BENCH_DURATION=60`. The local scripts
now use native Fortio; the historical containerized topology is not reproduced
by running today's script.

## Environment

| | |
|---|---|
| Date | 2026-07-07 |
| Host | Linux x86_64, 16 logical cores |
| Server | `examples/bench_server` (interlink `TlsServer`) |
| Load generator | Fortio 1.66.0 via Docker |
| Protocol | HTTPS + mutual TLS |
| Payload | 1 KB request, 1 KB response |
| Artificial delay | 200 ms per request |

## Results

| Profile | Target RPS | Connections | Actual RPS | p50 ms | p90 ms | p99 ms | Avg CPU % | Peak CPU % | Avg RSS MB | Peak RSS MB | Errors |
|---------|------------|-------------|------------|--------|--------|--------|-----------|------------|------------|-------------|--------|
| light   | 320        | 160         | 318.9      | 201.64 | 202.66 | 202.89 | 0.70      | 1.40       | 11.6       | 12.3        | 0      |
| medium  | 3,200      | 1,600       | 3,188.9    | 204.74 | 208.41 | 209.23 | 1.59      | 2.20       | 42.2       | 47.7        | 0      |
| heavy   | 12,800     | 6,400       | 12,749.0   | 212.76 | 222.80 | 225.06 | 5.01      | 6.60       | 149.8      | 156.6       | 0      |

## Interpretation

- **Latency**: The 200 ms fixed delay dominates. The observed HTTP+mTLS server
  latency above the base delay was roughly (not an isolated proxy-overhead measurement):
  - light: ~3 ms at p99
  - medium: ~9 ms at p99
  - heavy: ~25 ms at p99 (queueing under high connection count)
- **CPU**: Very low because the workload is delay-bound, not compute-bound. Peak CPU is <7 % even at 12,800 RPS.
- **Memory**: Scales roughly linearly with concurrent connections:
  - ~70 KB per connection at medium load
  - ~24 KB per connection at heavy load (amortized)
- **Throughput**: Achieved >99 % of target RPS in all profiles.

## Reproduce

For a new direct-server run with 60 measured seconds per profile:

```bash
BENCH_DURATION=60 ./bench/local/run.sh
```

Run this from the repository root. It creates a new native-server/Fortio dataset,
not a reproduction of the July capture. The measured default remains 300 seconds,
plus a discarded 30-second warm-up per profile.

See the [local benchmark guide](../local/README.md) for current prerequisites,
output names, and the known high-concurrency listener-backlog limitation.
For the native cutover, follow the [matched A/B requirements](../README.md#run-the-current-runtime-lifecycle-cases);
a single `cargo bench` invocation measures only the checked-out runtime.
