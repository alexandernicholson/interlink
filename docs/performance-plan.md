# Performance Uplift Plan

Status: **R29 resolved (2026-07-08, eleventh round)** — the reported `SO_REUSEPORT`
regression **does not exist**: an interleaved same-binary A/B (3× heavy + 2× churn
trials per arm via a new `INTERLINK_ACCEPTORS` env knob) shows the arms statistically
indistinguishable; single-run heavy-profile p99 on this shared machine varies
44→84 ms across *identical* configs, and every "regression" number cited in rounds
9–11 sits inside that noise band. See the eleventh-round findings for the data and
the new measurement rules. **R28 and R30 closed in the twelfth
round** — regression and measurement queues are both empty; see the twelfth-round
section for the copy-buffer decision (64 KiB), the bulk-256 KB baseline, and the
flamegraph, which points remaining optimization work at handshake avoidance. Items marked ✓ are implemented and verified;
⚠ marks partial or historical states in the findings log.
Grounded in a full read of the per-connection hot path
(`src/proxy/tcp.rs`, `src/proxy/outbound.rs`, `src/proxy/handshake.rs`, `src/policy/mod.rs`,
`src/common/identity.rs`, `src/discovery/dns.rs`, `src/metrics/mod.rs`) and the measured
results in `bench/results/`.

## Where the time goes today

interlink already wins the published comparison (p99 overhead +25 ms vs Linkerd +495 ms /
Istio ambient +147 ms at 12,800 RPS), but the benchmark workload is delay-bound (200 ms
server delay), which hides per-connection proxy cost. Every accepted connection currently pays:

1. A **full TCP connect + mTLS handshake to the upstream** (outbound path) — no connection
   reuse of any kind.
2. **`Url::parse` on every policy pattern, twice per rule, per connection**
   (`SpiffeId::matches_pattern` re-parses the pattern string on each call).
3. A **full X.509 parse of the peer certificate** to extract the SPIFFE ID, including
   building the SAN OID from scratch (`handshake.rs:233`).
4. **Serialized latency**: inbound does TLS handshake → policy → *wait for first client
   bytes* (protocol detect) → *then* TCP-connect the upstream. The upstream connect and the
   detection read could overlap the handshake.
5. **String churn**: original destination is formatted to `String`
   (`original_dst.rs:42`), possibly reformatted by `resolve_upstream`, then re-parsed by
   `TcpStream::connect`. Several allocations + a parse per connection.
6. **Three `info!` log lines per connection** at the default `info` level — formatting and a
   write syscall each, per connection.
7. **Head-of-line blocking in the accept loop**: `handle_accept` is awaited inline and blocks
   on `connection_semaphore.acquire_owned().await`, so when the limit is hit the listener
   stops accepting entirely (including shutdown responsiveness).
8. **8 KiB copy buffers**: `tokio::io::copy_bidirectional` defaults to 8 KiB per direction;
   `buffers::SOCKET_READ` (16 KiB) is defined but unused.
9. **DNS cache stampede**: on TTL expiry every in-flight connection for the same name
   re-resolves concurrently (no single-flight), and the TTL is accidentally the
   `DNS_RESOLVE` *timeout* constant (5 s).

## Review findings — first round (2026-07-08), fixes verified

Code review of commits `583f3f5..d071da4` verified the good items (compiled policy
patterns, identity cache, overlapped connect, log demotion, inbound accept fix, mimalloc)
and re-measured policy evaluation at **40.5 ns** (claim confirmed). It found three
functional regressions and two gaps, addressed in `6bf1bf7..08c2223` and re-reviewed below:

1. **✓ P0 — DNS single-flight deadlock** (`src/discovery/dns.rs`). The deadlock itself is
   fixed and covered by `test_resolve_timeout` (verified: full test suite passes, resolve
   fails fast instead of hanging). *However, the single-flight is now a no-op — see
   second-round finding 1.*
2. **✓ P0 — Connection pool dead streams**. Checkout/checkin removed from
   `outbound.rs`; no fabricated identities remain. Verified. `pool.rs` is kept but fully
   dead (7 `never used` warnings — see second-round finding 4).
3. **✓ P1 — Duration histograms**. `thread_local!` starts replaced with explicit
   `Duration` parameters at every call site. Verified correct. *Minor residue: failure
   paths record `Duration::ZERO` — see second-round finding 3.*
4. **✓ P1 — Accept-loop fix inbound-only**. Outbound now uses `try_acquire_owned()`.
   Verified. *The new rejection metric increments the wrong counter — see second-round
   finding 2.*
5. **✓ P2 — Plan/doc drift.** Corrected.

## Review findings — second round (2026-07-08), fixes verified with one exception

Re-review of `6bf1bf7..08c2223`; fixes landed in `e225998..2b129a8`. Verified:
warning-clean build, all committed tests pass. Findings 2–5 are genuinely fixed;
finding 1's fix introduced a new P0 (see third round):

1. **⚠ P1 — DNS single-flight is a no-op** (`src/discovery/dns.rs`). The permit is now
   correctly held across `resolve_inner` (B1 satisfied — `Ok(_p)` binds for the arm's
   scope), so single-flight works. **But the rewritten control flow panics on the
   waiter-success path — see third-round finding 1 — and the added
   `test_concurrent_resolve_dedup` is vacuous — see third-round finding 3.**
2. **✓ P2 — Saturation rejections counted as connections** (`src/metrics/mod.rs`).
   Fixed in commit `e225998`. New `interlink_saturation_rejections_total` counter,
   wired to both inbound and outbound proxies.
3. **✓ P2 — Failure paths pollute the duration histogram.** Fixed in `e225998`.
   New `record_connection_failed()` skips the histogram entirely (C2).
4. **✓ P3 — Dead pool code.** Fixed in `e225998`. `pool.rs` deleted entirely (B6).
   Build is warning-clean.
5. **✓ P2 — `SocketAddr` end-to-end is only half done.** Fixed in `e225998`.
   `resolve_upstream` returns `SocketAddr` in both proxies; inbound `TcpStream::connect`
   uses it directly. Outbound `tls_client.connect` still takes `&str` (trait bound),
   converted at the last call site (marked ⚠ partial).

## Sixteenth round (2026-07-08): SPIFFE server verification — mesh mTLS to ephemeral IPs

Attempting the on-host comparison exposed that outbound mTLS did RFC 6125 server-**name**
validation: peers dialed by pod/Service IPs (recovered from `SO_ORIGINAL_DST`) failed with
`certificate not valid for name`, because a workload cert cannot carry every ClusterIP it
is reached by. This silently meant earlier benchmarks measured *un-meshed* traffic.

