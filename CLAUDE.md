# interlink

A lightweight service mesh proxy in Rust (mTLS, SPIFFE identity, policy-based
authorization). The per-connection hot path lives in `src/proxy/`; treat everything it
touches as performance- and security-sensitive.

## Required reading before changing code

- **`docs/engineering-rules.md` — binding rules.** Follow them; cite rule numbers in
  reviews. They were derived from real regressions shipped during the 2026-07 performance
  work (deadlocked DNS single-flight, pool of dead connections, mispaired histograms, a
  reachable `unreachable!()` that aborts the release binary, validation deleted to
  silence a lint). Highlights: every new code path needs a test that drives it (A1);
  concurrency features need concurrency tests with timeouts (A2, A3); every test must be
  able to fail, assert its named property, and cover the success path (A6–A8); fakes
  must be gateable so tests can force the contested branch (A9); RAII guards
  (`SemaphorePermit` etc.) must be bound to a named variable, never dropped
  mid-expression (B1); no thread-local state across `.await` (B2); identity/policy
  failures fail closed, never a fabricated default (B5); no panic paths in
  connection-handling code — this proxy builds with `panic = "abort"`, and a match arm
  ending in an expression *reaches* the code after the match (B7–B9); caches need a
  written state lifecycle where every state has an exit (B10); hot-path I/O dependencies
  go behind traits so coordination is testable with fakes (B11); never satisfy a lint by
  deleting the check it flags — panics become `Result`, and invariant types expose only
  validated constructors (B12, B13); fixes to `src/proxy/tcp.rs` almost always apply to
  its twin `src/proxy/outbound.rs` and vice versa (C3); no parsing/formatting/allocation
  in per-connection code (C4); when fixing a review finding, re-verify the original
  property end-to-end, not just the cited clause (D4); regenerating a generator's
  committed outputs ships with the generator fix (D5); re-read the commit message
  against the diff (D6).
- **`docs/performance-plan.md`** — the current performance workstream, with verified
  status per item. Update it in the same PR as the work; mark items honestly
  (✓ done / ⚠ partial / TODO) and never claim a result without attached numbers.

## Commands

- Build/test: `cargo build`, `cargo test` (must be warning-clean; clippy too)
- Microbenchmarks: `cargo bench --bench proxy`
- End-to-end benchmarks: `bench/local/run.sh` and `bench/local/run-proxy.sh`
  (env: `PROFILE_DELAY`, `CHURN=1`, `BULK=1`); results are committed under
  `bench/results/`
