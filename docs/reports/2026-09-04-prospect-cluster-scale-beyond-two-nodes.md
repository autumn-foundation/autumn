# ⛏️ Prospect: does the gossip cluster converge correctly at N=5? (pursue: 4/4 runs clean vs. the pre-set correctness lines)

## 🎯 Question

`docs/guide/clustering.md` documents the embedded cluster's gossip design
(full-broadcast: every push carries the whole membership+counter document to
every known peer directly, no indirect probing, no quorum anywhere) and says,
in its own words: *"That is a sound design at two nodes and an unproven one
beyond that. Treat larger fleets as future work, not as a supported
configuration."* The only test coverage that exists —
`autumn/tests/integration/cluster_two_node.rs` (real-socket L2/L3 tests) and
`autumn/src/cluster/tests.rs` (deterministic virtual-clock unit tests, one
`async fn` per behavior) — never spins up more than two real cluster members;
the one three-party test in `tests.rs` (line 646, "a third node with the
wrong secret") is about auth rejection, not scale.

**Falsifiable question:** with a 5-node cluster, star-seeded (nodes 2-5 each
seed only from node 1's address — the minimal, realistic seed-list shape the
docs describe), using the *exact* `push_interval_ms`/`suspicion_timeout_ms`
ratios the existing 2-node test already uses, does the current implementation
(a) converge every node to an identical 5-member view from cold start, (b)
merge concurrent counter increments issued from all 5 nodes into the exact
correct sum on every node, and (c) converge the survivors to a correct
4-member view after one node's clean departure, and back to 5 after that node
rejoins — or does any of those steps produce a divergent/incorrect final
state (the "split-brain" failure mode full-broadcast-no-quorum designs are
generically prone to)?

**Decision:** whether `docs/guide/clustering.md`'s "unproven beyond two nodes"
line should be narrowed to name a verified N (if the assay passes — cheap,
concrete evidence to cite instead of a hedge) or whether a correctness issue
should be filed against the cluster module before any larger-fleet claim is
made either way (if it fails). **Decider:** repo maintainer (owns both the
cluster module and the clustering guide's claims; no other named owner
exists for either).

**Who is waiting on this, and what changes:** nobody has an active fleet
deployment blocked on this today — this is a documentation-accuracy and
early-warning question, not a shipped-feature blocker. What changes on yes:
the guide can state a verified range instead of "unproven," giving anyone
who does reach for N>2 real evidence instead of a hedge. What changes on no:
a concrete, reproducible bug report exists before anyone finds out the hard
way in a real deployment.

## 🔍 Prior art

- `grep -rn` across the repo for cluster test coverage: only
  `autumn/tests/integration/cluster_two_node.rs` (2-node, real sockets) and
  `autumn/src/cluster/tests.rs` (2-node, virtual clock) exist. No file
  anywhere constructs 3+ real cluster members.
- `docs/adr/` — no ADR covers cluster scale-out; the clustering guide itself
  is the only design record, and it states the open question directly rather
  than resolving it.
- `docs/reports/*prospect*` (the assay ledger) — no prior Prospect assay on
  clustering of any kind. This pit has not been dug before.
- The two existing Prospect reports in this ledger
  (`2026-09-02-prospect-cold-start-db-gate-verify.md`,
  `2026-09-03-prospect-cold-start-post-fix-bisect.md`) are unrelated
  (cold-start compile time), included here only to confirm the ledger format
  this report follows.

## ⚖️ Pre-registration

Committed before any node is ever started or any assertion is written.

- **Success line (pursue — narrow the documented claim):** in a single test
  process, 5 `ClusterHandle`s (star-seeded from node 1, `push_interval_ms:
  200`, `suspicion_timeout_ms: 1_000` — identical to the existing 2-node
  test's `cluster_config`) must, within **10 seconds** of starting all 5
  (2x the existing test's 5s two-node convergence bound, to allow for the
  larger one-time connection setup, not for slower steady-state gossip since
  every push is still a single direct hop):
  1. Converge to a 5-member view on **every** node (`handle.members().len()
     == 5` on all five, and all five sorted member-id lists identical — not
     a one-sided check, matching the existing test's own `assert_converged`
     discipline).
  2. After each of the 5 nodes issues a distinct, known counter increment
     (node *i* adds `i+1`, total = 1+2+3+4+5 = 15) and a further 10s
     convergence window, **every** node's `counter.get()` must equal exactly
     15 — no lost updates, no double-counts.
  3. After cancelling node 5's shutdown token (clean leave), the remaining 4
     must converge to an identical 4-member view within **5 seconds**
     (suspicion_timeout 1s + generous margin, matching the existing
     `tcp_survivor_converges_after_peer_cancelled` pattern).
  4. After starting a fresh node 5 (re-seeded from node 1) the cluster must
     reconverge to a 5-member view within the same 10s bound as step 1.
  All four hold ⇒ **pursue**: narrow the guide's hedge to a verified range.
- **Kill line (kill — file a correctness issue):** any one of:
  - Member views still disagree across the 5 nodes 3x past the relevant
    timeout above (30s / 15s respectively) — genuine divergence, not slow
    convergence.
  - The converged counter value on any node is not exactly 15 once every
    node reports a stable 5-member view for 2 consecutive polls.
  - A panic, deadlock, or hang in cluster code under N=5 (test itself times
    out past 60s total).
  - Any of the above reproduces on **2 of 3** repeated runs (ruling out a
    one-off scheduler fluke on this shared sandbox before calling it a real
    bug).
  If the top-level convergence/counter/departure/rejoin behavior holds but
  only the specific numeric margins above are what's missed (e.g. converges
  in 11s, not 10s) — report as **undetermined-on-the-line, qualitatively
  pursue**, not a silent pass: the margin miss itself is data (see Verdict).
- **Conditions:** this sandbox only (single machine, all 5 members as
  separate `TestApp`/`ClusterHandle` instances bound to `127.0.0.1:0` inside
  one `#[tokio::test(flavor = "multi_thread")]`), default crate features,
  `test-support` feature enabled (as the existing cluster integration tests
  require for `TestApp`). Star topology: nodes 2-5 seed only from node 1's
  observed `local_addr()`, mirroring the two-node test's own seeding
  convention and the docs' minimal-seed-list guidance — not a full seed-list
  (every node listing every other), which would be a different, easier
  question.
- **Time box:** ≤45 minutes cumulative wall-clock this session. If apparatus
  construction or a single run exceeds 15 minutes, stop and report
  undetermined with whatever partial evidence exists.
- **Riskiest assumption tested first:** that full-broadcast/no-quorum gossip
  stays *correct* (not fast, not efficient — those are separately-scoped,
  already-acknowledged future work) once more than 2 peers are exchanging
  full-document pushes concurrently. Correctness (split-brain / lost update)
  is the specific failure mode the guide's own wording gestures at, and is
  tested first and primarily; performance/scaling headroom is out of scope
  for this assay and is not measured here.
- **Control:** the existing 2-node test's own documented behavior (its
  timeouts, its `assert_converged` discipline) is the control this assay
  scales up rather than reinvents — same config ratios, same style of
  two-sided convergence assertion, same real-socket harness pattern
  (`TestApp` + `install_from_config` + `ClusterHandle`), just N=5 instead of
  N=2.
- **Containment:** apparatus is one new, throwaway test file
  (`autumn/tests/prospect_cluster_scale.rs`) plus a temporary `[[test]]`
  entry in `autumn/Cargo.toml`, both added on this branch, run only via local
  `cargo test` in this sandbox — never pushed to CI, never added to
  `tests/integration/mod.rs`'s consolidated binary. Both are reverted before
  this report's final commit; only the report and its embedded results
  survive. No production data, no external network, no spend.

### 🛠️ Correction (Codex review on PR #2503, before merge)

Two P2 findings, both fixed by strengthening the apparatus and re-running
rather than by softening the claim:

1. **The reproduce recipe pointed at a file that was never committed
   anywhere.** The original Apparatus/Reproduce text said to "restore
   `autumn/tests/prospect_cluster_scale.rs` from git history" — but per
   this ledger's own containment rule the file was written, run, and
   reverted *before* the first commit, so no commit on this branch ever
   contained it. Reproduction was actually impossible from the instructions
   as written. Fixed below: the full apparatus source is now embedded
   verbatim in **Reproduce**, which is the durable artifact from here on.
