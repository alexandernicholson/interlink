# Performance workstream

Maintain the [mesh comparison](../bench/results/comparison.md) and its
[README graph](../README.md#benchmarks).
Follow the [benchmark guide](../bench/README.md) to collect or regenerate results.

## Scope and status

- Measured capacity: 39 Linux echo points across interlink, Linkerd, and Istio ambient,
  using zero application delay, 1 KiB RPS and 1 MiB bulk workloads.
- Measured comparison: capacity curves plus a separate, error-free
  10,000 requests/s control at 64 connections for every mesh.
  Compare latency, CPU and anonymous RSS at this shared load, not each mesh's peak.
- The fastest mesh's request-rate curve ends where four times the clients add under 5%
  throughput: past that point the shared host, not the mesh, limits the measurement.
  Slower meshes plateau below a rate the host sustained, so their curves stay complete.
- Reuse the existing capacity sweeps when adding controls on the same host and runtime.
  Do not interpolate measurements or relabel older delayed workloads.
- Run captures on the Linux benchmark host, never in CI. CI validates stored results.
  Keep raw data and regenerate `bench/results/comparison.md`,
  `bench/results/comparison.svg`, and the README together.

## Proxy efficiency

Native A/B on the benchmark host: fortio → outbound → mux/mTLS → inbound → echo
over loopback, fresh processes per 10-second point, previous binary as control.

| Change | Status | Measured effect |
| --- | --- | --- |
| mimalloc without huge-page advice | ✓ | Anonymous RSS per proxy at 10k RPS: 44 → 6.0 MiB |
| Two Tokio workers by default | ✓ | At 10k RPS: CPU per request −14%, RSS 6.0 → 3.2 MiB; 1 MiB bulk at 64 clients +31% |
| Tunnel pool sized by `INTERLINK_MAX_CONNECTIONS` | ✓ | 4,096 clients: ~400k errors per 10 s → 0 |
| One busy tunnel per worker | ✓ | 1 MiB bulk at 16/64 clients: +49% / +46% |
| Flush-on-queue for mux relays | Rejected | +4.5% RPS only at 1,024 clients; changes flush semantics |

The kind comparison was recaptured at `356ac80` (previous capture: `205d05b`).
Interlink changes, with Linkerd and Istio ambient as same-run noise references
(their throughput moved by up to 8% outside overload):

| Interlink point | Before | After |
| --- | ---: | ---: |
| 10k RPS control: CPU / anonymous RSS / p99 | 0.501 cores / 22.2 MiB / 2.82 ms | 0.445 cores / 5.9 MiB / 2.83 ms |
| 1 KiB peak RPS (1,024 clients) | 69.9k | 75.5k |
| 4,096 clients | 53.3k RPS, 347k errors | 75.3k RPS, 0 errors |
| 1 MiB bulk at 16 / 64 / 256 clients | 1,864 / 2,273 / 1,558 MiB/s | 2,182 / 2,104 / 1,990 MiB/s |

Regressions to watch: 1 KiB at one client fell 7% (4,837 → 4,505 RPS), not seen
in the native A/B; bulk at 64 clients fell 7%, likely because two workers cap each
proxy at two cores (the old run used 4.39 cores across both proxies, the new 3.59).

## Contributor requirements

Follow the binding [engineering rules](engineering-rules.md), especially A4,
A10–A11, C6, and D4–D10. Report only measured improvements and explain changed numbers.
`bench/scripts/aggregate.py` owns reporting; `bench/tests/test_report.py`
covers selection, shared-load eligibility, and publication safety.
Run repository preflight before committing.

Host-specific echo capacity does not establish production sizing, handshake capacity,
or live per-connection allocation. Use [production tuning](production-tuning.md)
for deployment-specific measurements.
