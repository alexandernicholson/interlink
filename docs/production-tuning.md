# Tune an interlink deployment

Use these controls to evaluate `interlinkd` under your deployment's traffic.
Defaults are starting settings, not capacity guarantees or promised performance gains.

## Prerequisites

- A working deployment with valid SPIFFE credentials and the intended authorization policy.
- Access to proxy metrics, application latency and error counts, and host CPU and memory usage.
- A rollback path to the previous deployment configuration.

## 1. Record a baseline

Record the source revision, traffic shape, payload sizes, connection reuse,
concurrency, resource limits, and host configuration before changing a setting.
Observe application errors and p50/p99 latency alongside proxy CPU and RSS.

From the proxy's network namespace, check the default metrics endpoint:

```bash
curl --fail http://127.0.0.1:4190/metrics
```

Expect Prometheus text. If you changed the metrics bind address or port, use that
address instead. Confirm metrics collection works before interpreting a tuning trial.

## 2. Change one runtime setting

Set environment variables in the daemon's deployment configuration and restart
the affected proxies. Compare repeated trials with the same traffic and resource limits.

| Setting | Default | Behavior |
| --- | --- | --- |
| `INTERLINK_ACCEPTORS` | Available CPUs, limited to 2–4 | `SO_REUSEPORT` acceptor tasks per listener; explicit values are clamped to 1–16 |
| `INTERLINK_COPY_BUF_SIZE` | `65536` bytes | Relay buffer size per direction; clamped to 4 KiB–1 MiB |
| `INTERLINK_MAX_CONNECTIONS` | `10000` | Admission limit for each proxy direction, including mux streams; must be positive |
| `INTERLINK_MUX_MAX_STREAMS` | `64` | Concurrent streams per tunnel; must be positive; configure matching caps on both endpoints |
| `INTERLINK_TUNNEL_MAX_AGE_SECS` | `3600` | Inbound tunnel age limit; stops new admissions and drains existing streams; must be positive |
| `INTERLINK_WORKER_THREADS` | Available CPUs, limited to 2 | Tokio worker threads; explicit values are clamped to 1–64 |

Relay buffers return to a process-wide free list capped at 128 buffers.
At the default size, that free list can retain 8 MiB, excluding active buffers,
allocator overhead, and other proxy state. Increasing the buffer size increases
the free-list memory budget proportionally.

Outbound tunnels to one peer can carry `INTERLINK_MAX_CONNECTIONS` streams.
When every tunnel carries two or more streams, a new stream opens another tunnel,
up to one per worker thread.

Each relayed connection uses two file descriptors. At startup the daemon raises
its open-file soft limit to the hard limit and logs both values. Do not raise
admission limits without checking that hard limit, memory, and downstream capacity.
A stream cap is not a request-rate limit.

## 3. Verify behavior and keep a rollback

Confirm application success, byte integrity, and expected authorization before
evaluating latency or resource changes. Watch these counters:

- `interlink_saturation_rejections_total`: admission saturation.
- `interlink_connection_errors_total`: failed relays.
- `interlink_handshake_full_total` and `interlink_handshake_resumed_total`: completed handshake kinds.
- `interlink_mux_tunnels_opened_total` and `interlink_mux_streams_total`: tunnel and stream activity.

Compare counter deltas over the same interval; lifetime totals are not comparable
across process restarts. Retain a change only when the observed result meets your
latency, error, and resource requirements. Otherwise restore the previous setting.

## Questions

### Should I tune the Linux host first?

Start with measured CPU, memory, scheduler, and network pressure. Do not apply
CPU isolation, IRQ affinity, power-state, or huge-page changes without evidence
that the relevant subsystem limits your workload.

> **Warning:** Host-wide tuning can affect every workload on the node. Trial changes
> on an isolated node and retain the previous configuration.

### Does the release build need extra optimization scripts?

Use the standard release build. `Cargo.toml` enables optimization level 3, LTO,
one codegen unit, stripping, and abort-on-panic. The daemon uses mimalloc without
transparent-huge-page advice; a host that enables THP for all memory still applies it.
No additional build-training or binary-rewriting workflow is required.

### Which SVID signature family should I use?

Choose a family supported by your CA and peers. No production performance advantage
for a signature family is established here.

### Can I use the published comparison to size production?

Use it only within its recorded workload and topology. It does not replace a
capacity test with your application, network, connection lifecycle, and resource limits.

## Next steps

Read the [service-mesh comparison](../bench/results/comparison.md) and its
[capture guide](../bench/README.md) for the reproducible comparison workload.
See [key management](architecture/key-management.md) for credential lifecycle guidance.

