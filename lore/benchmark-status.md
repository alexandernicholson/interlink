# Benchmark status & methodology notes

## Topology (apples-to-apples)

All meshes run on one shared 3-node kind cluster (`bench/manifests/kind-3node.yaml`):
the Fortio load generator and the Go echo server are pinned to separate worker nodes
(`bench-role=client` / `bench-role=server`), so mesh traffic always crosses the node
boundary and every mesh pays its full data-path on the same path. Workload is identical
across meshes: Go echo server with a 200 ms fixed delay, 1 KB payload, Fortio load, and
CPU/memory sampled the same way (`kubectl top pod`, summed across each mesh's proxy
pods). interlink runs in-cluster as a node-level transparent daemon
(`bench/manifests/interlink/daemonset.yaml`), the same class of deployment as Istio's
ztunnel — previously it was measured out-of-cluster via `bench/local/`, which was not
comparable.

Run it:

```bash
cd bench
./setup.sh
./scripts/run-interlink.sh                      # mux tunnels (default)
INTERLINK_MUX=false ./scripts/run-interlink.sh  # 1:1 control variant
./scripts/run-linkerd.sh
./scripts/run-istio.sh
python3 scripts/aggregate.py                    # regenerates results/comparison.md
```

## Status (2026-07-08)

interlink's in-cluster data path is verified end-to-end: mTLS establishes, mux tunnels
form (`interlink_mux_tunnels_opened_total` / `interlink_mux_streams_total` increment),
and requests return HTTP 200 through the mesh on both ClusterIP and pod-IP paths.

A refreshed cross-mesh comparison table is **pending a run on a quiet, dedicated host.**
On the shared 16-core development machine used during implementation, the medium/heavy
profiles are contention-dominated and not trustworthy for a fair comparison: both
interlink *and* the `INTERLINK_MUX=false` control variant collapse identically at the
medium profile (~920 RPS achieved vs 3,200 target, p99 ~18 s, dozens of connection
errors) under 1,600 concurrent cross-node connections. Because both variants degrade the
same way, the dominant cause is host saturation — Fortio's 1,600 client threads plus two
proxies plus the kube system on one box — not a mesh-specific defect. This matches the
machine-noise ceiling documented in the performance plan (rounds 9/11), where heavy-
profile p99 varied 2× run-to-run for identical configs.

Per project rule (no publishing numbers we don't trust — A4/A10/C6), these numbers were
**not** written into `bench/results/comparison.md` or the README. The earlier
mixed-methodology figures (interlink measured locally; Linkerd/Istio on a single-node
cluster) were left in place, not overwritten, and are superseded by the unified harness
above once it is run on a suitable host.

## Known trade-off to quantify: mux at high concurrency

Multiplexed tunnels route *all* connections between a node pair through **one** yamux
session. This is exactly why mux wins on connection *churn* (one TLS handshake amortized
across thousands of sequential reconnects — measured −63 % proxy CPU on the local
harness), but at high *concurrency* it funnels everything through a single session's
flow-control window and stream scheduler, a plausible head-of-line-blocking bottleneck.
The current mux tests only exercise ≤10 concurrent streams; the medium K8s profile
(1,600 concurrent) can't isolate this because host contention masks it.

Planned work (see `docs/performance-plan.md`):
- Cap streams per tunnel and open N tunnels per peer, or fall back to 1:1 above a
  concurrency threshold.
- Add a >1,000-concurrent-stream mux test.
- Quantify mux vs 1:1 at each profile on a dedicated host.