Fix (`src/proxy/verify.rs`): a `SpiffeServerVerifier` for the outbound `TlsClient`. It
keeps **full RFC 5280 path validation** (rustls's own
`verify_server_cert_signed_by_trust_anchor` against the trust-domain roots, plus TLS 1.2/
1.3 handshake-signature verification via the crypto provider) and *replaces* name matching
with **SPIFFE X.509-SVID authentication**: the leaf must carry a SPIFFE URI SAN whose trust
domain is ours, else the handshake fails closed (B5). This matches the inbound direction,
which already does chain-only client-cert validation and defers identity to the policy
engine, and it does not weaken the security posture — tests
(`tests/spiffe_verify.rs`) prove it still rejects an untrusted CA and a valid cert from the
wrong trust domain, while accepting a valid peer dialed by an address absent from its SAN
(the case stock WebPKI rejected). RFC alignment: RFC 5280 §4.2.1.6 (URI SAN), RFC 8446
(handshake signatures) — both preserved; only RFC 6125 name matching, which SPIFFE
replaces by design, is dropped.

Result: the mesh data path now genuinely carries traffic (mux active, expected 200 ms-base
latency observed). Remaining errors under load are a benchmark transparent-proxy
interception limitation (ClusterIP + cross-node + hostNetwork), documented in
`lore/benchmark-status.md`; a publishable cross-mesh table still needs a CNI-integrated
deployment rather than the simplified iptables DaemonSet.

## Fifteenth round (2026-07-08): mux concurrency ceiling fixed; benchmark capped at 60 s

**Mux tunnel pool (`src/proxy/mux.rs`).** The K8s benchmark exposed interlink dropping
connections at moderate concurrency with the host idle (round 14). Root cause reproduced
locally: one yamux session per peer has a hard `max_num_streams = 512` cap, so a
single-tunnel-per-peer design is a concurrency ceiling and a single point of failure — a
new test driving 700 concurrent held-open streams failed **512/700** on the old design.
Fix: `MuxPool` now keeps a *pool* of tunnels per peer. `open_stream` reserves a live-slot
(released by a drop guard on the returned stream), the router picks the least-loaded
tunnel under a 200-stream soft cap, and grows the pool (under the existing single-flight
creation lock) up to 16 tunnels/peer — 3200 live streams before back-pressure, well above
the heavy profile's 2560. The open-request channel widened 64→256. All 7 mux tests pass,
including the 700-stream reproducer (0 failures) and the unchanged handshake-avoidance
assertions (N low-concurrency connections still share 1 handshake). Preflight green.

**Benchmark capped at 60 s/profile.** Default `BENCH_DURATION` is now 60 and `common.sh`
clamps any larger override down to 60 (metrics-server scrapes ~every 15 s, so 60 s still
yields enough CPU/mem samples). A full 3-mesh × 3-profile comparison is now a few minutes.

## Fourteenth round (2026-07-08): unified K8s benchmark harness + a real concurrency finding

The comparison harness was made apples-to-apples: all meshes now run on one shared
3-node kind topology (`bench/manifests/kind-3node.yaml`, load-gen and echo pinned to
separate workers so traffic crosses nodes), and interlink runs *in-cluster* as a
transparent node daemon (`bench/manifests/interlink/daemonset.yaml`) instead of the old
out-of-cluster `bench/local/` path. Data path verified end-to-end: mTLS + mux tunnels
form, requests return 200. Several real harness bugs fixed on the way (all committed):
Dockerfile (rust 1.88, `benches/`+`examples/` needed to parse the manifest); iptables
interception that intercepts only the workload port and never the OUTPUT chain (a
catch-all OUTPUT redirect melts the control plane); `SO_ORIGINAL_DST` self-connect guard;
cert IP SANs (C8); fortio's "Successfully wrote…" status line corrupting `-json`
capture (the recurring stdout-interleaving bug — parser now raw_decode-scans); and
CPU sampling summed across all proxy pods rather than `items[0]` (which caught the idle
daemon, not the busy one).

**Finding (not yet a publishable comparison):** on the shared 16-core dev box, both
interlink **and** the `INTERLINK_MUX=false` control variant collapse at the medium
profile — ~920 RPS achieved vs 3,200 target, p99 ~18 s, dozens of connection errors —
under 1,600 concurrent cross-node connections. Because *both* variants collapse
identically, the dominant cause is host contention (fortio's 1,600 client threads + two
proxies + docker builds + kube system on one box), consistent with the machine-noise
ceiling documented in rounds 9/11. So the numbers are **contention-dominated and were
not published** (A4/A10/C6 — no untrustworthy numbers in the README or comparison.md;
the prior mixed-methodology table was left in place, not overwritten, and flagged
pending a clean re-run).

**Separately real, and worth its own investigation:** mux routes *all* connections
between a node pair through **one** yamux session. That is exactly why it wins on churn
(one handshake for thousands of sequential reconnects, −63 % CPU — round 13) but it also
means high *concurrency* funnels through a single session's flow-control and stream
scheduler. The medium collapse can't isolate this (contention masks it), but it is a
plausible head-of-line-blocking bottleneck. Future work: cap streams per tunnel and open
N tunnels per peer (or fall back to 1:1 above a concurrency threshold); quantify on a
dedicated host. The mux tests only exercised ≤10 concurrent streams — a >1,000-stream
test is the missing coverage (A8/A9 at scale).

**To finish (needs a quiet, dedicated host):** run all four variants
(interlink, interlink-nomux, linkerd, istio) at 300 s via the unified harness, then
`python3 bench/scripts/aggregate.py` regenerates `bench/results/comparison.md` and the
README table.

## Thirteenth round (2026-07-08): handshake avoidance implemented — mux tunnels

**✓ ALPN-negotiated multiplexed mTLS tunnels** (`src/proxy/mux.rs`, ALPN `il/mux/1`),
the architectural item open since round one. One mTLS connection per upstream peer
carries all application connections as yamux streams; peers that don't offer the ALPN
fall back to the legacy 1:1 relay, so mixed versions interoperate. `INTERLINK_MUX=false`
removes the ALPN offer (the offer *is* the feature flag; once negotiated, the wire
protocol is committed — a disabled-but-negotiated mismatch was caught in the A/B and is
pinned by a test).

**Measured (interleaved two-proxy churn A/B, 500 conn/s, plain-HTTP app side, 2 rounds
per arm, reproducible to ±0.3 %):**

| arm | p50 | p99 | combined proxy CPU | handshakes for ~19,016 conns |
|---|---|---|---|---|
| mux on | 0.72 ms | 1.9 ms | **10.0 %** | **1 full** (1 tunnel, 19,016 streams) |
| mux off | 1.33 ms | 2.0 ms | 26.8 % | 19,016 (8 full + 19,008 resumed) |

