# Production tuning guide

Per-host and build-level tuning for `interlinkd` in production. None of these
change the proxy's semantics; they change where cycles and wakeups land.
Measured deltas for the build-level items live in `docs/performance-plan.md`
(R31 and later) — re-capture them on your own hardware before trusting the
magnitudes.

## Host tuning (Linux, sidecar-class nodes)

Highest-value items first; all are standard kernel-level moves, no code.

1. **IRQ + acceptor alignment.** Pin the NIC's receive queues to the same
   cores the proxy's acceptor tasks run on (`INTERLINK_ACCEPTORS`, 1..=16).
   `SO_REUSEPORT` hashes each connection to one listener; keeping the accept
   on the same core that receives the packets avoids cross-core cacheline
   transfer on every segment. With a single-queue NIC or small node, leave
   `INTERLINK_ACCEPTORS` at 1 — the R29 A/B found multi-acceptor neutral on
   a lightly loaded host.
2. **CPU isolation for the proxy cores** (`isolcpus=`, `nohz_full=`): removes
   scheduler ticks and other tasks from the cores running acceptor/relay
   work. Typical p99 win on pps-heavy nodes is double-digit percent; measure
   before and after on your traffic.
3. **Disable deep C-states** on the proxy cores (`cpupower idle-set -D`):
   exit latency from deep states shows up as p99 spikes under bursty churn.
4. **Transparent hugepages** (`madvise` mode) for the daemon: the relay
   buffer pool and mimalloc arenas touch memory in 64 KiB–2 MiB patterns;
   THP reduces TLB pressure on bulk profiles.

## Runtime knobs

| Knob | Default | Effect |
|---|---|---|
| `INTERLINK_ACCEPTORS` | 1 | SO_REUSEPORT listeners per proxy port; match to NIC queues/cores |
| `INTERLINK_COPY_BUF_SIZE` | 64 KiB | Relay buffer size (4 KiB..=1 MiB). 64 KiB won the R30 bulk A/B; larger buffers help only ≥256 KiB payloads |
| `INTERLINK_MUX_MAX_STREAMS` | 64 | Streams per proxy-to-proxy tunnel before a new tunnel is opened |
| `INTERLINK_TUNNEL_MAX_AGE_SECS` | (see config) | Tunnel lifetime cap; churn after eviction re-pays one handshake per tunnel |

Relay buffers are pooled and recycled across connections (bounded 8 MiB
free-list); raising `INTERLINK_COPY_BUF_SIZE` raises the retained ceiling
proportionally.

## Build-level

- **Release profile** (already set): `opt-level = 3`, fat LTO,
  `codegen-units = 1`, `panic = "abort"`, mimalloc global allocator,
  aws-lc-rs crypto provider.
- **PGO**: `scripts/pgo.sh all` produces a profile-trained `interlinkd`
  (churn+bulk bench workload as the profile source). Interleave arms when
  A/B-ing (single-run p99 varies 44–84 ms on identical configs, R29).
- **BOLT**: `scripts/bolt-optimize.sh` on Linux x86_64 — layout optimization
  on top of LTO/PGO. Experimental; no repo-verified numbers yet.

## SVID signature family

The proxy verifies whichever chain your CA issues; the test/bench PKI uses
Ed25519 SVIDs, and ECDSA P-256 is equally supported end-to-end (both are in
the aws-lc-rs verification set, and the bench suite drives full mTLS
handshakes with each — see the `tls_handshake` bench arms). Ed25519 signing
and verification are substantially cheaper than P-256 ECDSA, which matters
only on full handshakes; resumed handshakes and mux tunnels amortize the
cost away. Choose the family your CA/interop constraints dictate, not the
microbenchmark — but if you are issuing fresh, short-lived SVIDs at high
rotation rates, the issuance-side (CA) cost difference also favors Ed25519.
