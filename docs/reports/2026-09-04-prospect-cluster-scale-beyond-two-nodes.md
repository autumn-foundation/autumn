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

## 🧪 Apparatus

One throwaway test file, `autumn/tests/prospect_cluster_scale.rs` (a
temporary `[[test]]` entry added to `autumn/Cargo.toml` to build it,
`test-support` feature enabled), copying `cluster_two_node.rs`'s own
harness conventions (`ClusterConfig`, `install_from_config`, `ClusterHandle`,
a two-sided `describe()` failure formatter) and scaling them to 5 members.
Every check is **debounced**: `poll_until_stable` requires the condition to
hold on 3 consecutive observations 50ms apart before counting as converged
(see the third correction above), not a single-instant read, and yields
`STABLE_GAP` after every observation — success or failure — so a failing
streak cannot spin-retry without a genuine `.await` point (fourth
correction above). Step 3's checks use the pre-registered 5s
`DEPARTURE_TIMEOUT`; every other step uses the 10s `CONVERGE_TIMEOUT`
(fifth correction above — the debounce refactor had briefly hardcoded 10s
everywhere). One `#[tokio::test(flavor = "multi_thread")]` runs all steps
in sequence against one 5-node cluster:

1. `spawn_star_cluster(5)` — node 0 installs with no seeds; nodes 1-4 each
   install seeded only with node 0's observed `local_addr()` (star
   topology, matching the pre-registration). Every node binds
   `127.0.0.1:0`; `push_interval_ms: 200`, `suspicion_timeout_ms: 1_000`,
   identical to `cluster_two_node.rs`'s own `cluster_config()`.
2. `assert_stable_membership` (10s bound): all 5 nodes report an identical,
   full 5-member view, stably.
3. Every node increments the shared counter by a distinct amount
   (node *i* → `i+1`); `assert_stable_counter_sum` (10s bound): all 5 nodes
   read the exact sum, 15, stably — then `assert_stable_membership` again,
   to catch any membership flicker that happened *during* counter
   convergence.
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