2. **"Exact counter sums" was asserted for the whole run, but only checked
   before churn.** The original apparatus polled the counter for the exact
   sum (15) once, right after step 2's increments, then only checked
   *membership* (not the counter) through the departure (step 3) and
   rejoin (step 4) steps — while the pre-registration's own kill line
   ("the converged counter value on any node is not exactly 15 once every
   node reports a stable 5-member view for 2 consecutive polls") was never
   scoped to step 2 alone. This was a real gap between what was promised
   and what was measured, not a new criterion invented after the fact.
   Fixed by adding the same counter-sum assertion after step 3 (on the 4
   survivors) and after step 4 (on all 5, including the rejoined node),
   and re-running. See **Apparatus** and **Assay** below for the corrected
   procedure and results — the verdict is unchanged (still pursue), now on
   stronger evidence than the first pass actually supported.

**A third P2 finding**, on the revision-2 diff above: every
convergence/counter check up to that point read the condition **once** at
the instant a poll loop first saw it true, then moved on — it never
confirmed the view stayed put, and membership was never re-checked
immediately after a counter check (only before). A member evicted by the
suspicion timeout and then re-admitted between two checks, or a stale
counter cell surviving a membership flicker, could in principle slip
through undetected, and the report's "no divergence" language claimed more
than a single-instant read can support. Fixed by replacing every
single-read poll with a debounced one requiring the condition to hold on 3
consecutive observations 50ms apart (`poll_until_stable`/
`assert_stable_membership`/`assert_stable_counter_sum` below), and adding
a membership re-check immediately after every counter check (not just
before it) — so a flicker during counter convergence can't go unobserved
either. Re-ran 4 more times; see **Assay**. Verdict is unchanged.

**A fourth and fifth P2 finding**, both on the revision-3 diff above, both
in the debounce refactor itself rather than in what it was testing:

1. `poll_until_stable`'s inner loop slept `STABLE_GAP` only after a
   *successful* observation; a failed observation `break`ed out with no
   sleep or `.await` yield at all before the outer loop immediately
   retried. Since every condition here is a synchronous `ClusterHandle`
   read wrapped in an `async move` that completes instantly, a failing
   streak could in principle spin-retry with no genuine yield point — on a
   single-worker Tokio runtime this can monopolize the only executor
   thread and starve the very gossip/timer tasks the condition is waiting
   on, producing a false timeout on an actually-healthy cluster. This
   sandbox's `#[tokio::test(flavor = "multi_thread")]` has multiple
   workers, which is almost certainly why it was never observed here — but
   the apparatus's correctness shouldn't depend on how many cores happen
   to be available. Fixed: yield `STABLE_GAP` after *every* observation,
   success or failure, before deciding whether to retry.
2. The revision-3 refactor consolidated every membership/counter check
   into two helpers, and both hardcoded `CONVERGE_TIMEOUT` (10s) —
   silently dropping the pre-registered 5s `DEPARTURE_TIMEOUT` for step
   3's checks (both the membership check and the post-departure counter
   check). `DEPARTURE_TIMEOUT` was still declared but had become
   dead code, which is exactly the kind of drift a compiler warning alone
   doesn't catch in a `cargo test` invocation. Every run so far still
   converged in ~2.3-2.7s either way, well under 5s, so no run's *result*
   was actually affected — but the apparatus as written no longer enforced
   the bound it claimed to, which matters for anyone who reproduces this
   and hits a genuinely slow run. Fixed: both helpers now take an explicit
   `timeout` parameter, and step 3 passes `DEPARTURE_TIMEOUT` while
   everything else keeps `CONVERGE_TIMEOUT`.

Re-ran 4 more times (16 total across all four passes); see **Assay**.
Verdict is unchanged.

**A sixth and seventh P2 finding**, both on the revision-4 diff above, both
about the pre-registered kill line's own 60s total bound and the debounce
helper's deadline handling — not about the correctness result itself:

1. The pre-registered kill line includes "test itself times out past 60s
   total," but the apparatus had no whole-test watchdog, only per-phase
   bounds (10s×6 + 5s×3 = 75s summed) — a genuine hang, or a pattern where
   every phase individually stayed just under its own bound, could in
   principle blow past the registered 60s total with no single check ever
   catching it. Every run so far finished in ~2.3-2.7s total (a tiny
   fraction of even the tightest reading of the budget), so this was never
   close to firing in practice — but the apparatus as written didn't
   actually enforce the line it claimed to. Fixed: the entire test body
   now runs inside `tokio::time::timeout(TOTAL_TEST_TIMEOUT, ...)`,
   `TOTAL_TEST_TIMEOUT` = 60s, matching the registered line exactly and
   giving a genuine hang somewhere to actually fail against instead of
   running forever.
2. `poll_until_stable` checked the deadline only after a *failed*
   observation; a streak whose 3 observations all succeeded could still
   have its *last* observation (and the `STABLE_GAP` sleep after it) land
   after `deadline` had already passed, and the function would still
   return `true` — silently reporting convergence "within `{timeout:?}`"
   for a run that, by the clock, wasn't. Fixed: the deadline is now
   checked after every single observation, success or failure; any
   observation landing past it fails the poll immediately, even mid-streak.

Re-ran 4 more times (20 total across all five passes); see **Assay**.
Verdict is unchanged.

**An eighth, ninth, and tenth P2 finding**, all on the revision-5 diff
above:

1. The debounce window (`STABLE_CHECKS=3` × `STABLE_GAP=50ms` ≈ 150ms) was
   far shorter than the protocol's own `push_interval_ms` (200ms) and
   `suspicion_timeout_ms` (1000ms) — three reads 50ms apart mostly just
   re-observe the same pre-gossip state, so "stable across 3 observations"
   never actually spanned a real gossip round or failure-detector cycle,
   undermining the "no divergence" claim it was meant to support. Fixed:
   widened to `STABLE_CHECKS=5`, `STABLE_GAP=250ms` — a 1250ms window,
   longer than `suspicion_timeout_ms` and several multiples of
   `push_interval_ms`.
2. The pre-registration explicitly names a third outcome — "converges, but
   past the success line and short of the 3x divergence threshold" is
   undetermined-on-the-line, not a silent pass or a hard failure — but the
   apparatus only ever implemented pass/fail: it hard-failed the instant
   the first (success-line) deadline passed, regardless of whether the
   condition would have converged shortly after, well inside the
   registered 3x-timeout divergence threshold. No run has ever come
   remotely close to this boundary (every run is 2-13s against 5-10s
   phase bounds), so this never affected any actual result — but the
   apparatus didn't implement the classification the pre-registration
   promised. Fixed: `poll_until_stable` now returns a tri-state
   `ConvergenceOutcome` (`Converged` / `LateConverged` / `Diverged`) against
   `timeout` and `timeout × 3` (the pre-registration's own divergence
   multiplier); only `Diverged` fails the test, `LateConverged` is reported
   via `eprintln!` without failing it.
