# ⛏️ Prospect: does the gossip cluster converge correctly at N=5? (pre-registered, not yet run)

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

## 🧪 Apparatus

*(filled in after the pre-registration above is committed, per the gate —
not before)*
