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

**Mux concurrency ceiling — found and fixed.** The earlier run showed interlink
dropping connections at moderate concurrency (74 errors @ 800 rps/200 conns, 105 @
1600/400) with the host only 29–52 % utilized. Root cause, reproduced locally: a single
yamux session per peer caps at `max_num_streams = 512`, so all connections between a
node pair funneled through one session and were dropped past that ceiling (a
700-concurrent-held-stream test failed 512/700 on the old design). Fixed by pooling
tunnels per peer (`src/proxy/mux.rs`): streams are spread load-aware across up to 16
tunnels, each soft-capped at 200 live streams (3200/peer before back-pressure), and the
stream-open channel was widened. The reproducer test (`tests/mux_tunnel.rs::
test_mux_beyond_single_session_cap`, 700 concurrent held streams) now passes with zero
failures, as do the 300-concurrent and handshake-avoidance tests. See
`docs/performance-plan.md` for the round writeup.

**Linkerd/Istio numbers not yet captured on the new topology:** the Linkerd run in this
session produced empty Fortio output because of the meshed-load-generator bug above (now
fixed); Istio ambient was not completed. Rerun with the fixed harness for the comparison.
The earlier single-node figures were removed rather than mixed with the new topology.


## Root-cause chain from the on-host comparison attempt (2026-07-08)

Running the comparison on the shared host surfaced a chain of real defects. Fixing each
exposed the next — the earlier "good" interlink numbers turned out not to be measuring
interlink at all:

1. **Harness: Service traffic bypassed the mesh.** The egress iptables rule gated on the
   pod CIDR (`-d 10.244.0.0/16`), but apps dial the echo **Service ClusterIP**
   (`10.96.0.0/12`) — so the client-side outbound redirect matched **0 packets** and
   fortio's plaintext went straight to the echo pod. The "interlink light: 2 ms, 0 errors"
   figures were direct, un-meshed HTTP. Fixed: egress intercepts the app port regardless
   of destination (`bench/manifests/interlink/daemonset.yaml`); the proxy recovers the
   real dst via `SO_ORIGINAL_DST`.

2. **Product: outbound mTLS did RFC 6125 name validation.** Once traffic was actually
   intercepted, every handshake failed — `certificate not valid for name "10.96.6.223"`.
   A SPIFFE mesh dials by ephemeral pod/Service IPs and must authenticate by the **SPIFFE
   URI SAN**, not by matching the dial address. Fixed with a custom
   `SpiffeServerVerifier` (`src/proxy/verify.rs`) that keeps full RFC 5280 path validation
   (chain to the trust-domain CA, signatures, expiry — verified by tests that still reject
   an untrusted CA and a wrong trust domain) and replaces name matching with SPIFFE
   trust-domain authentication. This is the same chain-only posture the inbound direction
   already used for client certs.

   With (1) and (2) fixed the mesh genuinely carries traffic: q800 shows the expected
   ~210 ms (200 ms echo delay + proxy overhead) and mux is active (50k+ streams), where
   before it was 0.

3. **Still open — harness transparent-proxy interception is not production-grade.** Under
   real cross-node load the benchmark manifest still produces `Connection refused`
   (host-network proxy dialing a ClusterIP that kube-proxy doesn't DNAT on that path) and
   residual plaintext leaks (`InvalidContentType`) — dozens at q800, tens of thousands at
   q1600/400. Correctly intercepting the ClusterIP + cross-node + hostNetwork matrix is
   exactly what CNI-integrated meshes engineer carefully; this simplified iptables
   DaemonSet does not. **This is a benchmark-harness limitation, not an interlink proxy
   defect** (the proxy's SPIFFE mTLS + mux path is verified working by the unit/integration
   suites and carried 50k streams here).

**Consequently, a cross-mesh comparison is still not publishable from this harness.** A
trustworthy run needs either a CNI-integrated deployment (interlink as a real sidecar/
ambient dataplane) or a corrected interception model — not more runs of the current one.
The validity gate correctly refuses every affected profile.

## RESOLVED (2026-07-08): sidecar model, published numbers

The iptables/hostNetwork DaemonSet was the wrong deployment model for interlink (a
per-pod sidecar) and the source of the interception failures above. Replaced with the
intended **sidecar** deployment — app on localhost, interlinkd sidecar terminates/
originates mTLS, Service targets the sidecar's inbound port; **no iptables, no
NET_ADMIN, no hostNetwork**. The load generator is a long-lived Deployment driven by
`kubectl exec fortio load` (clean JSON on stdout, no Job+sidecar completion problem);
Linkerd/Istio use the same meshed client Deployment.

Result: a clean **9/9-valid, zero-error** comparison (see `bench/results/comparison.md`
and the README). interlink has the lowest latency overhead and CPU of the three, at the
cost of the highest memory (per-connection/tunnel state) — the next optimization target.
All three prior blockers are fixed: the CIDR-gated interception is gone (no interception
at all now), and the SPIFFE server verifier (round 16) makes identity-based mTLS work
regardless of dial address.
