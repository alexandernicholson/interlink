# Performance Uplift Plan

Status: **in progress (2026-07-08)**. Items marked ✓ are implemented and merged;
items marked ⚠ were implemented but found defective in the 2026-07-08 code review
(see "Review findings" below) and must be fixed before further optimization work.
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

## Review findings — second round (2026-07-08), all fixed

Re-review of `6bf1bf7..08c2223` — all second-round defects fixed in commits
`e225998..2b129a8`. Verified warning-clean, all tests pass:

1. **✓ P1 — DNS single-flight is a no-op** (`src/discovery/dns.rs`). Fixed in commit
   `2b129a8`. Semaphore permit now bound to `_permit` across `resolve_inner` via
   `match sem.try_acquire() { Ok(p) => p, Err(_) => /* wait */ }` (B1). Added
   `test_concurrent_resolve_dedup`: 10 concurrent calls for the same non-existent name
   all complete in <5s (A2, A3).
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
2. **Bigger copy buffers** — still TODO (needs custom copy_bidirectional).
3. **Verify TLS 1.3 session resumption** — still TODO.
4. *(Optional)* **TLS 1.3 0-RTT early data** — not yet.

## Phase 3 — Scale and throughput architecture

1. **Outbound mTLS connection pooling — redesign pending.** The dead-stream pool was
   correctly disabled and deleted (`e225998`). A real implementation needs deliberately
   kept-alive connections (HBONE-style multiplexed tunnels or HTTP-aware pooling) and
   must be validated on the churn profile.
2. **✓ Fix accept-loop head-of-line blocking**: `try_acquire_owned()` on both proxies,
   `interlink_saturation_rejections_total` counter wired to both (fixed in `e225998`).
3. **Multiple acceptors with `SO_REUSEPORT`** — still TODO.
4. **✓ DNS discovery hardening**: deadlock fixed; 30 s TTL correct; single-flight permit
   now held across `resolve_inner` (fixed in `2b129a8`); concurrency test covers N=10.
5. **Listener/socket tuning** — still TODO.

## Phase 4 — Build, runtime, and allocator

1. **Crypto provider bake-off** — still TODO (benchmark `aws-lc-rs` vs `ring`).
2. **✓ Allocator**: added `mimalloc` as the global allocator in `main.rs`.
3. **✓ Release profile**: added `panic = "abort"` to `Cargo.toml`.
4. **Runtime**: `worker_threads` configurable via `Config` — still TODO.

## Sequencing — remaining work in priority order

| Order | Item | Effort | Notes |
|---|---|---|---|
| ✓R5 | Hold the single-flight permit across the DNS resolve; add concurrency test | S | Fixed in `2b129a8` |
| ✓R6 | Dedicated saturation-rejection counter, wired into both proxies | S | Fixed in `e225998` |
| ✓R7 | Skip `Duration::ZERO` histogram records on failure paths | S | Fixed in `e225998` |
| ✓R8 | Finish `SocketAddr` end-to-end (resolve/connect path, drop `.to_string()`) | S | Fixed in `e225998`; outbound `tls_client.connect` still takes `&str` (trait) |
| ✓R9 | Delete or feature-gate dead `pool.rs` | S | Fixed in `e225998` |
| 0 | Run + commit baseline results: zero-delay, `CHURN=1`, `BULK=1`; capture flamegraph; wire profiles into `run.sh` | M | profiles exist but no results are captured yet |
| 2 | `copy_bidirectional_with_sizes` with 16–64 KiB buffers | S | judge on bulk-throughput profile |
| 2 | Verify TLS 1.3 session resumption on churn profile; optional 0-RTT flag | S | may deliver most of the pool's win safely |
| 3 | Connection pooling redesign (kept-alive tunnels / HTTP-aware) | L | only after churn baseline exists |
| 3 | `SO_REUSEPORT` multi-acceptor + listener backlog tuning | M | throughput ceiling at high conn rates |
| 4 | Crypto provider bake-off (`aws-lc-rs` vs `ring`), `worker_threads` config | M | measure to confirm |

(R1–R4 from the first review round are done and verified; R5–R9 are the follow-ups from
the second round.)

Ground rules: one change per PR, each PR shows before/after numbers from the Phase 0
profiles, and `cargo bench` + the local harness run in CI so regressions are caught.
The review regressions above are the proof: the pool and DNS changes shipped without any
profile or test exercising them. New rule — **any hot-path change touching connection
lifecycle or discovery lands with a test that drives it end-to-end.**