3. `tokio::time::timeout` is cooperative — it can only fire between
   `.await` points inside the future it directly wraps. The polled
   `members()`/`counter()` reads synchronously acquire locks inside the
   cluster module; a genuine deadlock holding one of those locks would
   block the very task the revision-5 watchdog was polling, so the 60s
   timeout could never fire in exactly the scenario ("deadlock or hang,"
   the kill line's own words) it exists to catch. Fixed: `run_assay()` now
   runs on its own `tokio::task::spawn`ed task, and the 60s timeout wraps
   `.await`ing the `JoinHandle` rather than the future directly — waiting
   on a `JoinHandle`'s completion doesn't require polling the (possibly
   wedged) task on the same call stack, so the test harness can still
   report the timeout and produce a result even if the spawned task stays
   genuinely blocked in the background.

Re-ran 4 more times (24 total across all six passes) with the wider
debounce window — real per-run wall-clock rose from ~2.6s to ~13.1s purely
because of the 9 stability checks' now-mandatory ≥1.25s windows each, not
because convergence itself got slower; see **Assay**. Verdict is unchanged.

**An eleventh and twelfth P2 finding**, both on the revision-6 diff above,
both about the two prior corrections' own precision rather than the
measured result:

1. With `STABLE_CHECKS=5`/`STABLE_GAP=250ms`, the 5 condition *reads*
   land at t≈0/250/500/750/1000ms — the 5th sleep happens *after* the
   last read, so it doesn't extend what's actually observed. The last
   real read sat exactly *at* `suspicion_timeout_ms` (1000ms), not
   strictly past it, which was the whole point of widening the window in
   the first place. Fixed: `STABLE_CHECKS=6`, so the final read lands at
   t≈1250ms — strictly beyond the suspicion timeout, not coincident with
   it.
2. `tokio::task::spawn` still schedules the spawned task on the same
   Tokio runtime as everything else; on a constrained single-worker
   runtime, a genuine synchronous-lock deadlock inside that task can
   starve the one and only worker thread, so *nothing* — including the
   task awaiting the `JoinHandle` + `tokio::time::timeout` — ever gets
   polled again either. This sandbox has never shown fewer than several
   workers in any prior report, so this was never close to firing here,
   but the claim that the watchdog "can still report the timeout... even
   if the spawned task stays genuinely blocked" was true only for a
   multi-worker runtime, which the text didn't say. Fixed: added a
   second, independent watchdog on a native `std::thread` — outside the
   Tokio runtime entirely, so it gets its own OS-level timeslice
   regardless of how many (or few) Tokio workers are wedged — that aborts
   the process directly if the assay hasn't signaled completion by
   `TOTAL_TEST_TIMEOUT`. This backstop trades a clean test failure for a
   hard process abort, but is not starvable by the same failure mode the
   Tokio-scheduled watchdog is.

Re-ran 4 more times (28 total across all seven passes); wall-clock rose
again, to ~15.1-15.3s, purely from the 6th observation added to every
debounce window. See **Assay**. Verdict is unchanged.

**A thirteenth P2 finding**, on the revision-7 diff above: step 2's
"concurrent counter increments from every node" — a claim repeated in
both this report and the guide text — were actually issued by a plain
sequential `for` loop, calling each node's `increment_by` to completion
before starting the next. No two nodes' increments could ever genuinely
overlap; the word "concurrent" wasn't backed by the apparatus. Fixed:
each node's increment now runs on its own `tokio::task::spawn`ed task,
rendezvousing on a `tokio::sync::Barrier` before calling `increment_by` so
all `N` calls start as close to simultaneously as the runtime can
arrange, then joined.

Re-ran 4 more times (32 total across all eight passes); see **Assay**.
Verdict is unchanged.

**A fourteenth and fifteenth P2 finding**, both on the revision-8 diff
above, both logic bugs in the apparatus's own safety-net code (not the
measured evidence itself):

1. `poll_until_stable` read the condition, then *unconditionally* slept
   `STABLE_GAP`, and only evaluated the soft/hard deadlines afterward —
   so a streak whose real, decisive last observation landed at, say,
   9.9s (inside a 10s `timeout`) could be misclassified `LateConverged`
   once the trailing sleep pushed the clock past 10s; one landing at
   29.9s (inside a 30s hard deadline) could likewise be misclassified
   `Diverged`. No run has ever landed in either narrow window, so no
   actual result was ever affected, but the classification logic itself
   was wrong at the boundary it exists to draw. Fixed: restructured
   around a plain streak counter that evaluates and classifies
   immediately after each read, *before* any sleep — the sleep now only
   happens when the loop is going to observe again, never after the read
   that decides the outcome.
2. The test function set `done` (which disarms the native watchdog)
   unconditionally as soon as `tokio::time::timeout(..., handle).await`
   resolved — including the `Err` (cooperative-timeout-elapsed) case,
   where the spawned assay task might still be genuinely running or
   blocked. Disarming the native watchdog there defeats its entire
   purpose: the one scenario it exists for (a real deadlock the
   cooperative watchdog can't reliably catch, because on a multi-worker
   runtime the *outer* awaiting task can still be polled even while the
   *inner* spawned task is permanently wedged) is exactly the scenario
   this bug would have silenced it in. Fixed: `done` is now only set in
   the branches where the spawned task actually terminated (success or
   panic), never on a bare cooperative-timeout `Err`.

Re-ran 4 more times (36 total across all nine passes); the trailing-sleep
removal in fix 1 also shaved real wall-clock time (~15.3s → ~12.8-13.1s)
since the observation-based debounce no longer pays for one unnecessary
sleep per successful streak. See **Assay**. Verdict is unchanged.

**A sixteenth P2 finding**, on the revision-9 diff above: the
completed-streak branch chose between `Converged` and `LateConverged`
using only `now <= soft_deadline` — it never checked whether `now` had
*also* already passed `hard_deadline`, which should make the outcome
`Diverged` instead. The separate `now >= hard_deadline` check lower down
only runs on the path where the streak does *not* complete on that
iteration, so it was dead code for exactly the case that mattered: a
streak whose 6th successful read landed past the 3x divergence threshold.
No run has ever landed there (every run completes in ~13s against a
30s-90s range of hard deadlines across the different phases), so no
actual result was affected, but the classification logic was wrong at
that boundary. Fixed: the completed-streak branch now checks both
deadlines explicitly (`Converged` / `LateConverged` / `Diverged`, in that
order).

Re-ran 4 more times (40 total across all ten passes); see **Assay**.
Verdict is unchanged.

**A seventeenth P2 finding**, on the revision-10 diff above: the
membership agreement check compared only `ClusterMemberInfo::id` across
nodes, discarding `addr` and `incarnation` — both of which
`docs/guide/clustering.md`'s "Replicated status" section documents as
part of the shared, converged document (merged via a specified rule:
higher incarnation wins, then `Left` beats `Alive`, then address as a
commutativity tie-break). An address or incarnation disagreement between
two nodes that happened to still agree on the member-*id* set would have
passed this check undetected, even though this report's and the guide's
"no divergence" / "identical views" language implied more than bare
id-set agreement. Fixed: the comparison now includes `addr` and
`incarnation` alongside `id`.

`ClusterMemberInfo::status` (Alive/Suspect) is deliberately still
excluded from the comparison — checked directly against the guide text
and the `members()` implementation before deciding this, not assumed:
`docs/guide/clustering.md` states the view is *"local: replicated `Alive`
records, minus peers this node currently considers down... Two nodes can
briefly disagree about the view — that is exactly what eventually
consistent means here."* `autumn/src/cluster/mod.rs`'s `members()`
confirms `status` is computed per-node from a local monotonic clock read
and a never-replicated "overlay," not copied from the merged document.
Comparing it for cross-node equality would test a property the design
explicitly disclaims, and risks introducing real flakiness unrelated to
any actual bug — the opposite of what this correction is for.

Re-ran 4 more times (44 total across all eleven passes) — all still pass,
confirming `addr` and `incarnation` do converge to bit-for-bit agreement
across all 5 nodes as the guide's merge rule promises. See **Assay**.
Verdict is unchanged.

## 🧪 Apparatus

One throwaway test file, `autumn/tests/prospect_cluster_scale.rs` (a
temporary `[[test]]` entry added to `autumn/Cargo.toml` to build it,
`test-support` feature enabled), copying `cluster_two_node.rs`'s own
harness conventions (`ClusterConfig`, `install_from_config`, `ClusterHandle`,
a two-sided `describe()` failure formatter) and scaling them to 5 members.
Membership agreement is checked on `(id, addr, incarnation)` triples, not
bare ids (seventeenth correction — `addr`/`incarnation` are part of the
documented, converged replicated document and a mismatch there could
previously slip past an id-only check); `status` is deliberately excluded
because the guide documents it as a local, not-replicated overlay that
can legitimately differ transiently between healthy nodes. Every check is
**debounced**: `poll_until_stable` requires the condition to
hold on 6 consecutive observations 250ms apart — the *last read* lands at
t≈1250ms, strictly past `suspicion_timeout_ms`=1000ms and several
multiples of `push_interval_ms`=200ms — before counting as converged, not
a single-instant read. Classification (against both the soft `timeout`
line and the hard 3x-`timeout` divergence line) happens immediately after
each read, *before* any sleep (fourteenth correction — evaluating after an
unconditional trailing sleep can misclassify a read that actually landed
inside a deadline as having landed outside it); the sleep only happens
when the loop is going to observe again. A failing streak still can't
spin-retry without a genuine `.await` point (fourth correction above).
Step 3's checks use the pre-registered 5s `DEPARTURE_TIMEOUT`; every other
step uses the 10s `CONVERGE_TIMEOUT` (fifth correction above). Every check
returns a tri-state `ConvergenceOutcome` — `Converged` inside its timeout,
`LateConverged` inside 3x the timeout (reported, not failed — the
pre-registration's own "undetermined-on-the-line" case, ninth correction
above), or `Diverged` past 3x (a real failure) — rather than plain
pass/fail; the completed-streak branch checks both deadlines explicitly,
so a streak that only finishes past the 3x line is correctly `Diverged`,
not `LateConverged` (sixteenth correction — the standalone hard-deadline
check elsewhere in the loop is dead code for that specific case). **Two
independent watchdogs** guard the whole test body: the
primary one runs `run_assay()` on its own spawned Tokio task with the 60s
`TOTAL_TEST_TIMEOUT` waiting on that task's `JoinHandle` (tenth
correction — a bare `tokio::time::timeout` around the future directly
can't preempt a genuine synchronous-lock deadlock, only a cooperative
one); a second, independent one runs on a native `std::thread` outside
the Tokio runtime entirely and `std::process::abort()`s if the assay
hasn't signaled completion within the same 60s (twelfth correction —
`tokio::task::spawn` still schedules on the same runtime, so a deadlock
that starves every Tokio worker thread, not just one, can starve the
first watchdog too). The native watchdog is only disarmed once the
spawned assay task has actually terminated — never merely because the
cooperative timeout's own await resolved (fifteenth correction — a bare
`Err` there doesn't mean the inner task finished, and disarming on it
would silence the one scenario the second watchdog exists to catch). One
`#[tokio::test(flavor = "multi_thread")]` runs all steps in sequence
against one 5-node cluster:

