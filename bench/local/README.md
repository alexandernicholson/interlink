# Local interlink benchmark

This directory contains a Docker-based benchmark for interlink that does **not** require Kubernetes. It measures the core mTLS data-plane performance by running:

- `examples/bench_server` — an mTLS echo server built with interlink's `TlsServer`
- `fortio` — load generator with mTLS client certificates

## Prerequisites

- Docker
- Rust toolchain (to build the benchmark server)
- `cargo` in PATH

## Run

```bash
cd bench/local
./run.sh
```

## Output

- `results/fortio-*.json` — raw Fortio results
- `results/proxy-metrics.csv` — CPU/memory samples of the server process
- `results/summary.md` — aggregated numbers

## Profiles

| RPS | Connections | Duration | Payload |
|-----|-------------|----------|---------|
| 320 | 160 | 5 min | 1 KB |
| 3,200 | 1,600 | 5 min | 1 KB |
| 12,800 | 6,400 | 5 min | 1 KB |

The server adds a fixed 200 ms processing delay per request so latency differences are visible above the base work.
