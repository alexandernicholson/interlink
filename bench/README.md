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

| Profile | RPS | Connections | Duration | Payload |
|---------|-----|-------------|----------|---------|
| light   | 320 | 160         | 5 min    | 1 KB    |
| medium  | 3,200 | 1,600     | 5 min    | 1 KB    |
| heavy   | 12,800 | 6,400    | 5 min    | 1 KB    |

Fortio is used as the load generator:

```text
fortio load -qps <RPS> -c <connections> -t 300s -payload-size 1024 http://echo-server:8080/echo
```

## Metrics collected

1. **Latency**: p50, p90, p99 from Fortio JSON output.
2. **Throughput**: actual RPS achieved.
3. **Proxy CPU**: millicores consumed by the proxy container during the run.
4. **Proxy memory**: peak RSS of the proxy container during the run.
5. **Error rate**: non-2xx / failed connections from Fortio.

Proxy CPU/memory are sampled every 2 seconds via `kubectl top pod` (Kubernetes path) or `docker stats` (local path) and summarized as average and peak.

## Directory layout

```text
bench/
├── README.md                     # this file
├── setup.sh                      # install pinned tools (kind, kubectl, linkerd, istioctl)
├── teardown.sh                   # destroy kind clusters
├── Makefile                      # convenience targets
├── workloads/
│   ├── echo-server.go            # Go echo server
│   ├── echo-server.Dockerfile
│   └── echo-server-deployment.yaml
├── load/
│   └── fortio-job.yaml           # Fortio Kubernetes Job template
├── manifests/
│   ├── namespace.yaml
│   ├── interlink/                # interlink DaemonSet + ConfigMap + RBAC
│   ├── linkerd/                  # namespace annotation for auto-injection
│   └── istio-ambient/            # namespace label for ambient mode
├── scripts/
│   ├── common.sh                 # helpers
│   ├── run-interlink.sh          # run full interlink benchmark suite
│   ├── run-linkerd.sh            # run full Linkerd benchmark suite
│   ├── run-istio.sh              # run full Istio ambient benchmark suite
│   └── measure.sh                # sample CPU/memory and aggregate Fortio output
├── local/                        # Docker-only interlink benchmark (no Kubernetes)
│   ├── docker-compose.yml
│   ├── run.sh
│   └── README.md
└── results/
    └── template.md               # where final numbers are recorded
```

## Quick start (local interlink only)

No Kubernetes required; only Docker and the interlink release binary.

```bash
cd bench/local
./run.sh
```

This produces:

- `results/fortio-*.json`
- `results/proxy-metrics.csv`
- `results/summary.md`

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

1. Creates a dedicated kind cluster.
2. Deploys the echo server.
3. Installs/configures the mesh.
4. Runs light, medium, and heavy Fortio load profiles.
5. Collects metrics.
6. Writes results to `results/<mesh>/`.

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

- **Fixed delay**: The echo server adds 200 ms of artificial work so latency differences between meshes are visible above the base processing time.
- **Warm-up**: Each profile is preceded by a 30-second warm-up run that is discarded.
- **Resource isolation**: Each mesh gets its own kind cluster to avoid cross-test interference.
- **No limits**: Proxy containers run without CPU/memory limits during measurement so we observe actual usage, not scheduler throttling.
- **Reproducibility**: All randomness is eliminated; seeds are fixed and container images are pinned.

## Known limitations

- `kubectl top` requires the metrics-server. The setup script installs it in each kind cluster.
- Istio ambient waypoints (L7) are not yet benchmarked; only L4 ztunnel mTLS is measured.
- Local benchmark exercises only interlink because Linkerd and Istio require Kubernetes.
