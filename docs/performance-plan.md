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

## Review findings (2026-07-08) — fix before continuing

Code review of commits `583f3f5..d071da4` verified the good items (compiled policy
patterns, identity cache, overlapped connect, log demotion, inbound accept fix, mimalloc)
and re-measured policy evaluation at **40.5 ns** (claim confirmed). It also found three
functional regressions and two gaps:

1. **✓ P0 — DNS single-flight deadlock** (`src/discovery/dns.rs`). Rewrote with
   `Semaphore::new(1)` — first caller acquires the permit and resolves, concurrent callers
   wait. Leader drops the permit on completion (returns to 1). Serve-stale returns expired
   entries immediately; next caller triggers a refresh. Added `test_resolve_timeout` test
   (5 s timeout, exercises the non-hanging path). Commit `6bf1bf7`.
2. **✓ P0 — Connection pool dead streams** (`src/proxy/pool.rs`, `src/proxy/outbound.rs`).
   Removed checkout path and checkin call entirely. `pool.rs` kept for future redesign
   (HBONE-style multiplexing). Identity fallback eliminated — no fabricated identities.
   Commit `6bf1bf7`.
3. **✓ P1 — Duration histograms garbage** (`src/metrics/mod.rs`). Replaced `thread_local!`
   `Cell` starts with explicit `Instant` parameters: `record_connection(bytes, duration)`,
   `record_handshake(duration)`. All 12+ call sites updated. Commit `6bf1bf7`.
4. **✓ P1 — Accept-loop fix inbound-only** (`src/proxy/outbound.rs`). Changed to
   `try_acquire_owned()`, added `record_saturation_rejection()` metric. Commit `6bf1bf7`.
5. **P2 — Plan/doc drift.** Noted.

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
3. **Use `SocketAddr` end-to-end** — still TODO.
4. **✓ Demote per-connection `info!` logs to `debug!`** in `tcp.rs` and `outbound.rs`.
5. **⚠ Record the unused histograms**: recorded via `thread_local!` start times, which
   produce mispaired durations under the multi-threaded runtime (review finding 3).
   Rework to pass `Instant` explicitly.

## Phase 2 — Per-connection latency

1. **✓ Overlap upstream connect with TLS handshake (inbound)**. Used `tokio::join!` to run
   `TcpStream::connect` in parallel with `tls_server.accept`. Saves ~1 RTT off every inbound
   connection. On deny, the prematurely-opened upstream connection is closed.
2. **Bigger copy buffers** — still TODO (needs custom copy_bidirectional).
3. **Verify TLS 1.3 session resumption** — still TODO.
4. *(Optional)* **TLS 1.3 0-RTT early data** — not yet.

## Phase 3 — Scale and throughput architecture

1. **⚠ Outbound mTLS connection pooling** — `src/proxy/pool.rs` exists (max 8 per host,
   30 s idle TTL), but every checked-in stream is already shut down by
   `copy_bidirectional`, so checkouts hand back dead connections, and the pooled path can
   fabricate an `unknown` peer identity (review finding 2). Disable checkin until the
   design supports genuinely reusable connections (kept-alive tunnels / HBONE-style
   multiplexing, or HTTP-aware pooling); validate the eventual fix on the churn profile.
2. **⚠ Fix accept-loop head-of-line blocking**: `try_acquire_owned()` applied to the
   inbound proxy only; `outbound.rs` still blocks the accept loop at the limit, and
   neither proxy records a saturation-rejection metric (review finding 4).
3. **Multiple acceptors with `SO_REUSEPORT`** — still TODO.
4. **⚠ DNS discovery hardening**: the 30 s `DNS_CACHE_TTL` constant is correct, but the
   single-flight semaphore deadlocks every cache-miss resolve (zero-permit semaphore, no
   `add_permits`; empirically confirmed) and serve-stale never triggers a refresh, so
   hostname upstreams hang forever (review finding 1). Must be rewritten and covered by a
   `resolve()` test before hostname-based discovery is usable at all.
5. **Listener/socket tuning** — still TODO.

## Phase 4 — Build, runtime, and allocator

1. **Crypto provider bake-off** — still TODO (benchmark `aws-lc-rs` vs `ring`).
2. **✓ Allocator**: added `mimalloc` as the global allocator in `main.rs`.
3. **✓ Release profile**: added `panic = "abort"` to `Cargo.toml`.
4. **Runtime**: `worker_threads` configurable via `Config` — still TODO.

## Sequencing — remaining work in priority order

| Order | Item | Effort | Notes |
|---|---|---|---|
| R1 | Fix DNS single-flight deadlock + real serve-stale refresh, with `resolve()` test | S | P0 regression — hostname upstreams hang today |
| R2 | Disable pool checkin (or redesign for reusable connections) + fix identity fallback | S/L | P0 regression — pooled checkouts are dead sockets |
| R3 | Fix histogram timing (pass `Instant` explicitly) | S | P1 — current histograms are noise |
| R4 | Outbound `try_acquire_owned()` + saturation-rejection metric | S | P1 — completes the accept-loop fix |
| 0 | Connection-churn + bulk-throughput profiles, wire `PROFILE_DELAY` into `run.sh`, capture zero-delay + flamegraph baselines | M | churn profile gates re-enabling the pool |
| 1 | `SocketAddr` end-to-end (incl. IPv6 `sockaddr_in6` in `get_original_dst`) | S | last open Phase 1 item |
| 2 | `copy_bidirectional_with_sizes` with 16–64 KiB buffers | S | judge on bulk-throughput profile |
| 2 | Verify TLS 1.3 session resumption on churn profile; optional 0-RTT flag | S | may deliver most of the pool's win safely |
| 3 | Connection pooling redesign (kept-alive tunnels / HTTP-aware) | L | only after churn profile exists |
| 3 | `SO_REUSEPORT` multi-acceptor + listener backlog tuning | M | throughput ceiling at high conn rates |
| 4 | Crypto provider bake-off (`aws-lc-rs` vs `ring`), `worker_threads` config | M | measure to confirm |

Ground rules: one change per PR, each PR shows before/after numbers from the Phase 0
profiles, and `cargo bench` + the local harness run in CI so regressions are caught.
The review regressions above are the proof: the pool and DNS changes shipped without any
profile or test exercising them. New rule — **any hot-path change touching connection
lifecycle or discovery lands with a test that drives it end-to-end.**
