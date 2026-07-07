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

## Review findings — second round (2026-07-08), current blockers

Re-review of `6bf1bf7..08c2223` (all tests pass; dead-code warnings aside, the build is
clean). Remaining defects, none of which hang or corrupt traffic:

1. **P1 — DNS single-flight is a no-op** (`src/discovery/dns.rs`). The leader election
   reads `sem.try_acquire().map_err(|_| ()).err()`: when `try_acquire` succeeds, the
   `SemaphorePermit` inside the `Ok` is **discarded by `.err()` and dropped at the end of
   that expression**, immediately returning the permit. The semaphore therefore always has
   its permit available, every concurrent caller "wins" the election, and each one issues
   its own DNS lookup — the expiry stampede this was meant to prevent is still fully
   present (just no longer a deadlock). The serve-stale and waiter branches are effectively
   unreachable, and the waiter branch's `.forget()` would leak a permit if it ever ran.
   Fix: bind the permit for the duration of the resolve —
   `let _permit = match sem.try_acquire() { Ok(p) => p, Err(_) => /* waiter path */ }` —
   and add a test that counts lookups under N concurrent `resolve()` calls for one name
   (e.g. via a resolver trait fake), since `test_resolve_timeout` cannot see this.
2. **P2 — Saturation rejections counted as connections** (`src/metrics/mod.rs`).
   `record_saturation_rejection()` increments `connections_total`, silently inflating a
   counter that means "completed connections" everywhere else. Give it its own counter
   (`interlink_saturation_rejections_total`) and also call it from the inbound proxy's
   rejection branch (`tcp.rs`), which currently records nothing.
3. **P2 — Failure paths pollute the duration histogram.** Every early-return calls
   `record_connection(0, 0, Duration::ZERO)`, pushing zeros into
   `interlink_connection_duration_seconds` and dragging percentiles down under error load.
   Skip the histogram record when the connection never carried traffic (or use a separate
   failed-connections counter).
4. **P3 — Dead pool code.** `pool.rs` plus the `connection_pool`/`config` fields generate
   7 compiler warnings. Either delete it (it's in git history for the redesign) or
   `#[cfg(feature = "pool")]`-gate it; warnings that scroll past every build train people
   to ignore the one that matters.
5. **P2 — `SocketAddr` end-to-end is only half done** (commit `08c2223` overstates).
   `get_original_dst` now correctly returns `SocketAddr` (and gained IPv6 via
   `IP6T_SO_ORIGINAL_DST`), but both call sites immediately do `.map(|sa| sa.to_string())`,
   and `resolve_upstream`/`TcpStream::connect` still work on `String` — so the
   format/re-parse cost this item exists to remove is still paid on every connection.
   Finish by making `default_upstream` parsing, `resolve_upstream`, and the connect calls
   `SocketAddr`-typed, keeping `String` only for hostname upstreams that need DNS.

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
3. **⚠ Use `SocketAddr` end-to-end** — half done: `get_original_dst` returns
   `SocketAddr` (+ IPv6), but callers convert straight back to `String` and the
   resolve/connect path is still string-typed (second-round finding 5).
4. **✓ Demote per-connection `info!` logs to `debug!`** in `tcp.rs` and `outbound.rs`.
5. **✓ Record the unused histograms**: now recorded with explicit `Duration` arguments
   (first-round finding 3 fixed). Remaining nit: don't record `Duration::ZERO` on failure
   paths (second-round finding 3).

## Phase 2 — Per-connection latency

1. **✓ Overlap upstream connect with TLS handshake (inbound)**. Used `tokio::join!` to run
   `TcpStream::connect` in parallel with `tls_server.accept`. Saves ~1 RTT off every inbound
   connection. On deny, the prematurely-opened upstream connection is closed.
2. **Bigger copy buffers** — still TODO (needs custom copy_bidirectional).
3. **Verify TLS 1.3 session resumption** — still TODO.
4. *(Optional)* **TLS 1.3 0-RTT early data** — not yet.

## Phase 3 — Scale and throughput architecture

1. **Outbound mTLS connection pooling — reverted, redesign pending.** The dead-stream
   pool was correctly disabled (first-round finding 2 fixed). A real implementation needs
   deliberately kept-alive connections (HBONE-style multiplexed tunnels or HTTP-aware
   pooling) and must be validated on the churn profile. Delete or feature-gate the dead
   `pool.rs` in the meantime (second-round finding 4).
2. **✓ Fix accept-loop head-of-line blocking**: `try_acquire_owned()` now on both
   proxies. Remaining nit: the rejection metric increments `connections_total` instead of
   a dedicated counter, and the inbound proxy records nothing (second-round finding 2).
3. **Multiple acceptors with `SO_REUSEPORT`** — still TODO.
4. **⚠ DNS discovery hardening**: deadlock fixed and tested; 30 s TTL correct. But the
   single-flight leader election drops its permit immediately, so concurrent cache-miss
   resolves still stampede and serve-stale is unreachable (second-round finding 1).
5. **Listener/socket tuning** — still TODO.

## Phase 4 — Build, runtime, and allocator

1. **Crypto provider bake-off** — still TODO (benchmark `aws-lc-rs` vs `ring`).
2. **✓ Allocator**: added `mimalloc` as the global allocator in `main.rs`.
3. **✓ Release profile**: added `panic = "abort"` to `Cargo.toml`.
4. **Runtime**: `worker_threads` configurable via `Config` — still TODO.

## Sequencing — remaining work in priority order

| Order | Item | Effort | Notes |
|---|---|---|---|
| R5 | Hold the single-flight permit across the DNS resolve; add concurrency test | S | P1 — stampede protection currently a no-op (2nd-round finding 1) |
| R6 | Dedicated saturation-rejection counter, wired into both proxies | S | P2 — rejections currently inflate `connections_total` (finding 2) |
| R7 | Skip `Duration::ZERO` histogram records on failure paths | S | P2 — error load skews latency percentiles (finding 3) |
| R8 | Finish `SocketAddr` end-to-end (resolve/connect path, drop `.to_string()`) | S | P2 — finding 5; `get_original_dst` half is done |
| R9 | Delete or feature-gate dead `pool.rs` | S | P3 — 7 warnings (finding 4) |
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
