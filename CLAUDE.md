# interlink

A lightweight service mesh proxy in Rust (mTLS, SPIFFE identity, policy-based
authorization). The per-connection hot path lives in `src/proxy/`; treat everything it
touches as performance- and security-sensitive.

## Required reading before changing code

- **`docs/engineering-rules.md` — binding rules.** Follow them; cite rule numbers in
  reviews. They were derived from real regressions shipped during the 2026-07 performance
  work (deadlocked DNS single-flight, pool of dead connections, mispaired histograms).
  Highlights: every new code path needs a test that drives it (A1); concurrency features
  need concurrency tests with timeouts (A2, A3); RAII guards (`SemaphorePermit` etc.)
  must be bound to a named variable, never dropped mid-expression (B1); no thread-local
  state across `.await` (B2); identity/policy failures fail closed, never a fabricated
  default (B5); fixes to `src/proxy/tcp.rs` almost always apply to its twin
  `src/proxy/outbound.rs` and vice versa (C3); no parsing/formatting/allocation in
  per-connection code (C4).
- **`docs/performance-plan.md`** — the current performance workstream, with verified
  status per item. Update it in the same PR as the work; mark items honestly
  (✓ done / ⚠ partial / TODO) and never claim a result without attached numbers.

## Commands

- Build/test: `cargo build`, `cargo test` (must be warning-clean; clippy too)
- Microbenchmarks: `cargo bench --bench proxy`
- End-to-end benchmarks: `bench/local/run.sh` and `bench/local/run-proxy.sh`
  (env: `PROFILE_DELAY`, `CHURN=1`, `BULK=1`); results are committed under
  `bench/results/`
