# Performance workstream

Maintain one reproducible Interlink/Linkerd/Istio ambient comparison:
[`bench/results/comparison.md`](../bench/results/comparison.md), with a generated
three-row [README view](../README.md#benchmarks) of the same capture.
The [benchmark guide](../bench/README.md) declares the experiment and publication contract.

## Current scope

- Fresh Linux echo capacity testing with zero application delay and unlimited
  Fortio offered load, not the old fixed-RPS/200 ms workload.
- Predeclared concurrency sweeps: 1 KiB request/response at 1, 4, 16, 64, 256, 1024,
  4096 connections; 1 MiB request/response at 1, 4, 16, 64, 256, 1024 connections.
- Fortio fast client for 1 KiB RPS; full-duplex `-stdclient` for 1 MiB bulk on
  every mesh. Require the corresponding raw `DisableFastClient` value and validate
  total-minus-header versus body-only response accounting, respectively.
- Separate 10-second warm-up and default 20-second measurement at every point,
  retaining actual elapsed duration, all points and request-error boundaries.
- The same five-second request timeout on every mesh, retained in metadata and
  checked against actual Fortio output. Timeouts remain ineligible request errors.
- The same three-node kind topology for all meshes; state sidecar/ambient differences.
- Publish peak measured error-free RPS and actual echoed payload MiB/s each direction,
  never doubled wire throughput. Report p99 and proxy CPU/RSS at the RPS peak.
- Schema-2 provenance records actual runtime source, time, Linux machine, versions,
  complete grids and selected concurrency. A separate relative-path `harness_sha256`
  map pins actual harness/workload/manifests and Docker/Cargo inputs even when the
  experiment is committed after the runtime revision. Never relabel old shaped data.

Updating documentation or the harness does not establish a measured capacity result.
Existing data stays intact until a fresh complete capture replaces it.

## Required validation

1. Confirm traffic traverses each configured mesh and reaches the zero-delay echo service.
2. Require complete raw Fortio, timestamped actual RSS/proxy CPU, and host CPU evidence
   for every declared mesh/workload/concurrency. Exclude warm-up from measured metrics.
3. Validate unlimited offered load, actual duration, payload digest/length, response
   counts and actual echoed response bytes. Missing, stale or malformed data is fatal,
   not an error boundary or a fabricated zero.
4. Retain measured and warm-up request errors. Only points with zero errors in both
   phases can win; every mesh/workload needs an eligible point. Verify stored winning
   concurrency against the full raw grid.
5. Host CPU saturation is recorded, not rejected: fixed-target achievement and the
   former 85% host gate do not describe unlimited-load capacity testing.
6. Stage both generated views until validation and source guards pass. Preserve the
   previous dataset and views on collection, validation or publication failure.
7. Verify both views in preflight; publish them with the dataset in one ordinary
   fast-forward commit. Never rebase stale evidence or force push.
8. Review new measured numbers and explain deltas; a successful capture alone does
   not prove improvement. If the largest tested concurrency wins, label the tested
   range limit rather than claiming an absolute machine ceiling.

`bench/scripts/aggregate.py` is the sole reporter. `bench/tests/test_report.py`
contains deterministic value/selection, boundary eligibility, complete-grid,
throughput/response-integrity, marker, stale-view, preservation and live-publication
regressions. Run them and the repository preflight before publication.
The validated 2026-11-17 Linux capture contains all 39 declared points; all six
selected peaks are inside the tested grids. Both generated views use that capture.
It replaces the old 1,600-target-RPS/200 ms workload with zero-delay capacity testing.
The workloads are not comparable, so these changed numbers establish no before/after
speedup; use the canonical table's machine, source and limits.

Internal `benches/`, `scripts/verify-microbench-paths.py`, and `scripts/preflight.sh`
remain correctness checks with locked semantic coverage gates, not additional public
performance comparisons. Hot-path changes still need authentication, authorization,
byte-integrity, half-close, cancellation and concurrency tests under the
[engineering rules](engineering-rules.md), especially A1–A11, C6 and D4–D10.

## Limits

These sweeps measure peak observed echo RPS and payload throughput within one Linux
machine's declared range, not theoretical capacity or performance on production
networks and independent machines. They do not establish connection-churn or handshake
capacity. Sampled proxy RSS is not a measurement of live per-connection allocation.
Use [production tuning](production-tuning.md) with deployment-specific traffic.

