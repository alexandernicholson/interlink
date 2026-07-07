# Performance Uplift Plan

Status: **in progress (2026-07-08)**. Items marked ✓ are implemented and merged.
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

## Phase 0 — Measure the right things first

The current harness cannot see most of the wins below. Before optimizing:

- **✓ Add a zero-delay echo profile** to `bench/local/run.sh` (delay=0) via `PROFILE_DELAY` env var.
- **Add a connection-churn profile** (new connection per request) — still TODO.
- **Add a bulk-throughput profile** (64 KiB–1 MiB payloads) — still TODO.
- **✓ Extend `benches/proxy.rs`**: added `compiled_pattern`, `segment_glob`, and
  `pattern_match` benchmarks comparing compiled vs string-based matching.
- Capture a **flamegraph** — still TODO.

Acceptance: every later phase must show its effect on at least one of these profiles.

## Phase 1 — Hot-path CPU (per-connection algorithmic wins)

1. **✓ Compile policy patterns once** (`src/policy/mod.rs`, `src/common/identity.rs`).
   Added `CompiledPattern` (pre-parsed SPIFFE pattern) and `SegmentGlob` (pre-split `*`
   wildcard segments). Policy evaluation: **8.2 µs → 41 ns** (200× improvement).
2. **✓ Cache peer-identity extraction** (`src/proxy/handshake.rs`). Added `IDENTITY_CACHE`,
   a `moka` LRU cache (1024 entries) keyed on leaf-cert DER bytes. SAN `Oid` hoisted to
   `static`.
3. **Use `SocketAddr` end-to-end** — still TODO.
4. **✓ Demote per-connection `info!` logs to `debug!`** in `tcp.rs` and `outbound.rs`.
5. **✓ Record the unused histograms**: `connection_duration` and `handshake_duration` are
   now recorded via `thread_local!` start times.

## Phase 2 — Per-connection latency

1. **✓ Overlap upstream connect with TLS handshake (inbound)**. Used `tokio::join!` to run
   `TcpStream::connect` in parallel with `tls_server.accept`. Saves ~1 RTT off every inbound
   connection. On deny, the prematurely-opened upstream connection is closed.
2. **Bigger copy buffers** — still TODO (needs custom copy_bidirectional).
3. **Verify TLS 1.3 session resumption** — still TODO.
4. *(Optional)* **TLS 1.3 0-RTT early data** — not yet.

## Phase 3 — Scale and throughput architecture

1. **✓ Outbound mTLS connection pooling** — added `src/proxy/pool.rs`. Per-host idle pool
   (max 8 per host, 30s idle TTL). On checkout, returns a warm TLS stream; on checkin,
   returns clean streams to the pool. Avoids TCP + full mTLS handshake on repeat connections.
2. **✓ Fix accept-loop head-of-line blocking**: replaced `acquire_owned().await` with
   `try_acquire_owned()` — listener never stalls when connection limit is reached.
3. **Multiple acceptors with `SO_REUSEPORT`** — still TODO.
4. **✓ DNS discovery hardening**: replaced 5s timeout-as-TTL with proper 30s `DNS_CACHE_TTL`
   constant. Added per-name single-flight via `Semaphore` to prevent expiry stampedes.
   Serve-stale: expired entries returned immediately while refresh happens in background.
5. **Listener/socket tuning** — still TODO.

## Phase 4 — Build, runtime, and allocator

1. **Crypto provider bake-off** — still TODO (benchmark `aws-lc-rs` vs `ring`).
2. **✓ Allocator**: added `mimalloc` as the global allocator in `main.rs`.
3. **✓ Release profile**: added `panic = "abort"` to `Cargo.toml`.
4. **Runtime**: `worker_threads` configurable via `Config` — still TODO.

## Sequencing and expected impact

| Order | Item | Effort | Expected effect (profile) |
|---|---|---|---|
| 0 | Benchmarks + flamegraph baseline | S | enables everything below |
| 1 | Compiled policy patterns | S | large CPU cut per connection (churn) |
| 1 | Identity-extraction cache + static OID | S | CPU cut per connection (churn) |
| 1 | `SocketAddr` end-to-end, log demotion | S | small CPU cut, less alloc churn |
| 2 | Overlapped upstream connect | M | −0.5–1 RTT p50/p99 (zero-delay) |
| 2 | 16–64 KiB copy buffers | S | syscalls halved (bulk throughput) |
| 3 | Outbound connection pooling | L | biggest win on churn: handshake CPU + latency |
| 3 | Accept-loop fix + SO_REUSEPORT | M | throughput ceiling at high conn rates |
| 3 | DNS single-flight + real TTL | S | removes stampede latency spikes |
| 4 | Crypto provider / allocator / profile | M | 5–20 % CPU, measure to confirm |

Ground rules: one change per PR, each PR shows before/after numbers from the Phase 0
profiles, and `cargo bench` + the local harness run in CI so regressions are caught.