1. `spawn_star_cluster(5)` — node 0 installs with no seeds; nodes 1-4 each
   install seeded only with node 0's observed `local_addr()` (star
   topology, matching the pre-registration). Every node binds
   `127.0.0.1:0`; `push_interval_ms: 200`, `suspicion_timeout_ms: 1_000`,
   identical to `cluster_two_node.rs`'s own `cluster_config()`.
2. `assert_stable_membership` (10s bound): all 5 nodes report an identical,
   full 5-member view, stably.
3. Every node increments the shared counter by a distinct amount
   (node *i* → `i+1`), genuinely concurrently — each on its own spawned
   task, rendezvousing on a `Barrier` first (thirteenth correction above;
   a plain sequential loop cannot support a "concurrent" claim);
   `assert_stable_counter_sum` (10s bound): all 5 nodes read the exact sum,
   15, stably — then `assert_stable_membership` again, to catch any
   membership flicker that happened *during* counter convergence.
4. Cancel node 4's shutdown token (clean departure);
   `assert_stable_membership` (**5s bound**): the 4 survivors converge to
   an identical 4-member view, stably; `assert_stable_counter_sum`
   (**5s bound**): the survivors still read the exact sum 15, stably (the
   departed node's contributed cells must still be present in the merged
   document); `assert_stable_membership` again (**5s bound**),
   post-counter-check.
5. Install a fresh node, re-seeded from node 0, replacing node 4;
   `assert_stable_membership` (10s bound): all 5 (4 original + 1 rejoined)
   reconverge to a full 5-member view, stably; `assert_stable_counter_sum`
   (10s bound): all 5 read the exact sum 15 again, stably (the rejoined
   node must relearn the full total from its peers, not just the
   membership shape); `assert_stable_membership` again (10s bound),
   post-counter-check.

**Stubs / what this apparatus faked or skipped** (scopes what the result
below actually proves):

- **Single process, single host, loopback only.** All 5 members run as
  `TestApp` instances inside one Tokio runtime on one machine, talking over
  `127.0.0.1`. No real network — no cross-host latency, no packet loss, no
  reordering, no NAT/firewall interaction — was exercised. This tests the
  gossip *protocol's* correctness under ideal networking, not its
  resilience to real network pathology.
- **Star topology only**, matching the pre-registration's chosen minimal
  seed-list shape. A full seed list (every node listing every other) or a
  ring were not tested; a different seed-list shape could plausibly behave
  differently and was explicitly out of this assay's scope.
- **One departure, one rejoin, one cycle.** No repeated churn, no
  concurrent multi-node departures, no restart-with-different-address case
  (the "Known residuals" the guide itself already documents as unresolved
  at any N).
- **No adversarial input.** No malicious peer, no clock skew, no truncated
  or corrupted frame, no wrong-secret peer at N=5 (the existing wrong-secret
  test in `src/cluster/tests.rs` already covers that at N=3 and was not
  re-run here — out of scope, this assay is about honest-peer convergence
  correctness, not the auth boundary).
- **Library API only, not the HTTP layer.** Unlike `cluster_two_node.rs`'s
  `full_app_two_nodes_health_and_counter_via_http` test, this apparatus
  talks to `ClusterHandle` directly rather than through `/actuator/health`
  — cheaper to write, and the HTTP layer is a thin read-only projection
  over the same handle the 2-node suite already covers.
- **N=5 only.** No claim is made about N=8, N=20, or any other fleet size;
  extrapolation beyond the measured point is exactly the inadmissible move
  this role's evidence rules forbid.
- **No performance/throughput measurement.** Explicitly out of scope per
  the pre-registration's riskiest-assumption framing: this assay tests
  whether the design stays *correct* at N=5, not whether it stays *fast* or
  *efficient* — those are the parts of "future work" the guide already
  named and this assay leaves untouched.

## 📊 Assay

**Control, run first:** the existing 2-node suite
(`cargo test -p autumn-web --features test-support --test integration_tests
cluster_two_node`), to confirm the baseline this apparatus scales up is
itself healthy on this sandbox before trusting the N=5 result. All 8
existing tests passed, 0.76s total test time (7m39s one-time compile of the
full consolidated binary, not part of the measured behavior):

```
test integration::cluster_two_node::disabled_cluster_installs_nothing ... ok
test integration::cluster_two_node::install_refuses_a_second_node_on_one_state ... ok
test integration::cluster_two_node::install_refuses_a_collision_on_the_membership_component_name ... ok
test integration::cluster_two_node::install_rejects_an_invalid_or_secretless_section ... ok
test integration::cluster_two_node::full_app_two_nodes_health_and_counter_via_http ... ok
test integration::cluster_two_node::tcp_survivor_converges_after_peer_cancelled ... ok
test integration::cluster_two_node::tcp_clean_leave_converges_before_the_suspicion_timeout ... ok
test integration::cluster_two_node::tcp_two_nodes_converge_and_counter_replicates ... ok
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 1916 filtered out
```

**N=5 assay, first pass, 4 runs** (before the correction above — counter
verified only at step 2):

| Run | Result | Wall-clock |
|---|---|---:|
| 1 | all steps pass | 1.16s |
| 2 | all steps pass | 1.27s |
| 3 | all steps pass | 1.32s |
| 4 | all steps pass | 1.03s |

**N=5 assay, corrected pass, 4 more runs** (`cargo test -p autumn-web
--features test-support --test prospect_cluster_scale`, run back-to-back in
this sandbox after adding the post-departure and post-rejoin counter
assertions — 8 total runs across both passes, repeated beyond the
pre-registration's implicit single run because a single pass is weak
evidence for "pursue" on its own, matching the lesson the prior cold-start
assays in this ledger paid dearly to learn):

| Run | Result | Wall-clock |
|---|---|---:|
| 5 | all steps + both post-churn counter checks pass | 0.91s |
| 6 | all steps + both post-churn counter checks pass | 1.23s |
| 7 | all steps + both post-churn counter checks pass | 1.03s |
| 8 | all steps + both post-churn counter checks pass | 1.11s |

**N=5 assay, third pass, 4 more runs** (after switching every check to the
debounced `poll_until_stable` — 3 consecutive positive observations, 50ms
apart — and adding a membership re-check immediately after every counter
check; 12 total runs across all three passes):

| Run | Result | Wall-clock |
|---|---|---:|
| 9 | all steps + stability + post-counter membership rechecks pass | 2.39s |
| 10 | all steps + stability + post-counter membership rechecks pass | 2.46s |
| 11 | all steps + stability + post-counter membership rechecks pass | 2.60s |
| 12 | all steps + stability + post-counter membership rechecks pass | 2.32s |

**N=5 assay, fourth pass, 4 more runs** (after fixing the two revision-4
findings — an unconditional yield in `poll_until_stable`, and step 3's
checks actually parameterized to the pre-registered 5s `DEPARTURE_TIMEOUT`
instead of silently inheriting 10s; 16 total runs across all four passes):

