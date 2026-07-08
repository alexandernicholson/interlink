# Benchmark status & methodology notes

## Topology (apples-to-apples)

All meshes run on the same 3-node kind topology (`bench/manifests/kind-3node.yaml`,
one dedicated cluster per mesh): the Fortio load generator and the Go echo server are
pinned to separate worker nodes (`bench-role=client` / `bench-role=server`), so mesh
traffic always crosses the node boundary. Identical workload (Go echo server, 200 ms
fixed delay, 1 KB payload), and CPU/memory sampled the same way (`kubectl top pod
--containers`, summed across all proxy containers).

interlink runs as a **per-pod sidecar** (`bench/manifests/interlink/
echo-with-sidecar.yaml`, `fortio-client.yaml`) — no iptables, no NET_ADMIN, no
hostNetwork; the Service targets the sidecar's inbound port and the app dials the
sidecar's outbound port. The load-generator pod is **meshed in every mesh** (interlink
sidecar / Linkerd injection / Istio ambient) and runs as a long-lived Deployment driven
by `kubectl exec fortio load` — the earlier Fortio Job model broke under Linkerd
because the injected sidecar kept the Job pod alive forever. See the RESOLVED section
at the bottom for the deployment-model history (the original iptables DaemonSet was
wrong for interlink and unmeasurable).

```bash
cd bench
./setup.sh
# Profiles are qps:conns pairs; durations are clamped to 60 s per profile.
BENCH_PROFILES="320:80,800:200,1600:400" ./scripts/run-interlink.sh
BENCH_PROFILES="320:80,800:200,1600:400" ./scripts/run-linkerd.sh
BENCH_PROFILES="320:80,800:200,1600:400" ./scripts/run-istio.sh
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
cost of the highest memory (root-caused below — allocator high-water, not live state).
All three prior blockers are fixed: the CIDR-gated interception is gone (no interception
at all now), and the SPIFFE server verifier (round 16) makes identity-based mTLS work
regardless of dial address.

## Memory root cause (2026-07-08): churn-ratcheted copy buffers, not live state

The published 94 MB avg at 1600 rps (vs Istio's 18.7 MB) was investigated with a local
two-sidecar replica of the k8s path (client → outbound proxy → mTLS/mux → inbound proxy
→ echo). Findings, all measured:

- The k8s samples ratchet monotonically across profiles — 7 MB fresh → 36 MB after the
  320 rps run → 48 after 800 → 131 MB by the end of 1600 — and never drop, even with
  all connections closed between profiles.
- 400 *fresh* held connections cost only ~29 MB total across both sidecars; sustained
  1600 rps-equivalent load holds it flat at ~31 MB. Live memory is small.
- Connection **churn** is the trigger: closing all 400 and reopening (exactly what
  fortio's discarded 10 s warm-up plus per-profile reconnects do) ratchets RSS
  15 → 35 → 69 MB per proxy per generation — 126 MB total after three generations,
  matching the k8s end-of-run 131 MB.
- With `INTERLINK_COPY_BUF_SIZE=8192` the same churn caps at 44 MB total → the two
  eager 64 KiB zeroed copy buffers per connection per sidecar dominate.

Mechanism: fresh `vec![0; 65536]` buffers are lazy zero-pages (only the ~1 KB actually
written ever faults in), but once freed and reused for a later zeroed allocation,
mimalloc must memset the recycled dirty block — faulting the full 64 KiB — and mimalloc
retains freed pages rather than returning them to the OS. RSS therefore converges on
the high-water mark of touched buffer memory (~128 KiB per connection per sidecar), not
on live usage. The same 64 KiB size is a deliberate R30 trade (p50 −15 %, CPU −22 % on
256 KB bulk payloads vs 8 KiB).

Options if memory becomes a requirement, in order of preference: pool copy buffers
across connections (removes the ratchet, keeps 64 KiB, small churn win — but requires a
custom bidirectional copy loop, a B14-sensitive change that must pin half-close
propagation); tune mimalloc page purging (cheap, decays RSS after churn, needs a
CHURN=1 A/B); the `INTERLINK_COPY_BUF_SIZE` knob already exists for memory-tight,
small-payload deployments. Lowering the 64 KiB default would regress bulk throughput
(R30) and is not recommended. No code change made yet.
