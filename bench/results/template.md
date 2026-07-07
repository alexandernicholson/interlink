# Service Mesh Benchmark Results

This file is populated after running the benchmark harness.

## Raw outputs

- `results/interlink/` — interlink transparent-proxy results
- `results/linkerd/` — Linkerd sidecar results
- `results/istio-ambient/` — Istio ambient ztunnel results
- `results/local/` — local interlink mTLS-only results

## Comparison table

| Mesh | Profile | p50 ms | p90 ms | p99 ms | avg proxy CPU | peak proxy CPU | avg proxy memory | peak proxy memory | actual QPS | errors |
|------|---------|--------|--------|--------|---------------|----------------|------------------|-------------------|------------|--------|

## Notes

- Workload: Go echo server with 200 ms fixed delay, 1 KB payload.
- Load generator: Fortio.
- Duration per profile: 5 minutes after 30-second warm-up.
- CPU and memory sampled every 2 seconds via `kubectl top pod` (K8s) or `ps` (local).