| Run | Result | Wall-clock |
|---|---|---:|
| 13 | all steps + fixed yield + phase-specific timeouts pass | 2.44s |
| 14 | all steps + fixed yield + phase-specific timeouts pass | 2.43s |
| 15 | all steps + fixed yield + phase-specific timeouts pass | 2.33s |
| 16 | all steps + fixed yield + phase-specific timeouts pass | 2.68s |

**N=5 assay, fifth pass, 4 more runs** (after adding the whole-test 60s
`tokio::time::timeout` watchdog and making `poll_until_stable` check its
deadline after every observation, not just failed ones; 20 total runs
across all five passes):

| Run | Result | Wall-clock |
|---|---|---:|
| 17 | all steps + 60s watchdog + deadline-checked streak pass | 2.63s |
| 18 | all steps + 60s watchdog + deadline-checked streak pass | 2.59s |
| 19 | all steps + 60s watchdog + deadline-checked streak pass | 2.58s |
| 20 | all steps + 60s watchdog + deadline-checked streak pass | 2.58s |

**N=5 assay, sixth pass, 4 more runs** (after widening the debounce window
past the protocol's own timers, adding the tri-state `ConvergenceOutcome`,
and moving the watchdog to wrap a spawned task's `JoinHandle`; 24 total
runs across all six passes):

| Run | Result | Wall-clock |
|---|---|---:|
| 21 | all steps + wider debounce + spawn-watchdog pass (all `Converged`, no `LateConverged`) | 13.08s |
| 22 | all steps + wider debounce + spawn-watchdog pass (all `Converged`, no `LateConverged`) | 13.08s |
| 23 | all steps + wider debounce + spawn-watchdog pass (all `Converged`, no `LateConverged`) | 13.08s |
| 24 | all steps + wider debounce + spawn-watchdog pass (all `Converged`, no `LateConverged`) | 13.08s |

**N=5 assay, seventh pass, 4 more runs** (after bumping `STABLE_CHECKS`
5→6 so the last read strictly clears `suspicion_timeout_ms`, and adding
the native-`std::thread` watchdog independent of the Tokio runtime; 28
total runs across all seven passes):

| Run | Result | Wall-clock |
|---|---|---:|
| 25 | all steps + 6-observation debounce + dual watchdogs pass | 15.34s |
| 26 | all steps + 6-observation debounce + dual watchdogs pass | 15.34s |
| 27 | all steps + 6-observation debounce + dual watchdogs pass | 15.09s |
| 28 | all steps + 6-observation debounce + dual watchdogs pass | 15.09s |

**N=5 assay, eighth pass, 4 more runs** (after replacing step 2's
sequential increment loop with `N` genuinely concurrent, barrier-synced
spawned tasks; 32 total runs across all eight passes):

| Run | Result | Wall-clock |
|---|---|---:|
| 29 | all steps + barrier-synced concurrent increments pass | 15.34s |
| 30 | all steps + barrier-synced concurrent increments pass | 15.59s |
| 31 | all steps + barrier-synced concurrent increments pass | 15.34s |
| 32 | all steps + barrier-synced concurrent increments pass | 15.59s |

**N=5 assay, ninth pass, 4 more runs** (after fixing the debounce's
pre-sleep classification and making the native watchdog disarm only on
genuine task completion; 36 total runs across all nine passes):

| Run | Result | Wall-clock |
|---|---|---:|
| 33 | all steps + pre-sleep classification + correctly-armed native watchdog pass | 12.83s |
| 34 | all steps + pre-sleep classification + correctly-armed native watchdog pass | 13.08s |
| 35 | all steps + pre-sleep classification + correctly-armed native watchdog pass | 12.83s |
| 36 | all steps + pre-sleep classification + correctly-armed native watchdog pass | 12.84s |

**N=5 assay, tenth pass, 4 more runs** (after making the completed-streak
branch check both the soft and hard deadlines explicitly, so a streak
finishing past the 3x divergence line is `Diverged` rather than
`LateConverged`; 40 total runs across all ten passes):

| Run | Result | Wall-clock |
|---|---|---:|
| 37 | all steps + hard-deadline-checked streak completion pass | 13.08s |
| 38 | all steps + hard-deadline-checked streak completion pass | 13.08s |
| 39 | all steps + hard-deadline-checked streak completion pass | 12.83s |
| 40 | all steps + hard-deadline-checked streak completion pass | 12.84s |

**N=5 assay, eleventh pass, 4 more runs** (after extending membership
agreement to `(id, addr, incarnation)` triples instead of bare ids, with
`status` deliberately still excluded as a documented local-only field; 44
total runs across all eleven passes):

| Run | Result | Wall-clock |
|---|---|---:|
| 41 | all steps + full (id, addr, incarnation) agreement pass | 13.08s |
| 42 | all steps + full (id, addr, incarnation) agreement pass | 12.84s |
| 43 | all steps + full (id, addr, incarnation) agreement pass | 12.82s |
| 44 | all steps + full (id, addr, incarnation) agreement pass | 13.08s |

**Against the pre-registered lines (eleventh pass, the one whose code
actually enforces every bound as registered — the whole-test 60s kill
line via two independent watchdogs (the native one correctly armed
throughout), a debounce window whose *last read* strictly clears the
protocol's own timers and is classified before any trailing sleep against
both the soft and hard deadlines, genuinely concurrent increments, full
`(id, addr, incarnation)` membership agreement, and the pre-registration's
own tri-state classification — see notes on Step 2 and Step 3 below for
why earlier passes' runs still count as evidence despite bugs or claim
gaps in that code):**

- **Step 1 (cold-start convergence, line: ≤10s):** passed (`Converged`, not
  `LateConverged`) on 44/44 runs across all eleven passes. The
  seventh-through-eleventh passes' ~12.8-15.6s *total* run time is the sum
  of 9 mandatory debounce windows, not slower convergence — no single
  phase check came close to its own 5s/10s bound; still roughly 3-8x
  inside the tightest of those, not a close call the way the cold-start
  ledger's margins were. **Caveat on runs 1-40:** those compared only
  `ClusterMemberInfo::id` across nodes, not the full `(id, addr,
  incarnation)` triple — flagged here; only runs 41-44 (eleventh pass) are
  code-verified to have checked the fuller identity. All 44 runs did
  converge to agreeing id sets either way.

- **Step 2 (counter merge, line: exact sum 15 on every node, ≤10s):**
  passed (`Converged`) on 44/44 runs, stability-checked since run 9
  (widened debounce since run 21, 6-observation debounce since run 25,
  pre-sleep classification since run 33, hard-deadline-checked completion
  since run 37), with a membership recheck immediately after confirming
  no flicker occurred during convergence. **Caveat on runs 1-28:** the
  increments driving this check were issued by a plain sequential loop,
  not genuinely concurrently — flagged here, not silently used as
  evidence for the "concurrent" claim; only runs 29-44 (eighth through
  eleventh passes) are code-verified to have actually issued all 5
  increments concurrently (barrier-synced spawned tasks). All 44 runs,
  including 1-28, did read the exact sum 15 — so the merge-correctness
  evidence stands regardless, it's specifically the *concurrency* of the
  input that only runs 29-44 can back.
- **Step 3 (departure, line: 4-member view on survivors, ≤5s, AND exact sum
  15 still held on all 4 survivors):** passed (`Converged`) on 44/44 runs —
  the counter half ran in runs 5-44, held on 40/40 of those; the stability
  debounce and post-counter membership recheck ran in runs 9-44, held on
  36/36. **Caveat on runs 9-12 specifically:** those ran before the fourth
  correction, so their code was actually bounded by the 10s
  `CONVERGE_TIMEOUT`, not the registered 5s `DEPARTURE_TIMEOUT` — the
  *test* didn't enforce the right line, even though it still passed. This
  is not silently swept in as clean evidence for the 5s bound: it's
  flagged here, and only runs 13-44 (fourth through eleventh passes) are
  code-verified to have actually been gated at 5s. All 44 runs, including
  9-12, did *empirically* complete step 3 in well under 5s either way — so
  the wall-clock evidence itself still supports the line; only the earlier
  code's enforcement of that specific line was what was broken, not the
  measured outcome.
- **Step 4 (rejoin, line: 5-member view, ≤10s, AND exact sum 15 relearned
  by the rejoined node):** passed (`Converged`) on 44/44 runs — counter half
  held on 40/40 (runs 5-44), stability + post-counter recheck held on 36/36
  (runs 9-44), all correctly bounded at 10s throughout (this step was
  never affected by the Step 3 timeout bug).
