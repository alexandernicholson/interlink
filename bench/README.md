# interlink Service Mesh Benchmark Harness

A reproducible, third-party-runnable benchmark suite that compares **interlink**, **Linkerd**, and **Istio ambient** on identical HTTP workloads.

## Goals

- Identical workload, identical load generator, identical measurement methodology across all meshes.
- Fully scripted so anyone with Docker + (optionally) a Kubernetes cluster can reproduce the numbers.
- Pin every tool and image version.
- Publish raw Fortio JSON plus resource usage CSVs.

## Workload

A minimal Go HTTP echo server:

- Handler reads the request body, waits a fixed processing delay (default **200 ms**), and echoes the body back.
- Exposes `/healthz` for readiness.
- Container image: built locally from `workloads/echo-server/`.

This matches the synthetic server pattern used in independent academic comparisons (e.g. arXiv 2411.02267).

## Load profiles

Profiles are `qps:connections` pairs set via `BENCH_PROFILES` (default
`320:80,800:200,1600:400`); connection counts are right-sized to the load by Little's
law (rps × 0.2 s latency × 1.25 headroom). Each profile runs for `BENCH_DURATION`
seconds (default 60, hard-clamped to 60 so a full 3-mesh comparison finishes in
minutes), preceded by a discarded 10 s warm-up.

Fortio runs in **server mode** inside a long-lived meshed client pod and is driven by
`kubectl exec`, so the result JSON arrives on a clean stdout:

```text
kubectl exec deploy/fortio-client -c fortio -- \
  fortio load -qps <RPS> -c <connections> -t 60s -payload-size 1024 -json /dev/stdout \
  http://echo-server:8080/echo
```

## Metrics collected

1. **Latency**: p50, p90, p99 from Fortio JSON output.
2. **Throughput**: actual RPS achieved.
3. **Proxy CPU/memory**: sampled every 2 s via `kubectl top pod --containers`, summed
   across every proxy container of the mesh (both interlink sidecars / both
   linkerd-proxy sidecars / all ztunnel pods), summarized as average and peak.
4. **Host CPU utilization**: sampled from `/proc/stat` — kind runs every node on one
   host, so this is the true saturation signal that per-node `kubectl top` cannot see.
5. **Error rate**: non-2xx / failed connections from Fortio.

`aggregate.py` applies a **validity gate**: a profile is publishable only if achieved
RPS is within 3 % of target, there were zero errors, and peak host CPU stayed below
85 %. Invalid profiles are excluded from the comparison table rather than silently
published.

## Directory layout

```text
bench/
├── README.md                     # this file
├── setup.sh                      # install pinned tools (kind, kubectl, linkerd, istioctl)
├── teardown.sh                   # destroy kind clusters
├── Makefile                      # convenience targets
├── workloads/
│   ├── echo-server.go            # Go echo server (200 ms fixed delay)
│   ├── echo-server.Dockerfile
│   ├── echo-server-deployment.yaml   # plain echo Deployment (linkerd/istio runs)
│   └── fortio-client-plain.yaml      # meshed fortio client (linkerd/istio runs)
├── manifests/
│   ├── kind-3node.yaml           # shared 3-node topology (client/server pinned to separate workers)
│   ├── namespace.yaml
│   ├── interlink/                # sidecar manifests: echo-with-sidecar.yaml, fortio-client.yaml
│   ├── linkerd/                  # namespace annotation for auto-injection
│   └── istio-ambient/            # namespace label for ambient mode
├── scripts/
│   ├── common.sh                 # cluster helpers, metric samplers, fortio exec driver
│   ├── run-interlink.sh          # run full interlink benchmark suite
│   ├── run-linkerd.sh            # run full Linkerd benchmark suite
│   ├── run-istio.sh              # run full Istio ambient benchmark suite
│   └── aggregate.py              # build results/comparison.md (with validity gate)
├── local/                        # local (no-Kubernetes) interlink harness
│   ├── run.sh                    # mTLS echo server benchmark
│   ├── run-proxy.sh              # full proxy-path benchmark
│   └── README.md
└── results/
    ├── comparison.md             # published gated comparison
    └── <mesh>/                   # raw fortio JSON + CPU/mem CSVs per run
```

## Quick start (local interlink only)

No Kubernetes required; only Docker (for the Fortio image) and a release build.
`run.sh` benchmarks the mTLS echo server directly; `run-proxy.sh` measures the full
proxy path (env knobs: `PROFILE_DELAY`, `CHURN=1`, `BULK=1`, `SKIP_STANDARD=1`).

```bash
cd bench/local
./run.sh
./run-proxy.sh
```

This produces `results/fortio-*.json`, CPU/memory CSVs, and `results/summary.md`.

## Full comparison (Kubernetes)

### Prerequisites

- Linux or macOS
- Docker
- 8 GB free RAM, 20 GB free disk

### Install tools

```bash
cd bench
./setup.sh
```

This installs pinned versions into `./bin/`:

- kind v0.24.0
- kubectl v1.30.0
- linkerd2 edge-24.11.4 (or stable equivalent)
- istioctl 1.24.0

### Run one mesh

```bash
./scripts/run-interlink.sh
./scripts/run-linkerd.sh
./scripts/run-istio.sh
```

Each script:

1. Creates a dedicated kind cluster from the shared 3-node topology
   (`manifests/kind-3node.yaml`), pinning the load generator and echo server to
   separate worker nodes so mesh traffic crosses the node boundary.
2. Deploys the echo server and the meshed fortio client (interlink: explicit sidecar
   containers, no iptables/NET_ADMIN; Linkerd: injected sidecars; Istio: ambient
   ztunnel).
3. Runs each `BENCH_PROFILES` entry via `kubectl exec fortio load` after a discarded
   10 s warm-up.
4. Samples proxy-container CPU/memory and host CPU during each run.
5. Writes raw results to `results/<mesh>/`.

### Run all

```bash
make all
```

### Teardown

```bash
./teardown.sh
```

## Reporting results

After running all suites, aggregate with:

```bash
make report
```

This generates `results/comparison.md` with a table like:

| Mesh | Profile | p99 latency | proxy CPU | proxy memory | throughput | errors |
|------|---------|-------------|-----------|--------------|------------|--------|

## Methodology notes

- **Fixed delay**: The echo server adds 200 ms of artificial work so latency differences between meshes are visible above the base processing time — and so that a ~200 ms p50 proves traffic actually traversed the mesh.
- **Warm-up**: Each profile is preceded by a discarded 10-second warm-up run.
- **Resource isolation**: Each mesh gets its own kind cluster (same 3-node topology) to avoid cross-test interference.
- **No limits**: Proxy containers run without CPU/memory limits during measurement so we observe actual usage, not scheduler throttling.
- **Validity gate**: profiles with >3 % RPS shortfall, any errors, or peak host CPU ≥85 % are excluded — a saturated host measures contention, not the mesh.
- **Reproducibility**: tool and image versions are pinned; the published run's raw JSON/CSVs are committed under `results/`.
- **Memory numbers**: proxy RSS is an allocator high-water mark, not live usage — fortio's warm-up plus per-profile reconnects churn connections, and freed 64 KiB relay buffers are retained by the allocator. See `lore/benchmark-status.md` for the verified root cause.

## Known limitations

- `kubectl top` requires the metrics-server. The setup script installs it in each kind cluster.
- Istio ambient waypoints (L7) are not yet benchmarked; only L4 ztunnel mTLS is measured.
- Local benchmark exercises only interlink because Linkerd and Istio require Kubernetes.
