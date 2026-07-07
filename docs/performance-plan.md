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

1. **P0 — DNS single-flight deadlocks on every cache miss** (`src/discovery/dns.rs`).
   The in-flight guard is `Semaphore::new(0)`: the first caller's `try_acquire()` fails
   (zero permits), so *no* caller ever becomes the resolver, everyone parks on
   `acquire().await`, and nothing calls `add_permits`. Confirmed empirically:
   `ServiceDiscovery::new().resolve("localhost")` hangs past a 10 s timeout. Any hostname
   upstream now hangs forever (the benchmarks use literal socket addresses, which is why
   this wasn't caught). Additionally, the "serve stale" fast path returns expired entries
   unconditionally and never schedules a refresh, so even if resolution worked, a cached
   name would never be re-resolved. Fix: single-flight where the *first* caller resolves
   (e.g. `Semaphore::new(1)` acquire-as-leader, or per-name `tokio::sync::OnceCell` /
   `watch`), leader wakes waiters, and expired hits spawn a background refresh task.
   Add a `resolve()` integration test with a timeout.
2. **P0 — Connection pool stores dead streams** (`src/proxy/pool.rs`,
   `src/proxy/outbound.rs`). `copy_bidirectional` shuts down each side after the opposite
   side reaches EOF — by the time the copy returns `Ok`, the TLS stream has had
   `poll_shutdown` driven (close_notify sent) and is not reusable. Every checked-in stream
   is dead, so the "warm" checkout path hands out closed connections and the first proxied
   request on them fails. The pool also never validates liveness at checkout, and the
   pooled path fabricates `spiffe://unknown/ns/unknown/sa/unknown` when identity extraction
   fails instead of failing closed — that fabricated identity is fed to the policy engine.
   Fix: cache the peer identity alongside the pooled stream (don't re-extract, never
   fabricate), only pool streams that are demonstrably reusable — which for opaque
   TCP-in-TLS means *not* pooling after `copy_bidirectional` completes. Realistically this
   feature needs the connection to be kept alive deliberately (per-stream multiplexing à la
   HBONE, or pooling only protocol-aware HTTP upstreams). Until then, disable checkin;
   an idle pool of dead sockets is worse than no pool.
3. **P1 — Duration histograms record garbage** (`src/metrics/mod.rs`). Start times live in
   `thread_local!` `Cell`s, but the proxy runs on the multi-threaded runtime: a task can
   start on one worker thread and finish on another (start never matched), and concurrent
   connections interleaving on the same worker clobber each other's `Cell`, pairing one
   connection's start with another's completion. `interlink_connection_duration_seconds`
   and `interlink_handshake_duration_seconds` are therefore noise. Fix: thread the
   `Instant` through explicitly (`record_connection(start, …)`) — the call sites already
   have `start` in scope.
4. **P1 — Accept-loop fix is inbound-only.** `outbound.rs:198` still does
   `acquire_owned().await` in the accept loop, so the outbound listener still stalls at the
   connection limit. Apply the same `try_acquire_owned()` change, and add the planned
   saturation-rejection counter metric (neither proxy records one).
5. **P2 — Plan/doc drift.** `PROFILE_DELAY` was added to `bench/local/run-proxy.sh`, not
   `run.sh` as previously claimed (corrected below). Also note `panic = "abort"` in
   `[profile.release]` is ignored by cargo for `cargo test/bench --release` builds
   (harness needs unwinding) — expected, but don't be surprised by the warning.

## Phase 0 — Measure the right things first

The current harness cannot see most of the wins below. Before optimizing:

- **✓ Add a zero-delay echo profile** via `PROFILE_DELAY` env var (in `bench/local/run-proxy.sh`;
  `run.sh` not yet wired). No zero-delay results captured in `bench/results/` yet.
- **Add a connection-churn profile** (new connection per request) — still TODO, and now the
  top measurement priority: it is the profile that would have caught the dead-stream pool
  (finding 2) and is the one the pool/resumption work must be judged against.
- **Add a bulk-throughput profile** (64 KiB–1 MiB payloads) — still TODO.
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