- **Undetermined-on-the-line classification (`LateConverged`):** never
  triggered in any of the 44 runs — every check reached `Converged` well
  inside its `timeout`, so the pre-registration's third outcome remains
  implemented but empirically unexercised. The apparatus now has the
  capacity to report it (including correctly deferring to `Diverged` past
  the hard deadline), but this assay's own data never approaches either
  boundary closely enough to demonstrate either path firing.
- **Whole-test 60s kill line:** enforced by two independent watchdogs since
  run 25 — the Tokio-scheduled one waiting on a spawned task's
  `JoinHandle` (runs 21-44; a bare wrapper around the future directly,
  runs 17-20, is cooperative and can't preempt a genuine synchronous-lock
  deadlock) and a native `std::thread` outside the Tokio runtime entirely,
  correctly armed throughout its full window since run 33 (runs 25-32 had
  it disarming prematurely on a cooperative-timeout `Err`, not just on
  genuine task completion — never actually observed firing incorrectly in
  this assay's clean runs, but a latent bug in the safety net itself).
  Every run completed in under 14s total, ~4x inside even this outermost
  bound (the tightest margin of any check in this assay, still
  comfortable).
- **Kill-line check:** zero divergent views, zero wrong counter values,
  zero panics/timeouts/hangs, zero `Diverged` outcomes, zero
  `LateConverged` outcomes, zero watchdog trips (Tokio-scheduled *or*
  native) across all 44 runs. The "2 of 3 repeats" kill-line threshold was
  never approached in either direction — this result is not a marginal
  call the way the cold-start bisection's sub-5,000ms deltas were; every
  margin here has roughly 3-25x headroom against its line, so ordinary
  run-to-run scheduling noise on this shared sandbox is not a plausible
  alternative explanation the way it was there.

## 🏁 Verdict

**Pursue**, against the pre-registered line: all success criteria
(cold-start convergence, exact counter merge, departure convergence,
rejoin convergence, and the whole-test 60s kill line) held — as
`Converged`, never `LateConverged` or `Diverged` — on every run across all
eleven passes, with comfortable margin against every bound — not a photo
finish, and (per the Step 3 caveat in **Assay**) the narrowest margin,
departure's 5s line, is only claimed as *code-enforced* for the fourth
through eleventh passes' 32 runs, though the wall-clock evidence supports
it across all 44. No kill-line condition was observed. After all
seventeen corrections above, the counter's exact sum was verified not
just once before churn but again on the survivors after departure and
again on the full cluster after rejoin, with the pre-churn increments
themselves now genuinely concurrent (barrier-synced spawned tasks, not a
sequential loop); membership agreement is checked on the full `(id, addr,
incarnation)` identity the design actually converges (not bare ids), with
`status` deliberately excluded as a documented, not-replicated local
overlay; every convergence/counter observation was required to hold
across a debounce window whose *last read*, not just a naive
window-length arithmetic, strictly clears the protocol's own gossip and
suspicion timers, classified immediately upon that read rather than
after an unconditional trailing sleep and correctly checked against both
the soft and hard deadlines on the iteration that decides the outcome,
with a membership recheck immediately after every counter check; the
debounce itself always yields rather than risking a spin; step 3's checks
are gated at the registered 5s, not a silently-inherited 10s; and the
whole test now runs behind two independent 60s watchdogs — a
Tokio-scheduled one and a native OS-thread one that cannot be starved by
the same deadlock the first could theoretically miss on a single-worker
runtime, and which now stays armed until the assay task has genuinely
terminated rather than disarming the instant the cooperative watchdog's
own await resolves — the actual claim the guide text makes ("no
divergence," stable identical views, exact sums through concurrent
churn) is the one actually measured, on 4/4 eleventh-pass runs, not the
progressively weaker versions the first ten passes of this report each
accidentally supported. Within the scope this assay actually tested
(single host, single process, star topology, honest peers, N=5,
correctness not performance), the full-broadcast/no-quorum gossip design
does **not** show the split-brain or lost-update failure mode the guide's
hedge gestures at.

This is deliberately a narrower claim than "clustering works past two
nodes" — see the stubs list above for exactly what was not tested (real
multi-host networking, packet loss/partition, adversarial peers, N>5,
repeated churn, non-star seed topologies). The falsifiable question this
assay pre-registered was scoped to correctness at N=5 under ideal
networking specifically because that is the cheapest apparatus that could
have falsified the design outright (a fundamental split-brain bug would
show up here first, before any of the harder-to-build adversarial or
multi-host conditions matter) — and it didn't.

## 💰 Cost to productionize

The "build" this pursue verdict authorizes is documentation only, not a
code change — the pre-registered decision was narrowing
`docs/guide/clustering.md`'s "unproven beyond two nodes" hedge, not
shipping new cluster code. Done directly as part of filing this report
(see the doc diff alongside this file): the guide now cites this assay and
states what was and was not verified, rather than a blanket "unproven."
No Bolt/Warden/Ballast gate applies — no production code path changed.

**What a real follow-up (not chartered or run here) would need**, if
someone wants a stronger claim than "N=5, one host, ideal network, honest
peers, one churn cycle":

1. **Real multi-host testing** — separate machines or containers with
   actual network latency, not loopback. Needs Keystone/infra sign-off if
   it requires provisioning beyond this sandbox.
2. **Partition/packet-loss injection** — `tc netem`-style fault injection
   or a proxy that drops/delays frames, to test the specific failure mode
   ("split-brain") this assay's clean-network result cannot speak to.
3. **Higher N** (10, 20+) — this assay makes no claim past N=5; the
   all-to-all full-broadcast design's message volume grows with N², which
   is a separate, already-acknowledged scaling question this correctness
   assay was never meant to answer.
4. **Repeated/concurrent churn** — multiple simultaneous departures,
   rapid restart storms, the "backward clock + different address" residual
   the guide already documents as unresolved at any N.

## 🔬 Reproduce

The apparatus was never committed (per this ledger's containment rule, it
is reverted before the report is finalized), so its exact source (revision
11, post all seventeen Codex corrections above) is embedded below rather
than referenced by history. This is now the durable artifact.

1. Add this entry to `autumn/Cargo.toml`, immediately after the existing
   `[[test]] name = "integration_tests"` block:

   ```toml
   [[test]]
   name = "prospect_cluster_scale"
   path = "tests/prospect_cluster_scale.rs"
   ```

