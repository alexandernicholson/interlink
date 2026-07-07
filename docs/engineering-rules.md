# Engineering Rules

Binding rules for changes to this repo, especially the proxy hot path. Each rule exists
because we shipped the mistake it forbids (2026-07-08 performance work, commits
`583f3f5..08c2223`; see `docs/performance-plan.md` "Review findings"). Cite the rule
number in review when you see a violation.

## A. Verification — nothing ships on plausibility

**A1. Every new code path must be executed by a test before it merges.**
Not "the build passes", not "adjacent tests pass" — a test that drives the new path.
The DNS single-flight deadlocked on *every* cache miss and the connection pool handed out
*only* dead sockets; both shipped green because no test called `resolve()` on a real name
and nothing ever checked out a pooled connection. If you cannot write a test that
exercises the path, say so in the PR and explain how you verified it instead.

**A2. Concurrency features need a concurrency test.**
A single-caller test cannot validate single-flight, pooling, backpressure, or
locking. Test with N concurrent tasks and assert the coordination property itself
(e.g. "N concurrent `resolve()` calls for one name → exactly 1 upstream lookup", using a
counting fake resolver). The rewritten single-flight passed its single-caller timeout
test while being a complete no-op under concurrency.

**A3. Wrap hang-prone assertions in `tokio::time::timeout`.**
Any test that awaits coordination (semaphores, channels, pools, DNS) must bound the wait,
so a deadlock fails in seconds instead of hanging CI.

**A4. Performance claims require attached numbers.**
A claim like "saves ~1 RTT" or "avoids the handshake" must come with before/after output
from `cargo bench` or a `bench/` profile, committed alongside the change (`bench/results/`
or the PR description). The churn and bulk profiles exist precisely so hot-path changes
can prove themselves; a profile that was never run proves nothing.

**A5. Report status precisely.**
Commit messages and plan checkmarks must describe what actually happened, at the file
level. "SocketAddr end-to-end" was claimed when only the producer was converted and both
callers immediately `.to_string()`'d the result back. Half-done is a fine state — call it
half-done and list the remaining half.

## B. Rust rules

**B1. RAII guards must be bound to a named variable for their intended scope.**
`MutexGuard`, `SemaphorePermit`, `OwnedSemaphorePermit`, spans — a guard that isn't bound
lives only to the end of its expression. `sem.try_acquire().map_err(..).err()` silently
dropped the permit inside the expression, turning leader election into a no-op. Write
`let _permit = sem.try_acquire()?;` (note: `let _ = guard` also drops immediately —
`_` binds nothing; use a named `_permit`). Never route a guard through
combinators that discard the `Ok` value.

**B2. No task-migration-unsafe state across `.await`.**
`thread_local!` values set before an `.await` and read after it are wrong twice over on
the multi-threaded runtime: the task may resume on another thread, and other tasks
interleave on the same thread and clobber the slot. Pass values (an `Instant`, an ID)
explicitly through the call, or put them in the task's own stack/struct.

**B3. Prove a semaphore's permit accounting on paper before using it.**
State the invariant in a comment: initial permits, who acquires, who releases/forgets,
and why waiters always wake. `Semaphore::new(0)` + `try_acquire` (nobody can ever win) and
an unreachable `.forget()` (leaks a permit when reached) both survived review because the
invariant was never written down. If the accounting needs more than three sentences,
use a higher-level primitive (`OnceCell`, `watch`, singleflight pattern) instead.

**B4. A connection is not reusable because you still hold it.**
`copy_bidirectional` propagates shutdown — when it returns, both streams have had
`poll_shutdown` driven (TLS close_notify sent). Any "keep this stream for later" design
must identify, in the PR description, the exact point in the protocol where the stream is
known to be alive and quiescent, and must include a liveness check at reuse time. If you
can't name that point, the stream is dead; don't pool it.

**B5. No `unwrap_or_else`-style fallbacks on security-relevant values.**
Identity, policy decisions, and trust material fail *closed*. Fabricating
`spiffe://unknown/...` when identity extraction failed fed a made-up identity into the
policy engine. If an identity can't be established, the connection is rejected — an
`Err` propagates; it is never papered over with a default.

**B6. Dead code is deleted, not parked.**
Git history is the parking lot. The disabled pool left 7 `never used` warnings that
train everyone to ignore the warning that matters. `cargo build` and `cargo clippy` must
be warning-clean on every merge; if code must stay for an imminent redesign, gate it
behind a feature flag so it still compiles in CI but doesn't warn.

## C. Hot-path and observability standards

**C1. A metric name is a contract.**
Never increment a counter whose name describes something else — saturation rejections
were counted into `connections_total`, corrupting both meanings. New event → new metric
with a precise name (`interlink_saturation_rejections_total`). When adding a metric,
wire it into *every* code path where the event occurs (the inbound proxy recorded
nothing while outbound recorded the wrong thing).

**C2. Don't record sentinel values into histograms.**
`Duration::ZERO` for "the connection failed" is not a latency observation; under error
load it drags every percentile toward zero. Failure paths skip the histogram and
increment an error counter instead. A histogram records only real measurements of the
thing it names.

**C3. Symmetry check: inbound ↔ outbound.**
`tcp.rs` and `outbound.rs` are structural twins. Any fix to accept loops, semaphores,
socket config, metrics, or logging applies to both; the head-of-line-blocking fix shipped
inbound-only. Before opening the PR, grep the sibling file for the pattern you just
changed and either change it too or state in the PR why it doesn't apply.

**C4. Per-connection work is guilty until proven cheap.**
Anything inside `handle_connection`/`handle_accept` runs millions of times: no
`Url::parse`, no `format!`/`to_string` round-trips of things that have typed forms
(`SocketAddr`), no per-call construction of constants (OIDs, patterns). Parse and compile
at configuration time; the hot path only matches and copies. When converting a path to
typed values, convert it end-to-end — producer *and* consumers — or you've added a
conversion, not removed one.

**C5. Constants mean what their module says.**
Reusing `timeouts::DNS_RESOLVE` (a timeout) as a cache TTL hid a 6× error. If the value
you need doesn't exist, add a named constant where it belongs; never borrow a number
because it's conveniently in scope.

## D. Process

**D1. One logical change per commit/PR**, with the profile or test evidence in the
description. A commit that fixes four review findings is four commits.

**D2. Update `docs/performance-plan.md` in the same PR** as the work it tracks, using
exact file paths and honest status (✓ done / ⚠ partial / TODO). The plan is the source of
truth for what's proven vs. claimed.

**D3. Pre-merge checklist** (all must hold):
- [ ] `cargo test` passes; new paths have tests (A1), concurrency has concurrency tests (A2)
- [ ] `cargo build` + `cargo clippy` warning-clean (B6)
- [ ] Perf-motivated change has attached numbers (A4)
- [ ] Sibling proxy file checked for the same pattern (C3)
- [ ] Metrics: right counter, all paths, no sentinel histogram values (C1, C2)
- [ ] Commit message states exactly what is and isn't done (A5)
