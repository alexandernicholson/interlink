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

## Review findings — fourth round (2026-07-08), current state

Review of `e8ea903..f00c8a9`. The third-round P0 is genuinely fixed and verified:

- **✓ R10 — waiter panic**: `unreachable!()` removed, both leader and waiter paths use
  early returns (B7/B9), the waiter's `acquire().await.unwrap()` became a mapped `Err`
  (B8). Empirically re-verified: 50 barrier-synchronized concurrent resolves of a real
  name complete without panic (the exact reproducer that crashed round three).
- **✓ R11 — frozen endpoints**: expired entries now fall through to leader election and
  re-resolve, with the cache lifecycle documented as a state table (B10). Design note:
  serve-stale was *dropped*, not implemented — an expired entry costs one blocking lookup
  on the next caller. Correct and simple; revisit stale-while-revalidate only if the
  churn profile shows TTL-expiry latency spikes.
- **✓ R12 — resolver behind a trait**: `DnsResolver` trait, production `HickoryResolver`,
  and a `CountingResolver` fake (B11). `test_concurrent_resolve_dedup` now asserts
  **exactly 1 lookup** for 10 concurrent callers and identical results (A6/A7);
  a success-path concurrency test was added (A8). `ServiceDiscovery::new()` returns
  `Result` instead of unwrapping resolver construction.
- **✓ R13 — benchmark header**: generator now echoes `${PROFILE_DELAY:-200ms}`.
- **✓ B8 lints**: `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]`
  on all `src/proxy/` modules; `cargo clippy --all-targets` is clean.

Four residual findings:

1. **P1 — `SpiffeId::new` validation was deleted, not made fallible**
   (`src/common/identity.rs`). To satisfy the B8 lint, the
   `validate_segments().expect(…)` call was removed from `new()` — so
   `SpiffeId::new("", "", "")` now silently succeeds. Call sites construct the local
   identity from config (`main.rs:52`, `tcp.rs:72`, `outbound.rs:74`) and from
   runtime-derived Kubernetes metadata (`identity/provider/mod.rs:47`); an empty
   `trust_domain` previously failed loudly at startup, now it silently produces an
   invalid identity (`spiffe:///ns/…`) that participates in policy evaluation. This is
   fail-open in a security type (B5) and fixes the lint's letter while breaking the
   original property (D4). Fix: add `try_new() -> Result<Self, InterlinkError>` and use
   it at all config/runtime call sites (startup errors are correct there); `new()` may
   remain for compile-time-literal construction only if documented, or delegate to
   `try_new().expect()` inside `#[cfg(test)]`-adjacent code.
2. **P2 — The waiter branch is still never executed by a test.** The counting fake
   returns `Ready` without yielding, so on the single-threaded test runtime every
   follower task finds a fresh cache entry; nobody ever blocks on the semaphore. The
   dedup assertion passes via the cache-hit path, and the branch that panicked in round
   three has no test driving it (A1). Fix: give the fake a gated/delayed `lookup` (e.g.
   await a `tokio::sync::Notify` or a 5 ms sleep) so followers demonstrably enter the
   waiter path, then assert 1 lookup + N successes.
3. **P3 — The committed `proxy-summary.md` still says "200 ms delay" over zero-delay
   numbers**: it was generated by the pre-fix script (`e8ea903` predates `f00c8a9`).
   Regenerate the baselines with the fixed header so the artifact is honestly labeled
   (C6). Bulk baselines are also still missing — only a partial 64 KiB metrics CSV
   exists, with no Fortio results and no summary entries.
4. **P3 — The B8 lint attribute was not applied to `src/discovery/`** although the
   commit message claims proxy *and* discovery (A5). One line in `discovery/mod.rs` or
   `dns.rs`.

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
|---|---|---|---|
| ✓R10 | Fix DNS waiter panic | S | Fixed in `866c5c6`, empirically re-verified |
| ✓R11 | Refresh expired DNS entries | S | Fixed in `f00c8a9` (blocking refresh; serve-stale dropped by design) |
| ✓R12 | Resolver trait + counting-fake concurrency tests | M | Fixed in `f00c8a9` |
| ✓R13 | `proxy-summary.md` header echoes `PROFILE_DELAY` | S | Generator fixed in `f00c8a9`; committed artifact still mislabeled → R16 |
| ✓R14 | Restore SPIFFE validation as `SpiffeId::try_new() -> Result`, used at config/runtime call sites | S | Fixed in `ce346c5` |
| ✓R15 | Waiter-branch test: gated/delayed fake resolver forces followers into the semaphore wait | S | Fixed in `ce346c5` |
| R16 | Regenerate baselines with fixed header; complete bulk baselines (`BULK=1`) | S | Generator fixed in `ce346c5`; committed artifact still mislabeled → still TODO |
| ✓R17 | Apply B8 lint attribute to `src/discovery/` | S | Fixed in `ce346c5` |
| 0 | Bulk baselines (`BULK=1`) + flamegraph; wire profiles into `run.sh` | S | zero-delay + churn baselines are captured |
| 2 | Verify TLS 1.3 session resumption on churn profile; optional 0-RTT flag | S | churn baseline says handshakes are ~25–30× steady-state CPU — biggest measurable win |
| 2 | `copy_bidirectional_with_sizes` with 16–64 KiB buffers | S | judge on bulk-throughput profile |
| 3 | Connection pooling redesign (kept-alive tunnels / HTTP-aware) | L | validate against churn baseline |
| 3 | `SO_REUSEPORT` multi-acceptor + listener backlog tuning | M | throughput ceiling at high conn rates |
| 4 | Crypto provider bake-off (`aws-lc-rs` vs `ring`), `worker_threads` config | M | measure to confirm |

(R1–R4: first round, done. R5–R9: second round, done — R5's rewrite introduced
R10–R12. R10–R13: third round, done and verified. R14–R17: fourth-round follow-ups —
**R14 first**; it is the only one with security impact. After R14–R17, the regression
queue is clear and optimization work resumes with TLS resumption verification, which the
churn baseline (~25–30× handshake CPU) says is the biggest measurable win.)

Ground rules: one change per PR, each PR shows before/after numbers from the Phase 0
profiles, and `cargo bench` + the local harness run in CI so regressions are caught.
The review regressions above are the proof: the pool and DNS changes shipped without any
profile or test exercising them. New rule — **any hot-path change touching connection
lifecycle or discovery lands with a test that drives it end-to-end.**