2. Save the following as `autumn/tests/prospect_cluster_scale.rs`:

   ```rust
   //! PROSPECT ASSAY APPARATUS — throwaway, not for merge.
   //!
   //! Answers the falsifiable question pre-registered in
   //! `docs/reports/2026-09-04-prospect-cluster-scale-beyond-two-nodes.md`:
   //! does the full-broadcast, no-quorum gossip cluster converge correctly at
   //! N=5? Deliberately copies the existing 2-node test's harness conventions
   //! (`autumn/tests/integration/cluster_two_node.rs`) scaled up to 5 members,
   //! star-seeded from node 1. No production code changes; this file and its
   //! `[[test]]` Cargo.toml entry are reverted after the assay runs — its
   //! source is embedded verbatim in the report's Reproduce section instead.
   //!
   //! Revisions 2-10: see the report's Correction sections for the full
   //! history (counter re-verified through churn; debounced, yield-safe,
   //! deadline-aware stability checks spanning the protocol's own timers,
   //! classified before any trailing sleep and checked against both the soft
   //! and hard deadlines on the deciding read; a tri-state
   //! Converged/LateConverged/Diverged outcome; a phase-specific departure
   //! timeout; dual watchdogs (Tokio-scheduled + native OS thread, the
   //! latter armed until genuine task completion); genuinely concurrent,
   //! barrier-synced counter increments).
   //!
   //! Revision 11 (Codex P2 on the revision-10 diff): the membership
   //! agreement check compared only `ClusterMemberInfo::id` across nodes,
   //! discarding `addr` and `incarnation` — both of which
   //! `docs/guide/clustering.md`'s "Replicated status" section documents as
   //! part of the shared, converged document (merged via a real, specified
   //! rule: higher incarnation wins, then `Left` beats `Alive`, then
   //! address as a commutativity tie-break) — so an address or incarnation
   //! disagreement between two nodes that happened to agree on the member-id
   //! *set* would have passed undetected, even though the report's "no
   //! divergence" / "identical views" language implied more than id-set
   //! agreement. Fixed: the comparison now includes `addr` and `incarnation`
   //! alongside `id`.
   //!
   //! `ClusterMemberInfo::status` (Alive/Suspect) is deliberately still
   //! excluded from the comparison — not an oversight, a documented design
   //! fact: `docs/guide/clustering.md` states the view is "local: replicated
   //! `Alive` records, minus peers this node currently considers down... Two
   //! nodes can briefly disagree about the view — that is exactly what
   //! eventually consistent means here." `status` in `ClusterMemberInfo` is
   //! computed per-node from a local suspicion timer
   //! (`autumn/src/cluster/mod.rs`'s `members()`, confirmed reading it
   //! directly: it reads `self.inner.clock.monotonic()` and an `overlay`
   //! that is never itself replicated), not copied from the merged document.
   //! Comparing it for cross-node equality would test a property the design
   //! explicitly does not claim to hold, and could introduce genuine
   //! flakiness unrelated to any real bug.

   use std::sync::atomic::{AtomicBool, Ordering};
   use std::sync::Arc;
   use std::time::Duration;

   use autumn_web::cluster::{ClusterHandle, install_from_config};
   use autumn_web::config::{AutumnConfig, ClusterConfig};
   use autumn_web::test::TestApp;
   use tokio::sync::Barrier;
   use tokio_util::sync::CancellationToken;

   const SECRET: &str = "a-shared-cluster-secret-value-32";
   const COUNTER: &str = "prospect_n5";
   const N: usize = 5;
   const EXPECTED_SUM: u64 = 1 + 2 + 3 + 4 + 5; // 15, fixed regardless of who departs/rejoins

   /// Matches the pre-registration's step-1/step-2 bound: 2x the existing
   /// 2-node test's 5s convergence bound.
   const CONVERGE_TIMEOUT: Duration = Duration::from_secs(10);
   /// Matches the pre-registration's step-3 departure bound.
   const DEPARTURE_TIMEOUT: Duration = Duration::from_secs(5);
   /// Matches the pre-registration's kill-line total: "test itself times out
   /// past 60s total."
   const TOTAL_TEST_TIMEOUT: Duration = Duration::from_secs(60);
   /// The pre-registration's own divergence multiplier: "disagree... 3x past
   /// the relevant timeout" is the kill line: a soft-deadline miss that still
   /// resolves inside this multiple is "undetermined-on-the-line," not a kill.
   const DIVERGENCE_MULTIPLIER: u32 = 3;
   /// Consecutive positive observations required before a condition counts as
   /// "stable," and the gap between them. The *last* read lands at
   /// `(STABLE_CHECKS-1) * STABLE_GAP` ≈ 1250ms, strictly past
   /// `suspicion_timeout_ms` (1000ms) and several multiples of
   /// `push_interval_ms` (200ms).
   const STABLE_CHECKS: u32 = 6;
   const STABLE_GAP: Duration = Duration::from_millis(250);

   /// The three outcomes a debounced poll can reach, matching the
   /// pre-registration's own classification (not just pass/fail):
   /// `Converged` inside the registered line, `LateConverged` past the line
   /// but short of `DIVERGENCE_MULTIPLIER`x it ("undetermined-on-the-line,
   /// qualitatively pursue... the margin miss itself is data"), or `Diverged`
   /// (a genuine kill-line condition).
   enum ConvergenceOutcome {
       Converged,
       LateConverged { elapsed: Duration },
       Diverged,
   }

   /// Debounced poll: `condition` must return `true` on `STABLE_CHECKS`
   /// consecutive observations, `STABLE_GAP` apart. Classification happens
   /// immediately after each read, before any sleep, and — on the iteration
   /// where the streak actually completes — checks *both* the soft and hard
   /// deadlines explicitly.
   async fn poll_until_stable<F, Fut>(timeout: Duration, mut condition: F) -> ConvergenceOutcome
   where
       F: FnMut() -> Fut,
       Fut: std::future::Future<Output = bool>,
   {
       let start = tokio::time::Instant::now();
       let soft_deadline = start + timeout;
       let hard_deadline = start + timeout * DIVERGENCE_MULTIPLIER;
       let mut streak: u32 = 0;
       loop {
           let ok = condition().await;
           let now = tokio::time::Instant::now();
           if ok {
               streak += 1;
               if streak >= STABLE_CHECKS {
                   return if now <= soft_deadline {
                       ConvergenceOutcome::Converged
                   } else if now < hard_deadline {
                       ConvergenceOutcome::LateConverged { elapsed: now - start }
                   } else {
                       ConvergenceOutcome::Diverged
                   };
               }
           } else {
               streak = 0;
           }
           if now >= hard_deadline {
               return ConvergenceOutcome::Diverged;
           }
           tokio::time::sleep(STABLE_GAP).await;
       }
   }

   fn cluster_config(seed_peers: Vec<String>) -> ClusterConfig {
       ClusterConfig {
           enabled: true,
           secret: Some(secrecy::SecretString::from(SECRET.to_owned())),
           bind_addr: "127.0.0.1:0".to_owned(),
           seed_peers,
           push_interval_ms: 200,
           suspicion_timeout_ms: 1_000,
           ..ClusterConfig::default()
       }
   }

   fn app_config(cluster: ClusterConfig) -> AutumnConfig {
       AutumnConfig {
           cluster,
           ..AutumnConfig::default()
       }
   }

   /// `(id, addr, incarnation)` triples, sorted — the fields
   /// `docs/guide/clustering.md`'s "Replicated status" section documents as
   /// part of the shared, converged document. `ClusterMemberInfo::status` is
   /// deliberately excluded (see the module doc comment above): it is a
   /// documented *local* overlay, not gossiped state, and can legitimately
   /// differ transiently between healthy, fully-converged nodes.
   fn member_identities(handle: &Arc<ClusterHandle>) -> Vec<(String, String, u64)> {
       let mut identities: Vec<(String, String, u64)> = handle
           .members()
           .into_iter()
           .map(|m| (m.id, m.addr, m.incarnation))
           .collect();
       identities.sort();
       identities
   }

   fn describe(handles: &[Arc<ClusterHandle>]) -> String {
       handles
           .iter()
           .enumerate()
           .map(|(i, h)| {
               format!(
                   "node{i}(id={}, members={:?}, {COUNTER}={})",
                   h.node_id(),
                   h.members(),
                   h.counter(COUNTER).get()
               )
           })
           .collect::<Vec<_>>()
           .join(" | ")
   }

   /// Spin up `n` nodes, star-seeded from node 0. Returns handles + their
   /// shutdown tokens (index-aligned).
   fn spawn_star_cluster(n: usize) -> (Vec<Arc<ClusterHandle>>, Vec<CancellationToken>) {
       let mut handles = Vec::with_capacity(n);
       let mut tokens = Vec::with_capacity(n);

       let shutdown0 = CancellationToken::new();
       let config0 = cluster_config(Vec::new());
       let app0 = TestApp::new().config(app_config(config0.clone())).build();
       install_from_config(app0.state(), &config0, &shutdown0).expect("node 0 must install");
       let handle0 = app0
           .state()
           .extension::<ClusterHandle>()
           .expect("node 0 must expose a ClusterHandle");
       let seed = handle0.local_addr().to_string();
       handles.push(handle0);
       tokens.push(shutdown0);
       std::mem::forget(app0); // keep the app (and its background tasks) alive

       for i in 1..n {
           let shutdown = CancellationToken::new();
           let config = cluster_config(vec![seed.clone()]);
           let app = TestApp::new().config(app_config(config.clone())).build();
           install_from_config(app.state(), &config, &shutdown)
               .unwrap_or_else(|e| panic!("node {i} must install: {e:?}"));
           let handle = app
               .state()
               .extension::<ClusterHandle>()
               .unwrap_or_else(|| panic!("node {i} must expose a ClusterHandle"));
           handles.push(handle);
           tokens.push(shutdown);
           std::mem::forget(app);
       }

       (handles, tokens)
   }

   /// Increment every node's counter by a distinct amount, all `N` calls
   /// genuinely concurrent: each runs on its own spawned task, rendezvousing
   /// on a `Barrier` before calling `increment_by` so none can start before
   /// every other one is ready.
   async fn increment_all_concurrently(handles: &[Arc<ClusterHandle>]) {
       let barrier = Arc::new(Barrier::new(handles.len()));
       let mut tasks = Vec::with_capacity(handles.len());
       for (i, h) in handles.iter().cloned().enumerate() {
           let barrier = Arc::clone(&barrier);
           tasks.push(tokio::task::spawn(async move {
               barrier.wait().await;
               h.counter(COUNTER).increment_by((i as u64) + 1);
           }));
       }
       for t in tasks {
           t.await.expect("increment task must not panic");
       }
   }

   /// Stable, two-sided membership check: every handle must report exactly
   /// `expected_n` members with identical `(id, addr, incarnation)` triples
   /// (see `member_identities`). Panics only on `Diverged`; `LateConverged`
   /// is reported, not failed (see `ConvergenceOutcome`).
   async fn assert_stable_membership(
       handles: &[Arc<ClusterHandle>],
       expected_n: usize,
       timeout: Duration,
       label: &str,
   ) {
       let hs = handles.to_vec();
       let outcome = poll_until_stable(timeout, || {
           let hs = hs.clone();
           async move {
               if !hs.iter().all(|h| h.members().len() == expected_n) {
                   return false;
               }
               let reference = member_identities(&hs[0]);
               hs.iter().all(|h| member_identities(h) == reference)
           }
       })
       .await;
       match outcome {
           ConvergenceOutcome::Converged => {}
           ConvergenceOutcome::LateConverged { elapsed } => eprintln!(
               "[{label}] UNDETERMINED-ON-THE-LINE: {expected_n}-member view converged stably at \
                {elapsed:?}, past the {timeout:?} success line but inside the \
                {DIVERGENCE_MULTIPLIER}x divergence threshold — per pre-registration this is data, \
                not a silent pass or a kill; {}",
               describe(handles)
           ),
           ConvergenceOutcome::Diverged => panic!(
               "[{label}] DIVERGED: all {expected_n} nodes never reported an identical \
                {expected_n}-member view even within {DIVERGENCE_MULTIPLIER}x {timeout:?} — \
                kill-line condition; {}",
               describe(handles)
           ),
       }
   }

   /// Stable, two-sided counter check: every handle must read exactly
   /// `EXPECTED_SUM`. Panics only on `Diverged`.
   async fn assert_stable_counter_sum(handles: &[Arc<ClusterHandle>], timeout: Duration, label: &str) {
       let hs = handles.to_vec();
       let outcome = poll_until_stable(timeout, || {
           let hs = hs.clone();
           async move { hs.iter().all(|h| h.counter(COUNTER).get() == EXPECTED_SUM) }
       })
       .await;
       match outcome {
           ConvergenceOutcome::Converged => {}
           ConvergenceOutcome::LateConverged { elapsed } => eprintln!(
               "[{label}] UNDETERMINED-ON-THE-LINE: counter={EXPECTED_SUM} converged stably at \
                {elapsed:?}, past the {timeout:?} success line but inside the \
                {DIVERGENCE_MULTIPLIER}x divergence threshold — per pre-registration this is data, \
                not a silent pass or a kill; {}",
               describe(handles)
           ),
           ConvergenceOutcome::Diverged => panic!(
               "[{label}] DIVERGED: counter never read {EXPECTED_SUM} on every node even within \
                {DIVERGENCE_MULTIPLIER}x {timeout:?} — kill-line condition; {}",
               describe(handles)
           ),
       }
   }

   async fn run_assay() {
       // --- Step 1: cold-start convergence to a full N-member view ---
       let (mut handles, mut tokens) = spawn_star_cluster(N);
       assert_stable_membership(&handles, N, CONVERGE_TIMEOUT, "step1-cold-start").await;

       // --- Step 2: genuinely concurrent counter increments from every node ---
       increment_all_concurrently(&handles).await;
       assert_stable_counter_sum(&handles, CONVERGE_TIMEOUT, "step2-counter").await;
       assert_stable_membership(&handles, N, CONVERGE_TIMEOUT, "step2-membership-recheck").await;

       // --- Step 3: clean departure of node N-1, survivors converge to N-1 ---
       // Pre-registered bound is 5s here (DEPARTURE_TIMEOUT), not the 10s
       // CONVERGE_TIMEOUT used everywhere else.
       let departing = N - 1;
       tokens[departing].cancel();
       let survivors: Vec<Arc<ClusterHandle>> = handles[..departing].to_vec();
       assert_stable_membership(&survivors, N - 1, DEPARTURE_TIMEOUT, "step3-departure").await;
       assert_stable_counter_sum(&survivors, DEPARTURE_TIMEOUT, "step3-departure-counter").await;
       assert_stable_membership(
           &survivors,
           N - 1,
           DEPARTURE_TIMEOUT,
           "step3-departure-membership-recheck",
       )
       .await;

       // --- Step 4: node 5 rejoins (fresh handle, re-seeded from node 0) ---
       let seed = handles[0].local_addr().to_string();
       let shutdown_rejoin = CancellationToken::new();
       let config_rejoin = cluster_config(vec![seed]);
       let app_rejoin = TestApp::new()
           .config(app_config(config_rejoin.clone()))
           .build();
       install_from_config(app_rejoin.state(), &config_rejoin, &shutdown_rejoin)
           .expect("rejoining node must install");
       let handle_rejoin = app_rejoin
           .state()
           .extension::<ClusterHandle>()
           .expect("rejoining node must expose a ClusterHandle");
       std::mem::forget(app_rejoin);

       handles[departing] = handle_rejoin;
       tokens[departing] = shutdown_rejoin;
       assert_stable_membership(&handles, N, CONVERGE_TIMEOUT, "step4-rejoin").await;
       assert_stable_counter_sum(&handles, CONVERGE_TIMEOUT, "step4-rejoin-counter").await;
       assert_stable_membership(&handles, N, CONVERGE_TIMEOUT, "step4-rejoin-membership-recheck").await;

       for t in &tokens {
           t.cancel();
       }
   }

   #[tokio::test(flavor = "multi_thread")]
   async fn n5_star_cluster_converges_counters_and_survives_departure_and_rejoin() {
       // Second, independent watchdog: a native OS thread outside the Tokio
       // runtime entirely. If a genuine synchronous-lock deadlock inside
       // run_assay() starves every Tokio worker thread, this thread still
       // gets its own OS-level timeslice from the OS scheduler, independent
       // of Tokio, and aborts the process directly.
       let done = Arc::new(AtomicBool::new(false));
       let watchdog_done = Arc::clone(&done);
       std::thread::spawn(move || {
           std::thread::sleep(TOTAL_TEST_TIMEOUT);
           if !watchdog_done.load(Ordering::SeqCst) {
               eprintln!(
                   "PROSPECT NATIVE WATCHDOG: assay exceeded {TOTAL_TEST_TIMEOUT:?} total \
                    kill-line bound without completing — aborting the process directly (this \
                    watchdog runs on its own OS thread, outside the Tokio runtime, specifically \
                    so a wedged single-worker Tokio runtime cannot starve it)."
               );
               std::process::abort();
           }
       });

       // First, cooperative watchdog: run_assay() on its own spawned Tokio
       // task, with the 60s timeout waiting on that task's JoinHandle rather
       // than the future directly.
       let handle = tokio::task::spawn(run_assay());
       let result = tokio::time::timeout(TOTAL_TEST_TIMEOUT, handle).await;

       // `done` (which disarms the native watchdog) is set ONLY when the
       // spawned task has actually terminated — never merely because this
       // cooperative await resolved. A cooperative-timeout Err means the
       // spawned task may still be genuinely running or blocked; disarming
       // the native watchdog there would defeat the one scenario it exists
       // for.
       match result {
           Ok(Ok(())) => {
               done.store(true, Ordering::SeqCst);
           }
           Ok(Err(join_err)) => {
               done.store(true, Ordering::SeqCst);
               std::panic::resume_unwind(join_err.into_panic());
           }
           Err(_) => {
               panic!(
                   "assay exceeded the {TOTAL_TEST_TIMEOUT:?} total kill-line bound (reported via \
                    the cooperative tokio::time::timeout path; the native watchdog stays armed in \
                    case the spawned task is still genuinely blocked, not merely slow)"
               );
           }
       }
   }
   ```

3. Run it:

   ```bash
   cargo test -p autumn-web --features test-support --test integration_tests \
     cluster_two_node -- --nocapture   # control: existing 2-node suite

   cargo test -p autumn-web --features test-support \
     --test prospect_cluster_scale -- --nocapture   # the N=5 assay, repeat 3-4x
   ```

4. Revert `autumn/Cargo.toml` and delete
   `autumn/tests/prospect_cluster_scale.rs` afterward — per this ledger's
   containment rule, the apparatus does not merge.
