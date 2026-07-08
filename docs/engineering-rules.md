# Engineering Rules

Binding rules for changes to this repo, especially the proxy hot path. Each rule exists
because we shipped the mistake it forbids (2026-07-08 performance work, commits
`583f3f5..8fbe693`; see `docs/performance-plan.md` "Review findings", three rounds).
Cite the rule number in review when you see a violation. Rules are append-only —
numbers are stable so citations stay valid.

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

**A6. A test must be able to fail. Prove it once before committing.**
`assert!(r.is_ok() || r.as_ref().unwrap().is_err())` is a tautology — it passed while the
code under test panicked the process in a different branch. For every new test, break the
code once (revert the fix, flip a condition) and watch the test go red; if you can't make
it fail, it tests nothing. Assertions must state the property, not a disjunction that's
always true.

**A7. The test must assert what its name promises.**
`test_concurrent_resolve_dedup` counted zero lookups — "dedup" was never checked. If the
property is "N concurrent callers → exactly 1 upstream lookup", the test counts lookups
and asserts `== 1`. A test whose body is weaker than its name is worse than no test:
it makes reviewers believe the property is covered.

**A8. Test the success path, especially under concurrency.**
The DNS waiter test only resolved a non-existent name, so the *normal* outcome — waiter
finds a populated cache — was never executed, and that exact path panicked in
production-shaped use. Error-path tests are necessary but not sufficient: for any
coordination code, at minimum one test drives N concurrent callers through the
*successful* flow (a barrier + a resolvable input caught the panic in seconds).

**A9. A fake must be able to hold the system in its intermediate states — and the test
must prove the contested branch ran.**
The counting fake resolver returns `Ready` without ever yielding, so under the
single-threaded test runtime no concurrent caller ever lands in the semaphore-wait
branch — the dedup test passes via the cache-hit path, and the branch that panicked the
process in round three is *still* unexecuted by any test. A fake that can't pause can't
test coordination: give fakes a gate (`tokio::sync::Notify`, a channel, or an injected
delay) so the test can park the leader mid-operation and force followers into the
waiting path. Then assert the branch was taken (a counter on the wait path, or assert
lookups == 1 *while the leader is provably still in flight*), not just that outputs look
right. Corollary of A2/A8: coverage of a concurrent function means coverage of its
*interleavings*, not its lines.

**A10. "Verify X" is closed by an observable signal, never by reasoning about the
design.**
"TLS resumption: ✓ verified architecturally — rustls enables it by default" closed a
measurement task with a restatement of the assumption the task existed to check. The
tell: nothing in the codebase could distinguish the property holding from it failing —
no metric, no test, no number. A verification task produces at least one of: a metric
that separates the two outcomes in production (e.g. resumed-vs-full handshake counts), a
test that fails if the property is absent, or a measured before/after. If none exists,
the honest status is "instrumented, not yet measured" or simply "not verified" — and
"the docs say it's the default" is a reason to *expect* verification to succeed, not a
substitute for it.

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

**B7. `unreachable!()` requires a local proof; a match arm that evaluates to a value is
reachable.**
The DNS rewrite ended `let _permit = match … };` with `unreachable!()` on the assumption
that "both branches return above" — but the waiter arm ended in an expression (`…?`
yielding the endpoints), not a `return`, so the normal success path fell straight into
the panic. Before writing `unreachable!()`, write the one-line proof as a comment naming
why *each* arm diverges (`return` / `?`-on-guaranteed-Err / `continue` / `!` call), and
prefer restructuring so the compiler enforces it: return the value from every arm, or let
the match be the function's tail expression. If any arm's last line is an expression that
produces a value, that arm reaches the code after the match — full stop.

**B8. No panic paths in connection-handling code — this proxy builds with
`panic = "abort"`.**
A panic anywhere in a spawned task doesn't kill one connection; it aborts the entire
proxy and every connection it carries. In `src/proxy/`, `src/discovery/`, and anything
else reachable from `handle_connection`: no `unwrap()`, `expect()`, `unreachable!()`,
`todo!()`, or indexing that can panic on runtime data. Return `Err` and let the
connection fail alone. Enforce mechanically:
`#![deny(clippy::unwrap_used, clippy::expect_used)]` on those modules (test code is
exempt via `#[cfg(test)]`).

