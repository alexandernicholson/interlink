# Performance Uplift Plan

Status: proposed (2026-07-08). Grounded in a full read of the per-connection hot path
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

- **Add a zero-delay echo profile** to `bench/local/run.sh` (delay=0, small payload) so proxy
  CPU/latency overhead dominates instead of the 200 ms sleep.
- **Add a connection-churn profile** (new connection per request, e.g. Fortio
  `-keepalive=false`) to measure handshake rate — this is where the mesh actually burns CPU.
- **Add a bulk-throughput profile** (64 KiB–1 MiB payloads) to expose copy-buffer and syscall
  costs.
- **Extend `benches/proxy.rs`**: policy evaluation with realistic rule counts, SPIFFE
  identity extraction from a real cert DER, and an end-to-end in-process
  handshake+proxy benchmark (loopback TlsServer↔TlsClient). The current
  `memory_copy` bench measures `memcpy`, not the proxy.
- Capture a **flamegraph** (`cargo flamegraph` against the zero-delay profile) as the
  baseline artifact in `bench/results/`.

Acceptance: every later phase must show its effect on at least one of these profiles.

## Phase 1 — Hot-path CPU (per-connection algorithmic wins)

1. **Compile policy patterns once** (`src/policy/mod.rs`, `src/common/identity.rs`).
   Parse each `PolicyRule` pattern at insert time into a
   `CompiledPattern { trust_domain: String, ns: SegmentGlob, sa: SegmentGlob }` and match
   against that. Eliminates 2×N `Url::parse` calls per connection. Expected: policy
   evaluation drops from ~µs×rules to tens of ns; this is the single largest pure-CPU win.
2. **Cache peer-identity extraction** (`src/proxy/handshake.rs`). Key a small LRU (or
   `DashMap` with cap) on the leaf-cert DER bytes → `SpiffeId`. Repeat connections from the
   same peer (the common case in a mesh) skip the X.509 parse entirely. Also hoist the SAN
   `Oid` into a `static`.
3. **Use `SocketAddr` end-to-end**. `get_original_dst` returns `SocketAddr` (and gains IPv6
   support via `sockaddr_in6` while we're there); `resolve_upstream` returns `SocketAddr`;
   `TcpStream::connect` takes it directly. Removes all per-connection address formatting,
   allocation, and re-parsing.
4. **Demote per-connection `info!` logs to `debug!`** in `tcp.rs` and `outbound.rs`
   (connection established / protocol detected / done). Keep lifecycle and error logs at
   `info!`/`warn!`.
5. **Record the unused histograms or delete them**: `connection_duration` and
   `handshake_duration` are registered but never recorded — record them (cheap, we already
   have `start`) so Phase 0/2 changes are observable in production.

## Phase 2 — Per-connection latency

1. **Overlap upstream connect with the TLS handshake (inbound)**. Today:
   handshake → read first bytes → connect upstream (serial). Change `tcp.rs::handle_connection`
   to `tokio::join!` the upstream `TcpStream::connect` with the TLS accept, and only *use*
   the upstream after policy allows (close it on deny). Saves one connect RTT + kernel
   connect latency off every inbound connection. Do the same on outbound: resolve + TCP
   connect already overlap naturally once resolution is cached.
2. **Bigger copy buffers**: switch to `copy_bidirectional_with_sizes` using
   `buffers::SOCKET_READ` (16 KiB, and consider 32–64 KiB after the bulk-throughput profile
   exists). Halves syscall count for streaming workloads.
3. **Verify TLS 1.3 session resumption is actually working** end-to-end (rustls enables an
   in-memory resumption store by default, and both `ClientConfig`/`ServerConfig` are built
   once and shared — but the churn benchmark should confirm resumed handshakes). If
   confirmed, a resumed handshake is ~1 RTT and no certificate verification.
4. *(Optional, behind a config flag)* **TLS 1.3 0-RTT early data** for resumed outbound
   connections — cuts a full RTT but has replay semantics; only for idempotent/TCP-opaque
   traffic, so default off.

## Phase 3 — Scale and throughput architecture

1. **Outbound mTLS connection pooling** — the largest architectural win. Every local
   connection currently costs a fresh TCP + full mTLS handshake to the upstream. Add a
   per-`(upstream SocketAddr, peer identity)` pool of idle TLS connections with idle timeout
   and max-per-host, so short-lived local connections reuse warm tunnels (what Linkerd/ztunnel
   do with HBONE-style multiplexing; a simple idle pool gets most of the benefit without
   multiplexing). Expected: order-of-magnitude drop in handshake CPU and connect latency on
   churn-heavy workloads.
2. **Fix accept-loop head-of-line blocking**: replace the inline
   `acquire_owned().await` with `try_acquire_owned()` (reject with a metric when saturated)
   or move the acquire inside the spawned task. The listener must never stall.
3. **Multiple acceptors with `SO_REUSEPORT`** (via `socket2`) — N listener tasks per port
   (N = min(cores, 4) to start) so accept + handshake setup scales across cores at high
   connection rates.
4. **DNS discovery hardening** (`src/discovery/dns.rs`):
   - Single-flight per name (e.g. `DashMap<String, Arc<tokio::sync::OnceCell>>` or a
     per-entry refresh lock) to stop expiry stampedes.
   - Serve-stale-while-revalidating: return the expired entry and refresh in the background.
   - Give the cache its own TTL constant (e.g. 30 s) instead of reusing the
     `DNS_RESOLVE` timeout, and honor upstream record TTLs where available.
5. **Listener/socket tuning**: explicit backlog via `socket2`, and only set keepalive on
   long-lived streams (the two `setsockopt` calls per socket are minor but measurable at
   very high accept rates).

## Phase 4 — Build, runtime, and allocator

1. **Crypto provider bake-off**: rustls 0.23 defaults to `aws-lc-rs`; the crate also pulls
   `ring` directly. Benchmark both providers on the churn + throughput profiles and pin the
   winner explicitly (aws-lc-rs usually wins AES-GCM bulk; also decides whether the direct
   `ring` dependency can go).
2. **Allocator**: try `mimalloc`/`jemalloc` as the global allocator — connection-heavy
   workloads with small allocations often gain 5–15 % CPU. Keep only if the churn profile
   shows it.
3. **Release profile**: add `panic = "abort"`; optionally document
   `RUSTFLAGS="-C target-cpu=native"` for benchmark builds (not release artifacts).
4. **Runtime**: confirm the multi-threaded runtime sizing on small edge devices
   (`worker_threads` configurable via `Config` — 1M-device edge targets may want 1–2 workers
   to cap memory).

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