**−63 % proxy CPU and −46 % p50 latency** under connection churn — against a baseline
where TLS resumption is already working. Which is the round's second result:
**proxy→proxy resumption is now production-confirmed** — the mux-off control arm's
counters show 99.96 % resumed handshakes on the mesh path (Fortio's 0 % applies only to
its own client leg). The old R19/R24 "resumed fraction" question is closed with a
number.

Implementation notes:
- Multiplexer: libp2p-maintained `yamux` crate. `tokio-yamux` was tried first and
  **rejected by a size-sweep probe**: it deadlocks stream flow control at exactly
  >256 KiB even with an actively reading peer (B14's "test the documented semantics"
  applied to a dependency; the probe swept 64→512 KiB and failed at 257).
- Outbound: per-address tunnel pool, single-flight establishment (per-address async
  mutex — the DNS lesson), compare-and-remove eviction by tunnel id (a dying driver
  must not evict its replacement), policy evaluated per stream against the tunnel's
  peer identity. Legacy `Route` boxed (clippy `large_enum_variant`).
- Inbound: ALPN check after policy; every stream relays to the tunnel connection's
  recovered upstream (no per-stream address header needed — the tunnel key is the
  original destination, same as a legacy connection). Tunnel holds the accept slot;
  streams carry the byte/duration metrics (`record_tunnel_closed` releases the gauge
  without polluting `connections_total`/histograms).
- Fixed on the way: `SO_ORIGINAL_DST` returns the proxy's *own* address for
  non-redirected connections (conntrack has an entry either way) — the outbound proxy
  self-looped in no-iptables topologies; original dst on our own listen port now falls
  back to `default_upstream`. And `issue_leaf_with_key` gained IP SANs (C8: mesh peers
  dial by ip:port; DNS-only SANs made the outbound path unverifiable — it had never
  been TLS-tested end-to-end before these tests).
- Tests (`tests/mux_tunnel.rs`, two-proxy mesh with a handshake-counting TlsClient
  wrapper): 5 sequential conns → exactly 1 handshake; 10 concurrent streams with
  distinct payloads → 1 handshake, no cross-stream corruption (A2); legacy peer →
  3 conns / 3 handshakes (fallback, and the falsifiability proof for the counter);
  mux-disabled client offers legacy ALPN (pins the protocol-mismatch bug); 512 KiB
  half-close roundtrip (C7, and the test that catches the tokio-yamux class of bug).
- New metrics: `interlink_mux_tunnels_opened_total`, `interlink_mux_streams_total`.

Remaining backlog after this: crypto-provider bake-off, `worker_threads` config — both
now lower-value, since the dominant cost the flamegraph identified (full-handshake
crypto) is amortized away for mesh traffic. Tunnel idle-reaping and per-tunnel stream
caps are noted as future hardening (tunnels currently live until either side closes).

## Twelfth round (2026-07-08): R28 and R30 closed (by reviewer, at user's request)

**✓ R30 — copy-buffer default is 64 KiB, decided on data.** Interleaved same-binary A/B
(new `INTERLINK_COPY_BUF_SIZE` env knob; 3 trials/arm, bulk profile, 256 KB payloads):

| arm | p50 (3 trials) | proxy CPU |
|---|---|---|
| 8 KiB | 1.34 / 1.33 / 1.32 ms | 1.7–1.8 % |
| 64 KiB | 1.11 / 1.13 / 1.14 ms | 1.4 % |

64 KiB wins every metric in every trial with zero overlap (p50 −15 %, CPU −22 %).
Guard check on the heavy 1 KB profile: p50 and CPU identical across arms (21–22 ms /
~27 %), p99 bounces inside the known noise band in both — no small-payload penalty.
`DEFAULT_COPY_BUF_SIZE = 65536`; the `6355caa` revert's "cache pressure" concern is
retired. One real artifact explained on the way: the alarming 891 MB "churn RSS" in the
full-harness run is **carryover** — one proxy process serves all profiles and mimalloc
retains memory from the preceding 6,400-connection heavy profile; isolated churn runs
sit at ~10 MB. (Future harness nicety: fresh proxy per profile.)

**✓ R28 — all four items closed:**
1. **Bulk 256 KB baseline captured**: p50 1.32 ms, p99 2.0 ms, 0 errors, 6.1 % CPU
   (50 qps × 256 KB ≈ 13 MB/s through mTLS). Two root causes fixed in the harness:
   Fortio's default 128 KB client buffer can't hold 256 KB responses
   (`-httpbufferkb 512` added), and `-json /dev/stdout` merged with `2>&1` let a
   stderr log line land *inside* the JSON payload, corrupting it — JSON now goes to
   its own file via the mounted results dir.
2. **Summary clobbering fixed** (ninth-round finding 5): each profile writes a
   per-label section file stamped with capture time + git SHA; `proxy-summary.md` is
   assembled from all section files, so targeted reruns (new `SKIP_STANDARD=1`
   selector) update only their own sections.
3. **Resumed fraction: 0 % for the benchmark client, by design.** The harness scrape
   (landed in `885dad4`) shows 52,340/52,340 handshakes Full: Fortio's Go TLS client
   does not reuse session tickets, so benchmark churn measures full-handshake cost.
   Mesh-path resumption remains proven by `test_tls_resumption`. The measured churn
   CPU is therefore an *upper bound* for proxy→proxy churn.
4. **Flamegraph captured** (`bench/local/results/flamegraph-mixed-load.svg`; profiling
   build with symbols via the new `[profile.profiling]`, perf dwarf @ 397 Hz under
   mixed 3,200 qps keepalive + 300 conn/s churn). Top symbols: **~25 % elliptic-curve
   handshake crypto** (x25519 scalar mult + ed25519 verify), ~6 % tokio runtime,
   ~1.9 % SHA-256, ~1.5 % AES-GCM (data path), provider confirmed aws-lc-rs.
   **Conclusion: full-handshake crypto dominates; the data path is cheap. The
   highest-value remaining work is handshake avoidance (resumption-capable peers /
   connection reuse), not byte-shuffling.**

Remaining optimization backlog (all gated on A4/D10 numbers): pooling decision
informed by the flamegraph, crypto-provider bake-off (aws-lc-rs confirmed in use),
`worker_threads` config for edge devices.

## Review findings — eleventh round (2026-07-08), the SO_REUSEPORT "regression" is noise

Investigation of the teammate's report that `SO_REUSEPORT` causes a performance
regression (commits `885dad4..6355caa` + uncommitted reruns):

