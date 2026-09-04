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

## 🧪 Apparatus

One throwaway test file, `autumn/tests/prospect_cluster_scale.rs` (a
temporary `[[test]]` entry added to `autumn/Cargo.toml` to build it,
`test-support` feature enabled), copying `cluster_two_node.rs`'s own
harness conventions (`ClusterConfig`, `install_from_config`, `ClusterHandle`,
a `poll_until` helper, a two-sided `describe()` failure formatter) and
scaling them to 5 members. One `#[tokio::test(flavor = "multi_thread")]`
runs all four pre-registered steps in sequence against one 5-node cluster:

1. `spawn_star_cluster(5)` — node 0 installs with no seeds; nodes 1-4 each
   install seeded only with node 0's observed `local_addr()` (star
   topology, matching the pre-registration). Every node binds
   `127.0.0.1:0`; `push_interval_ms: 200`, `suspicion_timeout_ms: 1_000`,
   identical to `cluster_two_node.rs`'s own `cluster_config()`.
2. Poll for all 5 nodes reporting an identical, full 5-member view.
3. Every node increments the shared counter by a distinct amount
   (node *i* → `i+1`); poll for all 5 nodes reading the exact sum, 15.
4. Cancel node 4's shutdown token (clean departure); poll for the 4
   survivors converging to an identical 4-member view.
5. Install a fresh node, re-seeded from node 0, replacing node 4; poll for
   all 5 (4 original + 1 rejoined) reconverging to a full 5-member view.

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

**N=5 assay, 4 runs** (`cargo test -p autumn-web --features test-support
--test prospect_cluster_scale`, run back-to-back in this sandbox — repeated
beyond the pre-registration's implicit single run because a single pass is
weak evidence for "pursue" on its own, matching the lesson the prior
cold-start assays in this ledger paid dearly to learn):

| Run | Result | Wall-clock |
|---|---|---:|
| 1 | all 4 steps pass | 1.16s |
| 2 | all 4 steps pass | 1.27s |
| 3 | all 4 steps pass | 1.32s |
| 4 | all 4 steps pass | 1.03s |

**Against the pre-registered lines:**

- **Step 1 (cold-start convergence, line: ≤10s):** passed on 4/4 runs.
  Actual convergence is folded into each run's total ~1-1.3s wall-clock
  (compile excluded) — roughly an order of magnitude inside the bound, not
  a close call the way the cold-start ledger's margins were.
- **Step 2 (counter merge, line: exact sum 15 on every node, ≤10s):**
  passed on 4/4 runs. Every node read exactly 15 on every run — no lost or
  duplicated increments observed.
- **Step 3 (departure, line: 4-member view on survivors, ≤5s):** passed on
  4/4 runs.
- **Step 4 (rejoin, line: 5-member view, ≤10s):** passed on 4/4 runs.
- **Kill-line check:** zero divergent views, zero wrong counter values,
  zero panics/timeouts/hangs across all 4 runs. The "2 of 3 repeats"
  kill-line threshold was never approached in either direction — this
  result is not a marginal call the way the cold-start bisection's
  sub-5,000ms deltas were; every margin here has roughly 8-13x headroom
  against its line, so ordinary run-to-run scheduling noise on this shared
  sandbox is not a plausible alternative explanation the way it was there.

## 🏁 Verdict

**Pursue**, against the pre-registered line: all four success criteria
(cold-start convergence, exact counter merge, departure convergence,
rejoin convergence) held on every one of 4 runs, each with roughly an
order of magnitude of margin against its bound — not a photo finish. No
kill-line condition was observed. Within the scope this assay actually
tested (single host, single process, star topology, honest peers, N=5,
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

```bash
# 1. Add the temporary [[test]] entry to autumn/Cargo.toml:
#      [[test]]
#      name = "prospect_cluster_scale"
#      path = "tests/prospect_cluster_scale.rs"
# 2. Restore autumn/tests/prospect_cluster_scale.rs from this report's PR
#    diff (reverted before the final commit; see git history on the commit
#    that pre-registered this assay for the version right before revert,
#    or the PR's own diff for this report).

cargo test -p autumn-web --features test-support --test integration_tests \
  cluster_two_node -- --nocapture   # control: existing 2-node suite

cargo test -p autumn-web --features test-support \
  --test prospect_cluster_scale -- --nocapture   # the N=5 assay, repeat 3-4x
```