**Against the pre-registered lines (fourth pass, the one whose code
actually enforces every bound as registered — see note on Step 3 below for
why the third pass's runs still count as evidence despite the bug in that
pass's code):**

- **Step 1 (cold-start convergence, line: ≤10s):** passed on 16/16 runs
  across all four passes. Actual convergence is folded into each run's
  total ~0.9-2.7s wall-clock (compile excluded) — the third and fourth
  passes run slower than the first two purely because of the added
  debounce delay (3×50ms per check × more checks per run, plus the
  revision-4 unconditional post-check yield), not because convergence
  itself got slower; still roughly 4-10x inside the bound, not a close
  call the way the cold-start ledger's margins were.
- **Step 2 (counter merge, line: exact sum 15 on every node, ≤10s):**
  passed on 16/16 runs, stability-checked since run 9, with a membership
  recheck immediately after confirming no flicker occurred during
  convergence.
- **Step 3 (departure, line: 4-member view on survivors, ≤5s, AND exact sum
  15 still held on all 4 survivors):** passed on 16/16 runs — the counter
  half ran in runs 5-16, held on 12/12 of those; the stability debounce and
  post-counter membership recheck ran in runs 9-16, held on 8/8. **Caveat
  on runs 9-12 specifically:** those ran before the fifth correction, so
  their code was actually bounded by the 10s `CONVERGE_TIMEOUT`, not the
  registered 5s `DEPARTURE_TIMEOUT` — the *test* didn't enforce the right
  line, even though it still passed. This is not silently swept in as
  clean evidence for the 5s bound: it's flagged here, and only runs 13-16
  (fourth pass) are code-verified to have actually been gated at 5s. All
  16 runs, including 9-12, did *empirically* complete in 2.3-2.6s either
  way — comfortably under 5s regardless of which timeout the code was
  configured to allow — so the wall-clock evidence itself still supports
  the line; only the earlier code's enforcement of that specific line was
  what was broken, not the measured outcome.
- **Step 4 (rejoin, line: 5-member view, ≤10s, AND exact sum 15 relearned
  by the rejoined node):** passed on 16/16 runs — counter half held on
  12/12 (runs 5-16), stability + post-counter recheck held on 8/8 (runs
  9-16), all correctly bounded at 10s throughout (this step was never
  affected by the Step 3 timeout bug).
- **Kill-line check:** zero divergent views, zero wrong counter values,
  zero panics/timeouts/hangs, zero stability-debounce failures (no run
  ever needed a second debounce window — every 3-observation streak
  succeeded on the first attempt) across all 16 runs. The "2 of 3 repeats"
  kill-line threshold was never approached in either direction — this
  result is not a marginal call the way the cold-start bisection's
  sub-5,000ms deltas were; every margin here has roughly 2-13x headroom
  against its line (narrowest: step 3's ~2.3-2.6s actual vs. its 5s line,
  once correctly enforced), so ordinary run-to-run scheduling noise on
  this shared sandbox is not a plausible alternative explanation the way
  it was there.

## 🏁 Verdict

**Pursue**, against the pre-registered line: all four success criteria
(cold-start convergence, exact counter merge, departure convergence,
rejoin convergence) held on every run across all four passes, with
comfortable margin against every bound — not a photo finish, and (per the
Step 3 caveat in **Assay**) the narrowest margin, departure's 5s line, is
only claimed as *code-enforced* for the fourth pass's 4 runs, though the
wall-clock evidence supports it across all 16. No kill-line condition was
observed. After all four corrections above, the counter's exact sum was
verified not just once before churn but again on the survivors after
departure and again on the full cluster after rejoin; every
convergence/counter observation was required to hold across 3 consecutive
checks (not a single instant) with a membership recheck immediately after
every counter check; the debounce itself was fixed to always yield rather
than risk spinning; and step 3's checks are now actually gated at the
registered 5s, not a silently-inherited 10s — the actual claim the guide
text makes ("no divergence," stable identical views, exact sums through
churn) is the one actually measured, on 4/4 fourth-pass runs, not the
progressively weaker versions the first three passes of this report each
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
4, post all four Codex corrections above) is embedded below rather than
referenced by history. This is now the durable artifact.

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
   //! Revision 2 (Codex P2 on PR #2503): the counter sum is now re-verified
   //! after departure (step 3) and after rejoin (step 4), not just after step
   //! 2.
   //!
   //! Revision 3 (Codex P2 on PR #2503, on the revision-2 diff): every
   //! convergence/counter check now requires the condition to hold across
   //! several *consecutive* observations (a debounce), not just once. A
   //! membership recheck now also runs immediately after every counter check.
   //!
   //! Revision 4 (two more Codex P2 findings on the revision-3 diff):
   //! - `poll_until_stable` retried immediately on a failed check with no
   //!   sleep/yield, which on a single-worker runtime could spin-monopolize
   //!   the only executor thread and starve the gossip/timer tasks the
   //!   condition depends on, producing a false timeout on a healthy cluster.
   //!   Fixed: yield `STABLE_GAP` after every check, success or failure.
   //! - The revision-3 refactor consolidated all membership/counter checks
   //!   into helpers that hardcoded `CONVERGE_TIMEOUT` (10s), silently losing
   //!   the pre-registered 5s `DEPARTURE_TIMEOUT` for step 3's checks (both
   //!   membership and the post-departure counter check). Fixed: both helpers
   //!   now take an explicit `timeout` parameter, and step 3 passes
   //!   `DEPARTURE_TIMEOUT`.

   use std::sync::Arc;
   use std::time::Duration;

   use autumn_web::cluster::{ClusterHandle, install_from_config};
   use autumn_web::config::{AutumnConfig, ClusterConfig};
   use autumn_web::test::TestApp;
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
   /// Consecutive positive observations required before a condition counts as
   /// "stable," and the gap between them. Adds ~150ms per check on the happy
   /// path — negligible against the multi-second timeouts above.
   const STABLE_CHECKS: u32 = 3;
   const STABLE_GAP: Duration = Duration::from_millis(50);

   /// Debounced poll: `condition` must return `true` on `STABLE_CHECKS`
   /// consecutive observations, `STABLE_GAP` apart, before this returns
   /// success. Any single `false` observation resets the streak. Always
   /// yields `STABLE_GAP` after each observation, success or failure — the
   /// condition futures here are synchronous `ClusterHandle` reads that
   /// complete instantly, so without an unconditional yield, a failed streak
   /// would spin-retry with no `.await` point, able to monopolize a
   /// single-worker Tokio runtime and starve the gossip/timer tasks the
   /// condition is waiting on (Codex P2 on PR #2503, revision 4).
   async fn poll_until_stable<F, Fut>(timeout: Duration, mut condition: F) -> bool
   where
       F: FnMut() -> Fut,
       Fut: std::future::Future<Output = bool>,
   {
       let deadline = tokio::time::Instant::now() + timeout;
       loop {
           let mut stable = true;
           for _ in 0..STABLE_CHECKS {
               let ok = condition().await;
               tokio::time::sleep(STABLE_GAP).await;
               if !ok {
                   stable = false;
                   break;
               }
           }
           if stable {
               return true;
           }
           if tokio::time::Instant::now() >= deadline {
               return false;
           }
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

   fn member_ids(handle: &Arc<ClusterHandle>) -> Vec<String> {
       let mut ids: Vec<String> = handle.members().into_iter().map(|m| m.id).collect();
       ids.sort();
       ids
   }

   fn describe(handles: &[Arc<ClusterHandle>]) -> String {
       handles
           .iter()
           .enumerate()
           .map(|(i, h)| {
               format!(
                   "node{i}(id={}, members={:?}, {COUNTER}={})",
                   h.node_id(),
                   member_ids(h),
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

   /// Stable, two-sided membership check: every handle must report exactly
   /// `expected_n` members with an identical sorted id set, on
   /// `STABLE_CHECKS` consecutive observations, within `timeout`.
   async fn assert_stable_membership(
       handles: &[Arc<ClusterHandle>],
       expected_n: usize,
       timeout: Duration,
       label: &str,
   ) {
       let hs = handles.to_vec();
       let ok = poll_until_stable(timeout, || {
           let hs = hs.clone();
           async move {
               if !hs.iter().all(|h| h.members().len() == expected_n) {
                   return false;
               }
               let reference = member_ids(&hs[0]);
               hs.iter().all(|h| member_ids(h) == reference)
           }
       })
       .await;
       assert!(
           ok,
           "[{label}] all {expected_n} nodes must report an identical {expected_n}-member view, \
            stable across {STABLE_CHECKS} consecutive observations {STABLE_GAP:?} apart, within \
            {timeout:?}; {}",
           describe(handles)
       );
   }

   /// Stable, two-sided counter check: every handle must read exactly
   /// `EXPECTED_SUM`, on `STABLE_CHECKS` consecutive observations, within
   /// `timeout`.
   async fn assert_stable_counter_sum(handles: &[Arc<ClusterHandle>], timeout: Duration, label: &str) {
       let hs = handles.to_vec();
       let ok = poll_until_stable(timeout, || {
           let hs = hs.clone();
           async move { hs.iter().all(|h| h.counter(COUNTER).get() == EXPECTED_SUM) }
       })
       .await;
       assert!(
           ok,
           "[{label}] every node must read counter={EXPECTED_SUM}, stable across {STABLE_CHECKS} \
            consecutive observations {STABLE_GAP:?} apart, within {timeout:?}; {}",
           describe(handles)
       );
   }

   #[tokio::test(flavor = "multi_thread")]
   async fn n5_star_cluster_converges_counters_and_survives_departure_and_rejoin() {
       // --- Step 1: cold-start convergence to a full N-member view ---
       let (mut handles, mut tokens) = spawn_star_cluster(N);
       assert_stable_membership(&handles, N, CONVERGE_TIMEOUT, "step1-cold-start").await;

       // --- Step 2: concurrent counter increments from every node ---
       for (i, h) in handles.iter().enumerate() {
           h.counter(COUNTER).increment_by((i as u64) + 1);
       }
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