1. **The regression does not exist. It is run-to-run variance.** Controlled interleaved
   A/B at HEAD — identical binary, `INTERLINK_ACCEPTORS=4` vs `=1` (new env knob),
   alternating trials to cancel environmental drift, heavy profile (12,800 qps /
   6,400 conns / 30 s):

   | trial | 4 acceptors p99 | 1 acceptor p99 | p50 (both arms) |
   |---|---|---|---|
   | 1 | 43.9 ms | 44.8 ms | ~22 ms |
   | 2 | 66.9 ms | 80.8 ms | ~22 ms |
   | 3 | 69.9 ms | 64.4 ms | ~22 ms |

   The arms are indistinguishable; **within-configuration p99 varies by ±37 ms across
   trials** while p50 is rock-stable at 21.5–22.2 ms in every run. An accept-heavy A/B
   (2,000 conns/s, `-keepalive=false`) is equally flat: p99 15.9–17.5 ms both arms.
   Every number cited in the regression narrative — 44.3 "baseline", 62.7, 72.7, 73.5,
   84.3 "regression" — sits inside the observed noise band for identical configs. The
   teammate's own uncommitted rerun of HEAD shows heavy p99 = 48.5 ms, next to the
   "baseline" the same code was claimed to regress from. The machine is shared (other
   Docker workloads, agent sessions) and Fortio's 6,400 client threads compete with the
   proxy for the same 16 cores — tail latency at this concurrency is dominated by
   environment, not by the proxy change.
2. **Consequences for earlier conclusions:**
   - The `6355caa` buffer revert's reasoning ("64 KiB caused 84.3 vs 44.3, cache
     pressure") compared two points inside the noise band. Also, buffer size *cannot*
     matter for the heavy profile — 1 KB payloads never fill even an 8 KiB buffer. The
     right instrument is the bulk profile; that decision is reopened as **R30**. Note
     the revert leaves `COPY_BUF_SIZE = 8192` = tokio's default, i.e. the Phase 2
     buffer optimization is currently a no-op.
   - `SO_REUSEPORT` shows **no benefit** either, at up to 2,000 accepts/s — accept was
     never the bottleneck (handshake work is already parallel in spawned tasks). Verdict
     per the keep-or-revert criterion: performance-neutral; keep the (now panic-free,
     shutdown-correct, knob-controlled) implementation, revisit only if a workload
     saturates a single accept loop.
3. **Measurement rule for this machine** (feeds D10): single-run heavy-profile p99 is
   not a usable signal — the noise floor is ~2× the effect sizes being discussed. Tail
   comparisons need ≥3 interleaved runs per arm with medians, or an isolated machine.
   p50 and CPU are stable and remain usable single-run signals.
4. **R28 partial credit**: `885dad4` added handshake-metrics scraping and the run
   header (Generated/Git SHA) to the harness — good. Resumed-fraction output should now
   appear in summaries; bulk 256 KB and the flamegraph remain open.

## Review findings — tenth round (2026-07-08), R25 landed; R26–R29 untouched

Review of `08ff63e`:

1. **✓ R25 — the P0 copy fix and regression test are committed and verified.**
   `src/proxy/mod.rs` at HEAD delegates to `tokio::io::copy_bidirectional_with_sizes`
   (64 KiB), `tests/halfclose_propagation.rs` is in the tree, and preflight is green.
   The data-plane half-close hang is closed.
2. **D6/D1 note — the commit message misdescribes the commit.** `08ff63e` is titled
   "C8: fix resumption test — connect by localhost, not IP", but that fix landed in
   `f2c46dc` two rounds ago; the actual change to `mtls_handshake.rs` here is a
   cosmetic restructuring of already-correct code. The commit's *real* payload — the
   P0 half-close fix and its regression test — is not mentioned at all. Anyone
   bisecting a data-plane behavior change to this commit will be actively misled.
   No action required on the code; flagged because message-vs-diff drift keeps
   recurring (D6 exists for this; `git diff --stat` takes seconds).
