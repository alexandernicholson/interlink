# Performance workstream

Maintain the [mesh comparison](../bench/results/comparison.md) and its
[README graph](../README.md#benchmarks).
Follow the [benchmark guide](../bench/README.md) to collect or regenerate results.

## Scope and status

- Measured capacity: 39 Linux echo points across interlink, Linkerd, and Istio ambient,
  using zero application delay, 1 KiB RPS and 1 MiB bulk workloads.
- Measured comparison: capacity curves plus a separate, error-free
  10,000 requests/s control at 64 connections for every mesh.
  Compare latency and resources at this shared load, not each mesh's peak.
- Reuse the existing capacity sweeps when adding controls on the same host and runtime.
  Do not interpolate measurements or relabel older delayed workloads.
- Run captures on the Linux benchmark host, never in CI. CI validates stored results.
  Keep raw data and regenerate `bench/results/comparison.md`,
  `bench/results/comparison.svg`, and the README together.

## Contributor requirements

Follow the binding [engineering rules](engineering-rules.md), especially A4,
A10–A11, C6, and D4–D10. Report only measured improvements and explain changed numbers.
`bench/scripts/aggregate.py` owns reporting; `bench/tests/test_report.py`
covers selection, shared-load eligibility, and publication safety.
Run repository preflight before committing.

Host-specific echo capacity does not establish production sizing, handshake capacity,
or live per-connection allocation. Use [production tuning](production-tuning.md)
for deployment-specific measurements.