**B9. Don't launder control flow through a binding.**
`let _permit = match …` where one arm returns, one arm was *supposed* to return, and the
binding sometimes holds a permit and sometimes holds DNS endpoints is how B7's bug became
invisible. A binding's name is a claim about its contents in every branch. If the arms of
a match do different jobs, don't force them to produce one value — use early returns:
handle the leader case and `return`, handle the waiter case and `return`, and let there
be no code after the match.

**B10. A cache needs a written lifecycle: every state must have an exit.**
The serve-stale rewrite returns expired entries unconditionally and nothing ever
refreshes them — after the first resolution, endpoints are frozen until restart, so
upstream redeploys and failovers become invisible. Before merging any cache, write the
state table in a comment: for each state (empty / fresh / expired / refresh-in-flight),
what does a reader get, and what transitions the entry back to fresh? "Expired → serve
stale" is only valid if something *also* schedules the refresh; an expired state with no
exit is a bug by inspection. Add a test that expires an entry (injected clock or tiny
TTL) and asserts a subsequent read observes updated data.

**B11. Hot-path external dependencies go behind a trait.**
`ServiceDiscovery` holds a concrete `TokioResolver`, so the single-flight property
("N callers → 1 lookup") is untestable without real DNS — which is exactly why its test
was vacuous (A7) and why two consecutive rewrites shipped broken. Anything that does I/O
on behalf of the hot path (resolver, time source if it affects logic, upstream dialer)
is injected as a trait object or generic, with the production impl as the default
constructor. The unit tests then use a counting/failing/delaying fake to pin the
coordination properties.

**B12. Never satisfy a lint by deleting the behavior it flags.**
To comply with B8's `deny(clippy::expect_used)`, the `validate_segments().expect(…)` call
was removed from `SpiffeId::new` — so the lint passed and `SpiffeId::new("", "", "")` now
silently constructs an invalid identity that flows into policy evaluation. The lint
forbids the *panic*, not the *check*: the correct transformations are panic → `Result`
(`try_new`), panic → validated-at-boundary (parse config once, store the typed value), or
— for a genuinely infallible case — a scoped `#[allow]` with a one-line justification.
If your lint fix removes an assertion, a validation, an error branch, or a log, you have
changed behavior, and the PR must say so in those words (A5) and defend it.