3. **✓ R26/R27 — fixed during this round (by the reviewer, at the user's request).**
   - **R26**: acceptor sockets are now bound *before* spawning via a shared fallible
     `bind_reuseport()` helper in `proxy/mod.rs` (no panics, no lint `allow`s — B15);
     a failed bind logs and skips that acceptor, and zero bindable acceptors makes
     `run()` log an error and return instead of aborting the process.
   - **R27**: acceptors now select on a per-acceptor clone of the watch channel via a
     shared `wait_shutdown()` helper (handles the already-signalled case); the
     redundant `AtomicBool` flag, its 100 ms poll loop, and the `with_shutdown_flag`
     API are deleted from both proxies and `main.rs` (B14/C9 — one mechanism, the
     existing primitive, no parallel no-op API).
   - Tests (`tests/proxy_shutdown.rs`): watch shutdown stops `run()` (verified to
     **fail against the pre-fix code** — A6 — and pass with the fix), pre-signalled
     shutdown stops `run()`, and a privileged-port bind failure returns promptly
     without panicking (root-guarded). Preflight green.
4. **R28/R29 remain the queue**: resumed-fraction capture + summary clobbering + bulk
   256 KB + flamegraph (R28), and the `SO_REUSEPORT` before/after (heavy p99
   62.7 → 73.5 ms still unexplained; keep-or-revert on numbers — R29).

## Review findings — ninth round (2026-07-08), data-plane regression fixed in review

Review of `607fed4..87f5ebe` (the first optimization-phase round: copy buffers,
`SO_REUSEPORT`, R24 bench captures):

1. **P0 — the custom `copy_bidirectional` dropped half-close propagation; fixed in
   review.** Commit `4925b72` replaced `tokio::io::copy_bidirectional` with a 60-line
   custom `select!` loop that, on EOF from one side, merely flags `done` and never shuts
   the other side down — no FIN, no close_notify. Any protocol that reads to EOF hangs
   through the proxy, and under churn every connection lingers until idle timeouts
   (300 s keepalive) instead of closing — a slow connection/memory leak. Confirmed
   empirically in both directions per A11: a probe driving the real `TcpProxy` with a
   half-closing client and a read-to-EOF backend **hangs on the custom loop** and
   **passes when the copy delegates to tokio's**. The bench didn't catch it because
   Fortio's HTTP uses content-length framing (no read-to-EOF), and the e2e echo reads a
   fixed buffer. The irony: the plan item literally named
   `copy_bidirectional_with_sizes` — the tokio API that does exactly this with
   correct shutdown propagation. **Fixed in review (working tree): the custom loop is
   replaced by a thin wrapper over `tokio::io::copy_bidirectional_with_sizes(a, b,
   64 KiB, 64 KiB)`, and the probe is committed-pending as
   `tests/halfclose_propagation.rs`** (5 s timeout, drives the full mTLS proxy path).
   Preflight green. → **R25**: commit these.
2. **P1 — `SO_REUSEPORT` acceptors panic on bind/listen failure** (`tcp.rs`,
   `outbound.rs`): `expect`/`panic!` inside spawned acceptor tasks, under
   `panic = "abort"` → a bind failure (port taken, permissions) aborts the whole proxy.
   The B8 lint was bypassed with a scoped `allow(clippy::expect_used)` with no
   impossibility argument — bind failure is an ordinary runtime error (B12: the check
   was deleted-by-allow, not made fallible). Fix: log + return from the acceptor (and
   surface a startup error if *zero* acceptors bind). → **R26**.
3. **P1 — watch-channel shutdown no longer stops the accept loops.** Acceptors now poll
   only the new `AtomicBool` flag (100 ms sleep loop); a caller using the documented
   `with_shutdown(rx)` alone — every library consumer and test — can no longer stop the
   proxy. `main.rs` sets both, so the daemon works, but the public API contract is
   silently broken, and the poll loop replaces event-driven wakeup. Fix: acceptors
   select on the watch receiver (clone per acceptor); drop the flag or keep it as an
   internal detail. → **R27**.
4. **P2 — R24 marked ✓ while its principal deliverable is missing** (D7/A10 again).
   `grep -r resumed bench/` returns nothing: the harness never scrapes the
   `interlink_handshake_{full,resumed}_total` counters, so the resumed fraction — the
   number the pooling decision hinges on — was never captured. Bulk 256 KB (commit
   message: "needs investigation") and the flamegraph are also still open. → **R28**:
   have `run-proxy.sh` curl the metrics endpoint before/after each profile and emit the
   two counters into the summary.
5. **P2 — summary regeneration clobbers prior sections**: the committed
   `proxy-summary.md` now contains only the four profiles from the last run — the churn
   sections captured in `607fed4` were overwritten by the bulk run (the script rewrites
   the file from scratch). Append per-profile files, or write one summary per run
   directory. → also **R28**.
6. **P3 — `SO_REUSEPORT` shipped without a before/after comparison** (A4). The rerun
   zero-delay baselines actually moved the heavy profile the wrong way (q12800 p99:
   62.7 → 73.5 ms; q320 unchanged) — possibly noise, possibly contention from 4
   acceptors × accept-loop `handle_accept` awaits, but nobody compared. → **R29**: run
   a controlled before/after on the heavy + churn profiles and keep or revert
   `SO_REUSEPORT` based on the numbers.

## Review findings — eighth round (2026-07-08), regression queue clear

Review of `f2c46dc`:

1. **✓ R23 — review fixes committed intact.** The seventh-round working-tree fixes
   (repaired `test_tls_resumption`, `.githooks/pre-commit`, the tmpfs→disk bench-target
   fix, rules/CLAUDE.md updates) landed verbatim in `f2c46dc`. Verified at HEAD:
   `./scripts/preflight.sh` is green (clippy `-D warnings` + all 75 tests, including
   the resumption test asserting `Full`→`Resumed`), the working tree is clean, and the
   pre-commit hook is active (`core.hooksPath = .githooks`).
2. **Process notes, minor**: the commit bundles four logical changes (test fix, hook,
   bench-script fix, docs) — D1 prefers these split; and the message ("Update plan:
   R23 ✓") *understates* the diff, the first time drift has run in that direction (D6).
   Neither affects correctness; noted for hygiene.
3. **Open work is measurement, not remediation**: **R24** (churn run reporting the
   resumed fraction via `interlink_handshake_*_total`, bulk 256 KB baseline,
   flamegraph) is the only Phase 0 item left. **For the first time in eight rounds
   there are no open regressions.** Optimization items may now proceed, gated as ever
   on before/after numbers (A4) — R24 first, since its resumed-fraction number decides
   the pooling redesign's priority.

## Review findings — seventh round (2026-07-08), test fixed in review

Review of `e0f4bc7..40242a7`, which claims "R21/R22 ✓, all 6 rounds resolved":

1. **✓ R22 — `SpiffeId::new` restricted to `pub(crate)`** (B13). Verified: external
   construction now goes through `try_new()`/`from_uri()`; examples, benches, and
   integration tests were migrated accordingly. Done cleanly.
2. **⚠→✓ R21 — the committed resumption test had three bugs and failed; fixed during
   this review.** `test_tls_resumption` as committed in `428c967` **did not pass** —
   `cargo test --test mtls_handshake` failed in 10.00 s (the TLS handshake timeout).
   Since it fails deterministically, it was never run before being committed: a direct
   D8 violation, and the "R21 ✓ / preflight" claims in `e0f4bc7`/`40242a7` are false
   (A5/D6). It is also the A6 failure in its purest form — a test that has never passed
   asserts nothing. The three bugs, each diagnosed and fixed in review (working tree,
   uncommitted):
   - A stray `TcpStream::connect(addr)` before the TLS connect consumed the server's
     single `accept()`, so the TLS server waited 10 s for a ClientHello on a raw socket
     while the real client's handshake starved. (Removed.)
   - The client connected by `addr.to_string()` = `127.0.0.1:port`, giving an IP
     `ServerName`; the server cert carries only the `localhost` DNS SAN →
     `InvalidCertificate(NotValidForName…)`. (Now connects via `localhost:{port}`,
     matching the cert and the sixth-round probe.)
   - Connection 2's server task does `read_exact`/`write_all` but the client never
     wrote → server panicked on `UnexpectedEof`. (Client now mirrors the byte exchange;
     the `Resumed` assertion stays immediately after the handshake, where the kind is
     already known.)
   Plus two clippy `useless_conversion` warnings in the same file (preflight would have
   been red for that reason alone). **After the fixes: the test passes, asserts
   `Full`→`Resumed` on interlink's own `TlsClient`/`TlsServer`, and
   `./scripts/preflight.sh` is green.** These fixes are in the working tree and need to
   be committed (→ **R23**).
3. **R21's second half remains open**: the resumed fraction from a churn-profile run
   (using the `interlink_handshake_*_total` counters) has still not been captured
   (→ folded into **R24** with the other outstanding bench work).

## Review findings — sixth round (2026-07-08), near-clean

Review of `b3b9958..a21a272`. Preflight (clippy `-D warnings` + tests) is green. This is
the first round where the fixes match their claims almost exactly:

1. **✓ R18 — SPIFFE validation at the three config sites.** Verified: all three now use
   `try_new(…).expect("trust_domain validated in Config::validate")`, and the
   justification is *true* — `Config::load()` calls `validate()` (`config.rs:67`), which
   rejects an empty trust domain, and the `expect` is startup-time with a scoped,
   justified `allow` per B12. Per-item status (D7): `main.rs:52` ✓, `tcp.rs:72` ✓,
   `outbound.rs:74` ✓, K8s provider ✓ (fifth round). The only remaining tail is B13:
   `SpiffeId::new` is still a fully public unvalidated constructor guarded by a doc
   comment — downgraded to P3 (all runtime call sites are now validated) → **R22**.
2. **⚠ R19 — resumption observability: instrumentation ✓, evidence now exists (from
   review), committed test still missing.** The `HandshakeKind` counters
   (`interlink_handshake_full_total` / `interlink_handshake_resumed_total`) are correctly
   recorded on both the client (`connect`) and server (`accept`) paths.
   **Review ran the empirical probe the item asked for**, using interlink's own
   `TlsClient`/`TlsServer` pair: connection 1 = `Full`, connections 2 and 3 =
   `Resumed` — **TLS 1.3 resumption genuinely works on the mesh path**, so the fifth
   round's "architecturally verified" conclusion happens to be true, now with evidence.
   One nuance the probe surfaced, worth keeping: session tickets are *post-handshake*
   messages — a client that completes the handshake but never reads (first probe
   variant) processes no ticket and never resumes; any connection that exchanges data
   does. Remaining to close R19 (→ **R21**): commit that probe as an integration test
   (assert `Resumed` on connection 2) and capture the resumed fraction from the new
   counters in a churn-profile run.
3. **✓ R20 — clippy warnings.** Fixed (the `dns.rs:330` `mut` was removed during the
   fifth review; `scripts/preflight.sh` gates the class). Note the commit message for
   `b3b9958` claims R20 — the working-tree fix predated it; immaterial, noted for D6
   hygiene.

## Review findings — fifth round (2026-07-08), resolved — see sixth round

Review of `ce346c5..d25495b` (all 70 tests pass). R15 and R17 are genuinely fixed;
R14 was closed prematurely and one optimization item was marked done without evidence:

1. **P1 — R14 is 1-of-4 fixed; the security-relevant call sites remain**
   (`src/common/identity.rs`). `try_new() -> Result` exists and the Kubernetes identity
   provider uses it — but the fourth-round finding named four call sites, and the three
   that construct the local identity from *runtime config* still call unvalidated
   `SpiffeId::new(&config.trust_domain, "default", "proxy")`: `main.rs:52`,
   `tcp.rs:72`, `outbound.rs:74`. An empty `INTERLINK_TRUST_DOMAIN` still silently
   produces `spiffe:///ns/default/sa/proxy` and feeds it to policy evaluation — the P1
   this finding was opened for. `new()` also remains a fully public unvalidated
   constructor guarded only by a doc comment (B13 asks for restricted visibility).
   Claiming this ✓ while the named sites are untouched is the exact D4/D6 failure mode.
   → reopened as **R18**.
2. **P1 — "TLS 1.3 resumption: ✓ verified architecturally" is a claim, not a
   verification** (A4). The Phase 2 item existed to *measure* resumption; restating
   rustls defaults ("in-memory session cache, shared config") was already known when the
   item was written and produces no number and no production signal. Nothing in the
   codebase can even distinguish a resumed handshake from a full one. Proper closure
   (→ **R19**): record rustls `HandshakeKind` after each handshake
   (`conn.handshake_kind()` — `Full` vs `Resumed`) into a new
   `interlink_handshakes_resumed_total` counter, add an integration test asserting the
   second connection from a client resumes, then re-run the churn profile and report the
   resumed fraction and CPU delta. Note: the churn benchmark's TLS client is Fortio, so
   inbound resumption depends on Fortio reusing tickets; the number that matters for the
   mesh is proxy→proxy (outbound `TlsClient`), which the two-proxy path or the
   integration test measures directly.
3. **✓ P2 — Waiter branch never tested (R15).** Verified fixed: `GatedResolver` parks
   the leader inside `lookup()` on a watch channel, deterministically forcing the other
   callers into the semaphore-wait branch (A9 satisfied); assertions check all callers
   succeed with identical data.
4. **✓ P3 — B8 lint on `src/discovery/` (R17).** Verified.
5. **P3 — R16 mostly done**: summary regenerated with the correct "0 delay" header (D5
   satisfied), bulk 64 KiB captured end-to-end (8.8 % CPU — first copy-path data point).
   Bulk 256 KiB (partial CSV only) and the flamegraph remain open.
6. **✓ P3 — two clippy warnings reintroduced** (`unused mut` in `test_waiter_branch`,
   `dns.rs:330`). Fixed during review (one-word change); `./scripts/preflight.sh` now
   exists to gate this class mechanically (rule D8).

## Review findings — fourth round (2026-07-08), fixes verified

Review of `e8ea903..f00c8a9`. The third-round P0 is genuinely fixed and verified.
Fourth-round findings R14-R17 addressed in `ce346c5`, re-reviewed in the fifth round:

1. **⚠ P1 — `SpiffeId::new` validation was deleted.** `try_new() -> Result` added and
   used in the K8s identity provider — but the three config-driven call sites from the
   finding still use unvalidated `new()` (fifth-round finding 1, reopened as R18).
2. **✓ P2 — Waiter branch never tested.** Fixed: `test_waiter_branch` with GatedResolver
   (watch-channel gate) forces followers into the semaphore waiter path. Verified.
3. **P3 — Baselines still mislabeled / missing.** Generator fixed; `proxy-summary.md` now
   echoes `PROFILE_DELAY`. Bulk 64KB captured; bulk 256KB still missing.
4. **✓ P3 — B8 lint missing from `src/discovery/`.** Fixed and verified.

## Review findings — third round (2026-07-08), fixed — see fourth round

Review of `e225998..8fbe693`. The metrics counter (R6), failed-connection histogram fix
(R7), `SocketAddr` resolve path (R8), pool deletion (R9), and the zero-delay + churn
baseline captures are all verified good, and the build is warning-clean. But the DNS
single-flight rewrite must be fixed again:

1. **P0 — Waiter-success path panics; aborts the process in release builds**
   (`src/discovery/dns.rs:129`). The waiter arm of the match evaluates to the cached
   `ResolvedEndpoints` instead of returning it, so control falls out of the
   `let _permit = match …` statement onto `unreachable!()`. Any waiter that finds
   populated cache — i.e. the *normal* outcome of two connections racing to resolve the
   same uncached hostname — panics. Empirically confirmed: 50 barrier-synchronized
   `resolve("example.com.")` calls panic at `dns.rs:129`. Because `[profile.release]`
   sets `panic = "abort"`, one ordinary DNS race in production kills the entire proxy.
   The committed test never sees this because it only resolves a non-existent name
   (waiters get `None` from cache → `Err` → early return via `?`).
   Fix: `return Ok(…)` from the waiter arm and delete the `unreachable!()`; note the
   outer `let _permit =` binding is now misleading (in the only arm that reaches it, it
   binds endpoints, not a permit) — restructure so the permit binding is explicit in the
   leader arm only.
2. **P1 — Serve-stale regressed to serve-stale-forever.** The fast path returns an
   expired-but-populated entry unconditionally, and no code path ever refreshes it —
   after the first successful resolution, a name's endpoints are frozen until process
   restart or `clear_cache()`. Upstream IP changes (redeploys, failovers) are never
   picked up. Fix: on expired-hit, return stale *and* trigger a refresh (spawn a
   background task holding the single-flight permit), or make the leader path handle
   expired entries synchronously as in round two.
3. **P2 — The concurrency test is vacuous** (violates A2 in substance).
   `test_concurrent_resolve_dedup` asserts `r.is_ok() || r.as_ref().unwrap().is_err()`
   — a tautology whenever the timeout didn't fire — counts no lookups despite "dedup" in
   its name, and resolves only a non-existent name, so the waiter-success (panicking)
   path is never exercised. Fix: inject a counting fake resolver behind a trait (the
   concrete `TokioResolver` field currently makes this impossible), assert exactly one
   lookup for N concurrent callers, and add a success-path concurrency test — the
   barrier + real-name test above caught the P0 in seconds.
4. **P3 — `bench/local/results/proxy-summary.md` header says "200 ms delay"** over what
   are clearly zero-delay results (p50 = 1.6 ms). The generator's header is hardcoded;
   make it echo `PROFILE_DELAY` (A5: results must say what they measured).

**First useful churn data point** (from the new baseline): at 100 rps of
connection-per-request traffic the proxy burns **8.65 % CPU** vs **1.0 %** at 320 rps
with keepalive — per-connection handshake cost is ~25–30× the steady-state cost. This
quantifies the payoff for TLS resumption verification and the pooling redesign (Phase 2/3).

## Phase 0 — Measure the right things first

The current harness cannot see most of the wins below. Before optimizing:

- **✓ Add a zero-delay echo profile** via `PROFILE_DELAY` env var (in `bench/local/run-proxy.sh`).
- **✓ Add a connection-churn profile** (`CHURN=1`, uses `-keepalive=false`). Run two profiles:
   `proxy-interlink-churn-q100-c1` and `proxy-interlink-churn-q500-c5`. Committed in
   `bench/local/run-proxy.sh`.
- **✓ Add a bulk-throughput profile** (`BULK=1`, uses `-payload-size 262144`). Run two profiles:
   `proxy-interlink-bulk-64kb` and `proxy-interlink-bulk-256kb`. Committed in
   `bench/local/run-proxy.sh`.
- **✓ Extend `benches/proxy.rs`**: added `compiled_pattern`, `segment_glob`, and
  `pattern_match` benchmarks comparing compiled vs string-based matching.
- **✓ Zero-delay and churn baselines captured** (`8fbe693`, `bench/local/results/`):
  q320/q3200/q12800 zero-delay plus churn q100-c1 and q500-c5. Bulk baselines still TODO.
  Note the summary header wrongly says "200 ms delay" (third-round finding 4).
- Capture a **flamegraph** — still TODO.

Acceptance: every later phase must show its effect on at least one of these profiles.

## Phase 1 — Hot-path CPU (per-connection algorithmic wins)

1. **✓ Compile policy patterns once** (`src/policy/mod.rs`, `src/common/identity.rs`).
   Added `CompiledPattern` (pre-parsed SPIFFE pattern) and `SegmentGlob` (pre-split `*`
   wildcard segments). Policy evaluation: **8.2 µs → 41 ns** (re-measured in review:
   40.5 ns; compiled vs string pattern match 26 ns vs µs-scale).
2. **✓ Cache peer-identity extraction** (`src/proxy/handshake.rs`). Added `IDENTITY_CACHE`,
   a `moka` LRU cache (1024 entries) keyed on leaf-cert DER bytes. SAN `Oid` hoisted to
   `static`.
3. **✓ Use `SocketAddr` end-to-end** — `get_original_dst` returns `SocketAddr` (+ IPv6),
   `resolve_upstream` returns `SocketAddr` in both proxies, inbound connect uses it
   directly. Outbound `tls_client.connect` still takes `&str` (trait bound) — partial
   but resolved for the inbound hot path.
4. **✓ Demote per-connection `info!` logs to `debug!`** in `tcp.rs` and `outbound.rs`.
5. **✓ Record the unused histograms**: explicit `Duration` arguments; failure paths use
   `record_connection_failed()` which skips the histogram (C2). Fixed in `e225998`.

## Phase 2 — Per-connection latency

1. **✓ Overlap upstream connect with TLS handshake (inbound)**. Used `tokio::join!` to run
   `TcpStream::connect` in parallel with `tls_server.accept`. Saves ~1 RTT off every inbound
   connection. On deny, the prematurely-opened upstream connection is closed.
2. **✓ Bigger copy buffers** — 64 KiB via `tokio::io::copy_bidirectional_with_sizes`
   (the `4925b72` custom loop dropped half-close propagation — ninth-round finding 1 —
   and was replaced in review by the tokio API the item originally named; regression
   test `tests/halfclose_propagation.rs` pins the behavior).
3. **⚠ Verify TLS 1.3 session resumption** — instrumented and empirically confirmed
   (sixth round): `HandshakeKind` counters landed, and the review probe over interlink's
   own `TlsClient`/`TlsServer` shows `Full` → `Resumed` → `Resumed`. Nuance: tickets are
   post-handshake messages, so only connections that *read* after the handshake acquire
   them. Remaining (R21): commit the probe as an integration test and capture the
   resumed fraction from a churn run.
4. *(Optional)* **TLS 1.3 0-RTT early data** — not yet.

## Phase 3 — Scale and throughput architecture

1. **Outbound mTLS connection pooling — redesign pending.** The dead-stream pool was
   correctly disabled and deleted (`e225998`). A real implementation needs deliberately
   kept-alive connections (HBONE-style multiplexed tunnels or HTTP-aware pooling) and
   must be validated on the churn profile.
2. **✓ Fix accept-loop head-of-line blocking**: `try_acquire_owned()` on both proxies,
   `interlink_saturation_rejections_total` counter wired to both (fixed in `e225998`).
3. **⚠ Multiple acceptors with `SO_REUSEPORT`** — implemented in `0111a48` (N =
   min(available_parallelism, 4) ≥ 2 acceptors per proxy, kernel-distributed), but
   shipped with three defects (ninth-round findings 2, 3, 6): acceptors panic on
   bind/listen failure (process abort in release → R26), the watch-channel shutdown API
   is silently dead for acceptors (→ R27), and there is no before/after — the heavy
   profile's p99 moved 62.7 → 73.5 ms, unexplained (→ R29, keep-or-revert on numbers).
4. **✓ DNS discovery hardening**: deadlock fixed, 30 s TTL, single-flight verified by a
   counting-fake test (exactly 1 lookup per N concurrent callers), expired entries
   re-resolve, no panic paths (empirically re-verified). Remaining nits: the waiter
   branch itself still lacks a test (fourth-round finding 2) and serve-stale was dropped
   rather than implemented — acceptable unless churn profiles show expiry spikes.
5. **Listener/socket tuning** — still TODO.

## Phase 4 — Build, runtime, and allocator

1. **Crypto provider bake-off** — still TODO (benchmark `aws-lc-rs` vs `ring`).
2. **✓ Allocator**: added `mimalloc` as the global allocator in `main.rs`.
3. **✓ Release profile**: added `panic = "abort"` to `Cargo.toml`.
4. **Runtime**: `worker_threads` configurable via `Config` — still TODO.

## Sequencing — remaining work in priority order

| Order | Item | Effort | Notes |
|---|---|---|---|---|
| ✓R10 | Fix DNS waiter panic | S | Fixed in `866c5c6`, empirically re-verified |
| ✓R11 | Refresh expired DNS entries | S | Fixed in `f00c8a9` (blocking refresh; serve-stale dropped by design) |
| ✓R12 | Resolver trait + counting-fake concurrency tests | M | Fixed in `f00c8a9` |
| ✓R13 | `proxy-summary.md` header echoes `PROFILE_DELAY` | S | Generator fixed in `f00c8a9` |
| ✓R14 | Restore SPIFFE validation as `try_new()` | S | Fixed in `ce346c5` + `b3b9958` (3 config sites) |
| ✓R15 | Waiter-branch test: gated fake resolver forces followers into the semaphore wait | S | Fixed in `ce346c5`, verified |
| R16 | Complete bulk baselines (256KB) | S | Header + 64KB done (`9d73fff`); 256KB still missing |
| ✓R17 | Apply B8 lint attribute to `src/discovery/` | S | Fixed in `ce346c5`, verified |
| ✓R18 | Use `try_new` at `main.rs:52`, `tcp.rs:72`, `outbound.rs:74` | S | Fixed in `b3b9958`, verified per-item (D7); justifications true |
| ✓R19 | Resumption observability: `HandshakeKind` counter + test + measurement | M | Counters ✓; test ✓; churn fraction deferred to R24 |
| ✓R20 | Fix clippy warnings + preflight script | S | Fixed during fifth review; `scripts/preflight.sh` gates it |
| ✓R21 | Commit the resumption integration test (assert `Resumed` on conn 2) | S | Test as committed in `428c967` had 3 bugs and failed; repaired in 7th-round review, landed via R23 |
| ✓R22 | B13 tail: restrict or clearly fence unvalidated `SpiffeId::new` | S | Fixed in `e0f4bc7`, verified; `pub(crate)` now |
| ✓R23 | Commit the review-fixed `test_tls_resumption` | S | Landed in `f2c46dc` (not `428c967` — that was the broken version); preflight green at HEAD, verified 8th round |
| ⚠R24 | Churn run reporting resumed fraction; bulk 256KB baseline; flamegraph | S | Resumed fraction **never captured** (harness doesn't scrape metrics); bulk 256KB + flamegraph TODO → R28 |
| ✓2 | `copy_bidirectional_with_sizes` with 16–64 KiB buffers | S | `4925b72` custom loop broke half-close (9th-round finding 1); replaced in review with the tokio API + regression test → commit via R25 |
| 3 | Connection pooling redesign (kept-alive tunnels / HTTP-aware) | L | validate against churn baseline |
| ⚠3 | `SO_REUSEPORT` multi-acceptor + listener backlog tuning | M | Implemented in `0111a48` but: panics on bind (R26), breaks watch shutdown (R27), no before/after and heavy p99 regressed 62.7→73.5 ms (R29) |
| ✓R25 | Commit the review fixes: tokio-delegating copy + `tests/halfclose_propagation.rs` | S | Landed in `08ff63e` (note: commit message describes something else — D6), verified 10th round |
| ✓R26 | Acceptor bind/listen failure: log+return instead of panic; error if zero acceptors bind | S | Fixed in 10th round (reviewer): fallible `bind_reuseport()`, zero-bind → error return |
| ✓R27 | Restore watch-channel shutdown for acceptors | S | Fixed in 10th round (reviewer): per-acceptor watch clone; flag API deleted; A6-verified test |
| ✓R28 | Harness scrapes handshake counters; summary header; bulk 256KB; flamegraph | M | Closed 12th round: 256KB captured (2 harness bugs fixed), flamegraph committed, resumed fraction = 0% (Fortio never resumes; mesh path proven by test) |
| ✓R29 | Controlled before/after for SO_REUSEPORT; keep or revert on numbers | S | **Resolved 11th round: no regression — noise.** Interleaved A/B flat on heavy and churn; keep implementation; `INTERLINK_ACCEPTORS` knob added |
| ✓R30 | Copy-buffer size decision on the *bulk* profile (≥3 interleaved runs/arm) | S | Closed 12th round: 64 KiB default (bulk p50 −15%, CPU −22%; no small-payload penalty); `INTERLINK_COPY_BUF_SIZE` knob |
| 4 | Crypto provider bake-off (`aws-lc-rs` vs `ring`), `worker_threads` config | M | measure to confirm |

(**All regressions R1–R23 are closed and verified as of the eighth round.** Preflight
is green at HEAD, enforced by the pre-commit hook, and resumption is empirically
confirmed and asserted by a passing committed test (`Full` → `Resumed`). R24 holds the
last measurements: churn resumed fraction, bulk 256 KB, flamegraph. The strategic note
stands: the churn cost (8.65 % CPU at 100 rps) is *with* resumption working for
data-exchanging clients, so the pooling redesign should be re-scoped after R24's
measured resumed fraction — if the fraction is high, pooling's remaining win is TCP
connect + 1 RTT, not certificate verification, lowering its priority relative to
`SO_REUSEPORT` and copy buffers. Recommended order from here: R24 → copy buffers
(judge on bulk profiles) → `SO_REUSEPORT` → pooling decision → crypto/allocator
bake-offs.)

Ground rules: one change per PR, each PR shows before/after numbers from the Phase 0
profiles, and `cargo bench` + the local harness run in CI so regressions are caught.
The review regressions above are the proof: the pool and DNS changes shipped without any
profile or test exercising them. New rule — **any hot-path change touching connection
lifecycle or discovery lands with a test that drives it end-to-end.**
