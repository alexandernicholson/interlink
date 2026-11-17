# Run interlink benchmarks

Start with the [latest native Veyrmux comparison](../docs/benchmarks/results.md)
and its [raw comparison data](results/veyrmux-integration/comparison.json).
Use the harness below that matches the question you want to answer.

## Goals

Keep primitive costs, completed-connection latency, HTTP load tests, and
historical multi-mesh results separate. They measure different workloads.

| Workload | Entry point | What it measures |
| --- | --- | --- |
| Criterion primitives | `cargo bench --bench proxy` or `--bench crypto` | Individual protocol, identity, policy, copy, and cryptographic operations; not proxy request latency. |
| Criterion runtime services | `cargo bench --bench runtime` | DNS coordination, configuration, metrics and runtime components; not end-to-end service capacity. |
| Criterion two-proxy lifecycle | `cargo bench --bench data_plane` | Actual outbound and inbound proxies plus TCP echo in one process; selected cases complete a new application connection per iteration. |
| Direct TLS-server Fortio | [`local/run.sh`](local/run.sh) | Fortio → `examples/bench_server` using mTLS; no `interlinkd` proxy. |
| Single inbound-proxy Fortio | [`local/run-proxy.sh`](local/run-proxy.sh) | Fortio → inbound `interlinkd` over mTLS → plain HTTP echo; no outbound proxy or proxy-to-proxy mux. |
| Full two-proxy Fortio | [`scripts/run-interlink.sh`](scripts/run-interlink.sh) | Fortio → outbound sidecar → inbound sidecar → HTTP echo across kind workers. |
| Historical multi-mesh kind workloads | [`results/comparison.md`](results/comparison.md) | July 2026 interlink, Linkerd, and Istio ambient HTTP profiles with a 200 ms application delay. |

Only the two-proxy paths exercise the proxy-to-proxy mux. The old Fortio
captures do not measure the native Veyrmux cutover.

## Run the current-runtime lifecycle cases

From the repository root, run:

```bash
cargo bench --bench data_plane -- 'proxy_relay/(mux|legacy)_(small|16kb)$'
```

Criterion writes estimates under `target/criterion/` unless you override Cargo's
target directory. These four cases time application TCP connect, payload write,
write-half shutdown, exact echo, and peer EOF, with a one-second request deadline.
Fixture setup and teardown are excluded.

**One invocation measures only the checked-out runtime.** Its mux and legacy
cases are not the published stock-Yamux versus native-Veyrmux A/B.
The published values are serialized loopback lifecycle latencies, not maximum
throughput, request p99, cross-host, NAT, or persistent-connection measurements.

To reproduce the matched dataset, preserve these conditions:

1. Build runtime revisions `f3ccc5781dab` and `bbdf99a9ac40` with identical
   benchmark sources from `61f196597b80`, not each revision's original harness.
   Match the source hashes, compiler, dependencies, and build profile in the
   [raw comparison](results/veyrmux-integration/comparison.json).
2. Run each block unprivileged in a **fresh loopback-only Linux network
   namespace**. Shared-namespace attempts were excluded after conntrack
   saturation; do not substitute global network tuning.
3. Use fixed ABBAABBA ordering, four blocks per arm, 20 samples per block,
   100 ms warm-up, and 500 ms measurement. Retain network-counter deltas and
   aggregate the four block medians per arm.

See the [validation record](results/veyrmux-integration/validation.json) for the
checks behind the published dataset.

## Workload

The kind harness uses the [Go HTTP echo server](workloads/echo-server.go):
`/echo` reads and echoes the body after a default 200 ms delay; `/healthz`
reports readiness. The delay dominates HTTP latency and does not prove that
traffic traversed a proxy. The Criterion lifecycle workload has no such delay.

## Load profiles

These settings apply to the kind scripts, not the local scripts:

| Name | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `BENCH_PROFILES` | String | No | `320:80,800:200,1600:400` | Comma-separated `qps:connections` pairs. |
| `BENCH_DURATION` | Integer seconds | No | `60` | Measured duration per profile, capped at 60 seconds. |
| `INTERLINK_MUX` | Boolean string | No | `true` | Set `false` for the interlink mux-off control; writes to `results/interlink-nomux/`. |

Each kind profile uses a discarded 10-second warm-up and a 1,024-byte payload.
Fortio runs inside a long-lived client pod and is driven by `kubectl exec`.

## Metrics collected

- Fortio JSON records latency percentiles, achieved RPS, and response codes.
- CSV files record sampled proxy CPU and memory.
- The comparison aggregator checks achieved RPS, errors, and host CPU evidence
  before including a profile. See the [historical report](results/comparison.md)
  for its validity gate.

## Directory layout

| Path | Purpose |
| --- | --- |
| [`local/`](local/) | Native, no-Kubernetes Fortio scripts and captured local profiles. |
| [`scripts/`](scripts/) | kind mesh runners and comparison aggregation. |
| [`workloads/`](workloads/) | HTTP echo server and plain workload manifests. |
| [`manifests/`](manifests/) | kind topology and mesh-specific manifests. |
| [`results/veyrmux-integration/`](results/veyrmux-integration/) | Current matched runtime A/B artifacts. |
| [`results/comparison.md`](results/comparison.md) | Historical July 2026 multi-mesh comparison. |

## Quick start (local interlink only)

Follow the [local benchmark guide](local/README.md) for prerequisites, commands,
durations, optional churn/bulk profiles, and output filenames. Docker is not
required by the current local scripts.

## Full comparison (Kubernetes)

The checked-in multi-mesh numbers are historical July 2026 captures, not a
recapture after the native cutover. Running these scripts creates a new dataset.

### Prerequisites

- Docker and a Rust toolchain for the interlink image.
- `curl`, `tar`, `make`, Python 3, OpenSSL, and `envsubst`.
- Capacity to run a three-node kind cluster.

### Install tools

From the repository root:

```bash
cd bench
./setup.sh
```

The script installs kind `v0.24.0`, kubectl `v1.30.0`, Linkerd
`stable-2.14.10`, and istioctl `1.24.0` under `bench/bin/`, then builds images.

### Run one mesh

From `bench/`, choose one:

```bash
./scripts/run-interlink.sh
./scripts/run-linkerd.sh
./scripts/run-istio.sh
```

Each script creates its own three-node kind cluster, pins the client and server
to separate workers, and writes profile JSON and CSV files under `results/<mesh>/`.
Interlink uses two explicit sidecars; Linkerd uses injected sidecars; Istio
ambient uses L4 ztunnel without an L7 waypoint.

### Run all

From `bench/`, run:

```bash
make all
```

This runs all three meshes and generates `results/comparison.md`.

### Teardown

From `bench/`, remove the benchmark clusters:

```bash
./teardown.sh
```

## Reporting results

After collecting kind profiles, regenerate the comparison from `bench/`:

```bash
make report
```

Inspect `results/comparison.md` and the raw profile JSON. Verify validity and
capture conditions before comparing a new run with historical results.

## Methodology notes

- Keep the 200 ms HTTP-delay workload separate from zero-delay TCP lifecycle cases.
- Keep mux-on/off CPU comparisons separate from the stock-Yamux/native-Veyrmux A/B.
- Compare matching topologies, revisions, payloads, connection reuse, and durations.
- Treat sampled RSS as process memory, not live per-connection allocation.

## Known limitations

**Does `local/run-proxy.sh` measure the full mesh?** No. It measures one inbound
proxy, not an outbound/inbound pair.

**Do the July HTTP p99 values describe native Veyrmux?** No. Use the
[current comparison](../docs/benchmarks/results.md), which reports a different
metric and workload.

**Can I compare a new local run directly with July's numbers?** No. The local
Fortio and echo topology changed to native host processes in November 2026.
Keep those captures separate.

For the next task, [run local HTTP profiles](local/README.md) or
[read the matched native-mux results](../docs/benchmarks/results.md).
