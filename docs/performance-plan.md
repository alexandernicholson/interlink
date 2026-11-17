# Performance workstream

The current workstream maintains one reproducible service-mesh comparison.
Use [`bench/results/comparison.md`](../bench/results/comparison.md) for published
measurements and the [benchmark guide](../bench/README.md) to collect a new dataset.
This note records validation requirements, not additional benchmark results.

## Current scope

- Compare interlink, Linkerd, and Istio ambient with the same declared HTTP load
  profiles, payload, application delay, and measured duration.
- Use three-node kind clusters with the load generator and application on separate
  workers. Document sidecar and ambient architecture differences.
- Publish one complete capture with its actual timestamps, source revision, tool
  versions, Fortio output, and proxy and host metrics.
- Replace the published dataset only after validation succeeds. Never combine
  profiles from different captures or re-date existing evidence.

A documentation or harness update does not establish performance for a new source
revision. The report's recorded revision identifies the measured code.

## Required validation

1. Confirm traffic traverses each configured mesh and reaches the echo workload.
2. Collect proxy and host metrics during the measured load window, excluding warm-up.
3. Require every declared mesh/profile and its Fortio, proxy, and host evidence.
4. Reject request errors, malformed or non-finite measurements, achieved RPS below
   97% of target, host CPU utilization at or above 85%, and mismatched workload parameters.
5. Generate the report only from a complete valid capture. Preserve the previous
   report if collection or validation fails.
6. Review new captures against the previous capture; explain changes in the PR
   rather than treating publication as proof of improvement.

The reporter is `bench/scripts/aggregate.py`; its validation tests belong beside
the reporter. Internal `benches/`, `scripts/verify-microbench-paths.py`, and
`scripts/preflight.sh` remain correctness checks with locked coverage gates.
They do not supply additional public performance comparisons.

Hot-path changes still require driving tests for authentication, authorization,
byte integrity, half-close, cancellation, and concurrency where applicable.
Follow [engineering rules](engineering-rules.md), especially A1–A11, C6, and D4–D10.
Passing these checks does not establish a throughput or latency result.

## Unmeasured limits

The fixed-delay HTTP workload does not establish maximum capacity, connection-churn
cost, large-payload throughput, or performance across production networks.
Three kind nodes on one host do not reproduce independent physical machines.
Sampled proxy RSS is not a measurement of live per-connection allocation.

Resource limits and runtime settings must be validated with deployment-specific
traffic. No percentage improvement or resource target is promised by this note.
For operational settings, see [production tuning](production-tuning.md).

