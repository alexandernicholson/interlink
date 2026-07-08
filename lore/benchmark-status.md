# Benchmark status & methodology notes

## Topology (apples-to-apples)

All meshes run on one shared 3-node kind cluster (`bench/manifests/kind-3node.yaml`):
the Fortio load generator and the Go echo server are pinned to separate worker nodes
(`bench-role=client` / `bench-role=server`), so mesh traffic always crosses the node
boundary. Identical workload (Go echo server, 200 ms fixed delay, 1 KB payload), and
CPU/memory sampled the same way. The load generator itself is **unmeshed** in every
mesh (it only generates load; the mesh under test is the echo-side proxy / node daemon) —
meshing it broke the Linkerd run, since the injected sidecar keeps the Fortio Job pod
alive forever so the Job never completes and its logs are never collected.

interlink runs in-cluster as a node-level transparent daemon
(`bench/manifests/interlink/daemonset.yaml`), the same class of deployment as Istio's
ztunnel — previously it was measured out-of-cluster via `bench/local/`, which was not
comparable.

```bash
cd bench
./setup.sh
# Default profiles (320/3200/12800 rps) target a dedicated host. On a modest/shared
# host, use a sustainable set so the validity gate yields usable points:
BENCH_PROFILES="320:80,800:200,1600:400" BENCH_DURATION=120 ./scripts/run-interlink.sh
BENCH_PROFILES="320:80,800:200,1600:400" BENCH_DURATION=120 ./scripts/run-linkerd.sh
BENCH_PROFILES="320:80,800:200,1600:400" BENCH_DURATION=120 ./scripts/run-istio.sh
python3 scripts/aggregate.py           # regenerates results/comparison.md, gated
```

## The validity gate (why numbers can now be trusted)

`aggregate.py` marks each profile **valid** only if: achieved RPS was within 3 % of
target, there were zero connection errors, and peak host CPU (sampled from `/proc/stat`,
not per-node `kubectl top` — kind runs every node on one host) stayed below 85 %.
Invalid profiles are host-bound or error-bound and are excluded from the comparison and
overhead tables. This is the fix that makes the benchmark honest: it can no longer
silently publish contention-dominated or error-ridden numbers.

## Findings (2026-07-08, shared 16-core dev host)

**interlink at light load is excellent and trustworthy.** 320 rps / 80 conns:
p50 2.8 ms, p99 13.5 ms (+13.5 ms over the 200 ms base), **0 errors**, ~99 m proxy CPU,
host 43 %. ✅ valid.

**interlink drops connections at moderate concurrency — a real robustness issue, not
host contention.** At 800 rps / 200 conns it returned **74 connection errors** (of
~96 k requests) with p99 1.76 s, and at 1600 rps / 400 conns **105 errors** with p99
1.83 s — while the host sat at only **29 % / 52 %** CPU. Because the box had ample
headroom, this is interlink itself failing above ~200 concurrent connections, not the
machine. Prime suspect: the mux single-tunnel design (all connections between a node
pair funnel through one yamux session), consistent with the round-13 "high concurrency
funnels through one session" trade-off. **This is the next thing to investigate** — cap
streams per tunnel / open N tunnels per peer / fall back to 1:1 above a concurrency
threshold — and it should be reproduced with `INTERLINK_MUX=false` to confirm mux is the
cause. A published cross-mesh comparison table is deferred until this is resolved, since
interlink's moderate-load numbers would otherwise reflect a fixable bug rather than the
design's real ceiling.

**Linkerd/Istio numbers not yet captured on the new topology:** the Linkerd run in this
session produced empty Fortio output because of the meshed-load-generator bug above (now
fixed); Istio ambient was not completed. Rerun with the fixed harness for the comparison.
The earlier single-node figures were removed rather than mixed with the new topology.