**B13. A type with an invariant must not expose an unvalidated public constructor.**
`SpiffeId`'s segments must be non-empty — that's what makes it safe to feed to the policy
engine (B5). Such a type gets `try_new() -> Result<Self, _>` as the only public
construction path from runtime data; struct literals stay private to the module. If an
infallible constructor is genuinely needed for compile-time constants, restrict it
(`#[cfg(test)]`, `pub(crate)` with a doc comment naming the caller's obligation) — never
a public `new()` that skips the check, because every future call site inherits the
footgun invisibly.

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

**C6. Benchmark outputs record the parameters of the run that produced them.**
`proxy-summary.md` says "200 ms delay" above zero-delay results because the generator's
header is hardcoded prose. Every results file must be stamped from the *actual* run
parameters (`PROFILE_DELAY`, QPS, connections, duration, payload, keepalive, git SHA) by
the harness itself, never typed by hand. A number whose conditions are mislabeled is
worse than no number — it will be compared against the wrong baseline.

## D. Process

**D1. One logical change per commit/PR**, with the profile or test evidence in the
description. A commit that fixes four review findings is four commits.

**D2. Update `docs/performance-plan.md` in the same PR** as the work it tracks, using
exact file paths and honest status (✓ done / ⚠ partial / TODO). The plan is the source of
truth for what's proven vs. claimed.

**D3. Pre-merge checklist** (all must hold):
- [ ] `./scripts/preflight.sh` is green (D8 — covers clippy `-D warnings` + tests)
- [ ] New paths have tests that drive them (A1), concurrency has concurrency tests (A2)
- [ ] Each new test was made to fail once, asserts its named property, and covers the
      success path (A6, A7, A8); concurrency tests force the contested branch via a
      gated/delayed fake (A9)
- [ ] `cargo build` + `cargo clippy` warning-clean (B6); no panic paths in
      connection-handling code (B8)
- [ ] Every `unreachable!()` has a divergence proof per arm — or was restructured away (B7)
- [ ] Caches/coordination: state lifecycle written down, every state has an exit (B3, B10)
- [ ] Perf-motivated change has attached numbers from an honestly-labeled run (A4, C6)
- [ ] Sibling proxy file checked for the same pattern (C3)
- [ ] Metrics: right counter, all paths, no sentinel histogram values (C1, C2)
- [ ] Lint fixes preserved every assertion/validation they touched (B12); invariant
      types still have no unvalidated public constructor (B13)
- [ ] Changed a generator? Its committed outputs are regenerated in this PR (D5)
- [ ] Commit message re-read against `git diff --stat` — every claim appears (A5, D6)
- [ ] Closing a finding? Every enumerated item addressed or explicitly deferred (D7)
- [ ] "Verify X" items closed with a signal — metric, failing-test, or measurement,
      not design reasoning (A10)

**D4. A fix to a reviewed defect gets re-reviewed against the *original* failure mode.**
The single-flight bug was "fixed" twice; each fix satisfied the letter of the cited rule
(B1: permit held) while shipping a new defect in the same function (reachable
`unreachable!()`, frozen cache). When fixing a review finding, restate the original
property in the PR ("N concurrent resolves → 1 lookup, waiters get the result, no
panic, entries refresh after TTL") and show the test output that demonstrates each
clause — not just the clause the reviewer named.

**D5. Fixing a generator means regenerating its committed artifacts in the same PR.**
The benchmark-summary header was fixed to echo `PROFILE_DELAY`, but the committed
`proxy-summary.md` — produced by the *old* script — still says "200 ms delay" above
zero-delay numbers. A repo where the generator and its outputs disagree is worse than
either bug alone: the artifact looks authoritative and the fix looks done. When you
change any script that produces committed files (benchmark summaries, generated configs,
codegen), rerun it and commit the regenerated outputs together with the script change;
if rerunning isn't possible, delete the stale artifact rather than leave it mislabeled.

**D6. Claims in commit messages are diff-checkable — check them.**
"B8: add deny lints to proxy/discovery" landed with the attribute on every proxy module
and *not* on discovery. Before committing, reread the message against `git diff --stat`:
every claimed location, fix, and scope must appear in the diff. This is A5 applied
mechanically; it costs thirty seconds and it has caught something in three of four
review rounds.

**D7. A finding that enumerates N items is closed item-by-item — one-of-N is not
done.**
The `SpiffeId` finding named four call sites (`main.rs:52`, `tcp.rs:72`,
`outbound.rs:74`, `identity/provider/mod.rs:47`); the "fix" converted one, and the item
was marked ✓ while the three *config-driven* sites — the ones carrying the security risk
— stayed unvalidated. When closing a review finding, copy its enumerated list into the
PR and mark each element fixed/deferred-with-reason; grep for the pattern once more to
catch sites the finding itself missed. If any element is deferred, the finding's status
is ⚠ partial, never ✓. (This is D4's re-verification requirement made concrete for
list-shaped findings.)

**D8. Run `./scripts/preflight.sh` before every commit — the mechanical half of D3 is
not optional and not from memory.**
Clippy warnings were reintroduced in round five by a commit that fixed other review
findings — diligence doesn't scale across five rounds, scripts do. Preflight runs
`cargo clippy --all-targets -- -D warnings` and `cargo test`; it must pass on every
commit, not just at PR time (a red intermediate commit poisons bisect). Green preflight
covers B6/B8/A1's mechanical halves only — the judgment items in D3 remain yours. If
preflight is red on code you didn't touch, fixing it is part of your change, not
someone else's.
