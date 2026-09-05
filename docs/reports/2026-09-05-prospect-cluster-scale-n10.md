# ⛏️ Prospect: does the gossip cluster still converge correctly at N=10? (pursue: 5/5 clean, 13.1-13.4s, well inside every N=5-derived bound)

## 🎯 Question

`docs/reports/2026-09-04-prospect-cluster-scale-beyond-two-nodes.md` (PR
#2503, merged) ran a 5-node convergence assay and updated
`docs/guide/clustering.md` to say the design has "correctness evidence at
five" but explicitly refused to extrapolate further: *"The assay never left
one process, one host, or loopback networking; it tested only N=5, one churn
cycle, and honest peers — it says nothing about real multi-host latency,
packet loss or partition, N>5, or the all-to-all message volume's O(N²)
growth. Treat larger fleets... as still future work, not as a supported
configuration, until a follow-up assay closes those gaps."* This is that
follow-up, scoped to the one sub-gap that is cheap to test on one host: N>5.

**Falsifiable question:** with a 10-node cluster (double the previously-
verified N), star-seeded exactly as the N=5 assay was (nodes 1-9 each seed
only from node 0's address), using the *identical*
`push_interval_ms`/`suspicion_timeout_ms` values the N=5 assay used
(200ms/1000ms — unchanged, not scaled up for the larger N), does the current
implementation still (a) converge every node to an identical 10-member view
from cold start, (b) merge concurrent counter increments issued from all 10
nodes into the exact correct sum, and (c) converge the 9 survivors to a
correct view after one node's clean departure, and back to 10 after that
node rejoins with the same identity — all inside the **same wall-clock
bounds the N=5 assay used** (10s cold-start/counter/rejoin, 5s departure) —
or does doubling N degrade any of those, either through outright divergence
or through the timeouts themselves creeping closer to their bounds as the
O(N²) push volume (100 total pushes/interval at N=10 vs. 25 at N=5 — the
number of directed peer-to-peer pushes per interval, unrelated to the number
of distinct sockets: gossip is full-mesh push, not a ring) starts to bite?

**Decision:** whether `docs/guide/clustering.md`'s "N>5... future work" hedge
can be narrowed to name N=10 as also evidenced under the same bounds (if the
assay passes), or whether a concrete point where correctness or margin
starts degrading should be documented — and possibly a correctness issue
filed — before N=10 is treated as informally safe either way (if it fails
or degrades). **Decider:** repo maintainer (owns both the cluster module
and the clustering guide's claims — the same, only named owner, as the
parent N=5 assay).

**Who is waiting on this, and what changes:** same as the parent assay —
nobody has an active fleet deployment blocked on this today; this is a
documentation-accuracy and early-warning question, not a shipped-feature
blocker. What changes on pursue: the guide can name a second verified point
on the N axis instead of a bare "future work" hedge for anything past 5.
What changes on kill: a concrete degradation point is on record before
anyone reaches for N=10 in practice and finds out the hard way.

## 🔍 Prior art

- `docs/reports/2026-09-04-prospect-cluster-scale-beyond-two-nodes.md` — the
  parent assay this follows up on; explicitly names N>5 as an untested gap
  and its own guide edit says so in `docs/guide/clustering.md` (lines
  783-797, read directly before writing this pre-registration).
- `grep -rn "O(N" docs/ autumn/src/cluster/` and a search of every
  `docs/reports/*prospect*` file — no other assay or report touches cluster
  scale past N=5. This specific pit (N=10) has not been dug before.
- `autumn/tests/integration/cluster_two_node.rs` and
  `autumn/src/cluster/tests.rs` — still only N=2 (plus the one N=3
  wrong-secret auth test, unrelated to scale) in the committed test suite;
  the N=5 assay's own throwaway apparatus was never committed (by design,
  per its own containment rule).
- Read `autumn/src/cluster/wire.rs` directly:
  `MAX_FRAME_BYTES = 65_536`. A 10-member document (10 rows of
  `{id, addr, status, incarnation}` plus up to 10 counter cells) is on the
  order of 1-2KB serialized — orders of magnitude under the frame cap, so
  frame-size rejection (`autumn_cluster_pushes_unsendable_total`) is not a
  plausible failure mode at N=10 and is not instrumented in this assay (see
  Apparatus stubs). The O(N²) risk this assay actually tests is push/task
  *volume* (more concurrent gossip work per interval), not frame size.

## ⚖️ Pre-registration

Committed before any node is started or any assertion is written.

- **Success line (pursue — narrow the guide further):** in a single test
  process, 10 `ClusterHandle`s (star-seeded from node 0,
  `push_interval_ms: 200`, `suspicion_timeout_ms: 1_000` — unchanged from
  both the N=2 and N=5 tests, deliberately not scaled up for the larger N,
  since the guide's own hedge is about whether the *existing* protocol
  config degrades at larger N, not about whether a re-tuned one could cope)
  must, within the **same bounds the N=5 assay used**:
  1. Converge to a 10-member view on **every** node within **10s**
     (`handle.members().len() == 10` on all ten, all ten sorted
     `(id, addr, incarnation)` triples identical — `status` excluded, as in
     the N=5 assay, because the guide documents it as a local,
     never-replicated overlay).
  2. After each of the 10 nodes issues a distinct, genuinely concurrent
     counter increment (node *i* adds `i+1`, total = 1+2+...+10 = 55) and a
     further **10s** convergence window, every node's `counter.get()` must
     equal exactly 55.
  3. After cancelling node 9's shutdown token (clean leave), the remaining 9
     must converge to an identical 9-member view, with the counter still
     summing to 55, within **5s**.
  4. After starting a fresh node re-seeded from node 0 and reusing node 9's
     exact id (a genuine same-identity rejoin), the cluster must reconverge
     to a 10-member view with the counter still summing to 55, within
     **10s**.
  All four hold, on every run ⇒ **pursue**: the guide can name N=10 as a
  second verified point.
- **Kill line:** any one of:
  - Member views still disagree across the 10 nodes 3x past the relevant
    bound above (30s / 15s) — genuine divergence, not slow convergence.
  - The converged counter value on any node is not exactly 55 once every
    node reports a stable 10-member view for 2 consecutive polls.
  - A panic, deadlock, or hang in cluster code under N=10 (the whole test
    times out past 90s total — widened from the N=5 assay's 60s only
    because 10 nodes' one-time TCP handshake set-up is a larger constant
    cost than 5's was, not because any per-step bound above changed).
  - Any of the above reproduces on **2 of 3** repeated runs (the same
    threshold the N=5 assay used, ruling out a one-off scheduler fluke on
    this shared sandbox before calling it a real bug).
  If the top-level convergence/counter/departure/rejoin behavior holds but
  only the specific margins above are missed narrowly (e.g. converges in
  11s, not 10s) — report as **undetermined-on-the-line, qualitatively
  pursue**, not a silent pass, exactly as the N=5 assay's own third outcome
  was defined.
- **Conditions:** this sandbox only (4-core, 15GiB, rustc/cargo 1.94.1 —
  confirmed the same environment class as the N=5 and cold-start assays),
  single process, all 10 members as separate `TestApp`/`ClusterHandle`
  instances bound to `127.0.0.1:0`, `test-support` feature enabled. Star
  topology: nodes 1-9 seed only from node 0's observed `local_addr()` —
  matching both the guide's minimal-seed-list description and the N=5
  assay's own choice, so this is a like-for-like doubling of N and nothing
  else.
- **Time box:** ≤45 minutes cumulative wall-clock this session (matching
  the N=5 assay's original box). If apparatus construction or a single run
  exceeds 15 minutes, stop and report undetermined with whatever partial
  evidence exists.
- **Riskiest assumption tested first:** that full-broadcast/no-quorum
  gossip *correctness* (not speed, not efficiency) survives a doubling of N
  — the specific, named gap in the guide's own hedge — rather than starting
  to show divergence or lost updates as the O(N²) push volume grows.
  Performance/throughput/bandwidth measurement is explicitly out of scope,
  same as the parent assay.
- **Control:** the N=5 assay's own 60 clean runs (8-13x margin against every
  line) is the baseline this assay scales up from — same protocol config,
  same style of two-sided, snapshot-consistent convergence assertion, same
  real-socket `TestApp` + `install_from_config` + `ClusterHandle` harness,
  just N=10 instead of N=5. The existing N=2 suite
  (`cluster_two_node.rs`) is re-run first, unmodified, to confirm this
  sandbox's baseline is healthy before trusting any N=10 result on it —
  same discipline the N=5 assay used.
- **Containment:** apparatus is one new, throwaway test file
  (`autumn/tests/prospect_cluster_scale_n10.rs`) plus a temporary
  `[[test]]` entry in `autumn/Cargo.toml`, both added on this branch, run
  only via local `cargo test` in this sandbox — never pushed to CI, never
  added to `tests/integration/mod.rs`'s consolidated binary. Both are
  reverted before this report's final commit, with the apparatus's full
  source embedded verbatim in **Reproduce** (the N=5 assay's own
  post-merge correction found that "restore the file from git history"
  is not reproducible when the file was never committed — this report
  avoids that mistake from the start). No production data, no external
  network, no spend.

## 🧪 Apparatus

One throwaway test file, `autumn/tests/prospect_cluster_scale_n10.rs`,
copying the N=5 assay's own (heavily corrected) design directly rather than
re-deriving it — every correction that assay's Codex reviews forced is
baked in from the start here:

- **Snapshot-consistent membership checks.** Each node's `members()` is read
  exactly once per observation; both the cardinality check and the
  `(id, addr, incarnation)` identity comparison are derived from that same
  snapshot, so a live transition between two separate reads can't be
  accepted as one stable state (the N=5 assay's twenty-second correction).
  `status` is deliberately excluded from the comparison — the guide
  documents it as a local, never-replicated overlay that can legitimately
  differ transiently between healthy nodes.
- **Debounced convergence**, not single-instant reads: `poll_until_stable`
  requires a condition to hold on 6 consecutive observations 250ms apart
  (last read at t≈1250ms, strictly past `suspicion_timeout_ms`=1000ms),
  classified against both the soft `timeout` line and the hard `timeout×3`
  divergence line *before* any trailing sleep (the N=5 assay's fourteenth
  correction), with an in-progress positive streak that began before the
  hard deadline allowed to finish rather than being killed mid-stream (the
  N=5 assay's twentieth/twenty-third corrections).
- **Genuinely concurrent increments**: all `N` nodes increment on their own
  spawned task, rendezvousing on a `tokio::sync::Barrier` first — not a
  sequential loop (the N=5 assay's thirteenth correction).
- **Genuine same-identity rejoin**: every node gets an explicit, stable
  `node_id` (`"node-0"`..`"node-9"`), and the rejoined node reuses the
  departed node's exact id, asserted directly (the N=5 assay's eighteenth
  correction).
- **Whole-test kill line** enforced by running the assay body on its own
  spawned Tokio task and wrapping the await on its `JoinHandle` (not the
  future directly) in a 90s `tokio::time::timeout` — 90s rather than the
  N=5 assay's 60s only because 10 nodes' one-time TCP handshake setup is a
  larger constant cost than 5's, not because any per-step bound changed.

**One deliberate simplification versus the N=5 assay's final (heavily
corrected) form, disclosed rather than silently adopted:** this apparatus
does **not** add the N=5 assay's second, independent native-`std::thread`
watchdog (added there specifically to survive a deadlock that starves every
Tokio worker thread, which the spawned-task watchdog alone cannot catch).
Given this session's time box and that the N=5 assay's own 60 runs across
fifteen passes never once needed that second watchdog to fire, this was
judged an acceptable, disclosed simplification for a single-pass assay
built with the lessons already known up front — not a corner cut silently.
If a future assay on this cluster module needs the same paranoia level the
N=5 report eventually reached, add it back.

**Stubs / what this apparatus faked or skipped** (identical in kind to the
N=5 assay's own list, scaled to N=10 — scopes what the result below actually
proves):

- **Single process, single host, loopback only.** No real network, no
  cross-host latency, no packet loss, no reordering. Tests the gossip
  *protocol's* correctness under ideal networking, not resilience to real
  network pathology.
- **Star topology only**, matching the N=5 assay's own choice and the
  guide's minimal-seed-list description. A full seed list or a ring were
  not tested.
- **One departure, one rejoin, one cycle.** No repeated churn, no
  concurrent multi-node departures, no restart-with-different-address case.
- **No adversarial input.** No malicious peer, no clock skew, no truncated
  or corrupted frame.
- **Library API only, not the HTTP layer.** Talks to `ClusterHandle`
  directly, not through `/actuator/health` — the N=5 and N=2 suites already
  cover that thin, read-only projection.
- **N=10 only.** No claim is made about N=15, N=20, or any other fleet
  size; extrapolating the clean N=5→N=10 trend further is exactly the
  inadmissible move this role's evidence rules forbid.
- **No frame-size or bandwidth instrumentation.** `MAX_FRAME_BYTES` (64KB,
  confirmed by reading `autumn/src/cluster/wire.rs`) is orders of magnitude
  above what a 10-member document serializes to, so
  `autumn_cluster_pushes_unsendable_total` was judged not worth wiring up
  for this N — it would almost certainly read zero regardless of whether
  the design is actually healthy at this scale, telling this assay nothing.
  This apparatus tests correctness under O(N²) *task/connection* volume
  growth, not frame-size growth; a future assay aimed specifically at the
  guide's O(N²) bandwidth hedge would need a much larger N (or direct
  bandwidth measurement) to say anything about framing.
- **No performance/throughput measurement.** Same as the N=5 assay: this
  tests whether the design stays *correct* at N=10, not whether it stays
  *fast* — wall-clock is reported below for transparency, but no line was
  pre-registered against it beyond the per-step timeout bounds themselves.

## 📊 Assay

**Control, run first:** the existing 2-node suite
(`cargo test -p autumn-web --features test-support --test integration_tests
cluster_two_node`), to confirm the sandbox's baseline is healthy before
trusting any N=10 result on it — same discipline the N=5 assay used. All 8
existing tests passed, 0.70s test time (9m43s one-time compile of the full
consolidated binary — longer than the N=5 assay's 7m39s, plausibly because
`test-support` on this run also pulled in `loom`/`trybuild`/`proptest` that
weren't already warm in this fresh sandbox; not part of the measured
behavior either way):

```
test integration::cluster_two_node::disabled_cluster_installs_nothing ... ok
test integration::cluster_two_node::install_refuses_a_second_node_on_one_state ... ok
test integration::cluster_two_node::install_refuses_a_collision_on_the_membership_component_name ... ok
test integration::cluster_two_node::install_rejects_an_invalid_or_secretless_section ... ok
test integration::cluster_two_node::full_app_two_nodes_health_and_counter_via_http ... ok
test integration::cluster_two_node::tcp_survivor_converges_after_peer_cancelled ... ok
test integration::cluster_two_node::tcp_two_nodes_converge_and_counter_replicates ... ok
test integration::cluster_two_node::tcp_clean_leave_converges_before_the_suspicion_timeout ... ok
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 1925 filtered out
```

**N=10 assay, 5 runs** (`cargo test -p autumn-web --features test-support
--test prospect_cluster_scale_n10 -- --nocapture`; a compile-check pass
first caught two apparatus bugs before any run — see the note below the
table — so these 5 runs are all against the corrected apparatus embedded in
**Reproduce**):

| Run | Result | Wall-clock (test-reported / process) |
|---|---|---:|
| 1 | all 9 checkpoints `Converged`, no `LateConverged`, no `Diverged` | 13.10s / 14.96s |
| 2 | all 9 checkpoints `Converged`, no `LateConverged`, no `Diverged` | 13.36s / 13.78s |
| 3 | all 9 checkpoints `Converged`, no `LateConverged`, no `Diverged` | 13.34s / 13.74s |
| 4 | all 9 checkpoints `Converged`, no `LateConverged`, no `Diverged` | 13.10s / 13.57s |
| 5 | all 9 checkpoints `Converged`, no `LateConverged`, no `Diverged` | 13.10s / 13.56s |

(Run 1's higher process time — 14.96s vs. the test's own 13.10s — is the
`cargo test` binary's own compile-check/startup overhead on the first
invocation after editing the source; runs 2-5 reuse the already-built
binary, and the tiny 0.3-0.4s "Finished" compile line each still shows
comes from Cargo's own change-detection, not a rebuild.)

**Two apparatus bugs, found by the compiler before any run — not corrections
after a wrong result, since no run had happened yet:**

1. **First compile attempt:** `tokio::spawn(run_assay())` failed because
   `Node` originally held a `TestApp` field, and every assertion helper
   takes `&[Node]` across an `.await` inside that spawned (Send-bound)
   future — a shared reference is `Send` only if its referent is `Sync`,
   and `TestApp`'s `HashMap<TypeId, Box<dyn Any + Send>>` extensions map is
   `Send` but not `Sync`. Fixed by removing `TestApp`/`TestClient` from
   `Node` entirely and returning it as a separately-owned, never-referenced
   `Vec<TestClient>` from `spawn_star_cluster`/`install_node` instead — see
   the `Node` doc comment in **Reproduce** for the full reasoning.
2. **Second compile attempt:** `TestApp::new().config(...).build()` returns
   `TestClient`, not `TestApp` (confirmed by reading `autumn/src/test.rs`
   directly: `TestApp` is the builder, `.build()` consumes it into
   `TestClient`) — and `AppState::extension::<T>()` returns `Arc<T>`, not
   `T` (confirmed the same way in `autumn/src/state.rs`), unlike the
   `AppState`-inherent `extension` at `autumn/src/app.rs:1760`. Fixed the
   type annotations accordingly; no logic changed.

Neither bug ever produced a passing or failing test run — both were compile
errors, caught before the first execution. Reported here for the same
reason the N=5 assay's own report logs every correction: transparency about
what the apparatus actually is, not just its final result.

**Against the pre-registered lines:**

- **Step 1 (cold-start convergence, line: ≤10s):** `Converged` (not
  `LateConverged`) on 5/5 runs.
- **Step 2 (counter merge, line: exact sum 55 on every node, ≤10s):**
  `Converged` on 5/5 runs, with the post-counter membership recheck also
  clean on all 5.
- **Step 3 (departure, line: 9-member view on survivors + exact sum 55,
  ≤5s):** `Converged` on 5/5 runs, both the membership and counter halves,
  plus the post-counter membership recheck.
- **Step 4 (rejoin, line: 10-member view + exact sum 55 relearned by the
  rejoined node, ≤10s):** `Converged` on 5/5 runs, including the direct
  `rejoined.handle.node_id() == departed_id` assertion (a genuine
  same-identity rejoin, not a new member).
- **`LateConverged` classification:** never triggered in any of the 5 runs
  — every check reached `Converged` well inside its soft `timeout`, same
  as every one of the N=5 assay's 60 runs. The apparatus has the capacity
  to report it, but this data never approaches that boundary.
- **Whole-test 90s kill line:** never approached — every run completed in
  13.1-15.0s total, ~6-7x inside the bound.
- **Kill-line check:** zero divergent views, zero wrong counter values,
  zero panics/timeouts/hangs, zero `Diverged` outcomes across all 5 runs.
  The "2 of 3 repeats" kill-line threshold was never approached in either
  direction.
- **Wall-clock comparison against the N=5 control:** the N=5 assay's own
  fully-corrected (fifteenth-pass) runs landed at 13.08-13.34s; this N=10
  assay's 5 runs land at 13.10-13.36s (test-reported) — statistically
  indistinguishable from the N=5 numbers despite double the membership,
  double the per-interval push count, and roughly 4x the total directed
  push volume (100 vs. 25 pushes/interval). The debounce window's own fixed
  cost (9 mandatory ≥1.25s stability checks across the four steps) fully
  dominates both assays' wall-clock; neither shows any measurable
  convergence slowdown from N itself at this scale.

## 🏁 Verdict

**Pursue.** All four pre-registered success-line checks held on every one
of 5 runs, with zero margin erosion versus the N=5 control (same
~13.1-13.4s band, not a widening one) and zero `LateConverged`/`Diverged`
outcomes. Doubling N from 5 to 10 — quadrupling the per-interval directed
push volume — produced no detectable degradation in this apparatus's
correctness or timing under ideal single-host, single-churn-cycle,
honest-peer conditions.

This is **not** evidence the trend continues past N=10, and this report
does not claim it does — extrapolating "doubled cleanly once, so it'll
double cleanly again" is exactly the inadmissible move this role's evidence
rules forbid. What it *does* license: `docs/guide/clustering.md`'s hedge
can honestly name a second, higher verified point (done, in the same
commit as this report) rather than leaving "N>5" as a bare, single-data-point
future-work marker. The guide's larger, still-untested gaps — real
multi-host latency, packet loss/partition, and O(N²) bandwidth growth at a
scale where it would actually bite — are unchanged by this assay and remain
explicitly named as future work in the updated guide text.

**Decision, resolved:** per this report's own pre-registration, the repo
maintainer's guide-narrowing option is exercised now (the same edit that
produced this report also updates `docs/guide/clustering.md`) rather than
left as a follow-up ask — there is no ambiguity to escalate on a clean 5/5
pursue with a fully-explained, non-degrading margin. No correctness issue
is filed, since the kill line was never approached.

## 💰 Cost to productionize

N/A in the usual sense — this assay's "pursue" is a documentation-narrowing
outcome, not a build. The stubs list above is nonetheless the honest scope
of what remains before N=10 (or any N) could be called production-ready:
real multi-host networking (not loopback), partition/packet-loss handling,
repeated and concurrent churn, adversarial peers, and O(N²) bandwidth
measurement at a scale where the 64KB frame cap or raw throughput could
plausibly matter (this assay's own reading of `MAX_FRAME_BYTES` suggests
that scale is well past N=10). Any of those would be its own separately
pre-registered assay, per this role's charter — none is chartered here.

## 🔬 Reproduce

```bash
# Temporary [[test]] entry added to autumn/Cargo.toml (reverted after this
# assay; re-add before running):
#
# [[test]]
# name = "prospect_cluster_scale_n10"
# path = "tests/prospect_cluster_scale_n10.rs"

cat > autumn/tests/prospect_cluster_scale_n10.rs <<'RUST_EOF'
//! Throwaway Prospect apparatus: does the gossip cluster still converge
//! correctly at N=10 (double the previously-verified N=5)?
//!
//! Pre-registered in
//! `docs/reports/2026-09-05-prospect-cluster-scale-n10.md` before this file
//! was written. Per this ledger's containment rule this file is reverted
//! before the report's final commit — its source is embedded verbatim in
//! that report's Reproduce section, which is the durable artifact from here
//! on, not this file.

use std::time::Duration;

use autumn_web::cluster::{ClusterHandle, ClusterMemberInfo, install_from_config};
use autumn_web::config::{AutumnConfig, ClusterConfig};
use autumn_web::test::{TestApp, TestClient};
use std::sync::Arc;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

const SECRET: &str = "a-shared-cluster-secret-value-32";
const COUNTER: &str = "prospect_n10";
const N: usize = 10;

const CONVERGE_TIMEOUT: Duration = Duration::from_secs(10);
const DEPARTURE_TIMEOUT: Duration = Duration::from_secs(5);
const TOTAL_TEST_TIMEOUT: Duration = Duration::from_secs(90);

// Last read lands at t≈1250ms — strictly past `suspicion_timeout_ms`=1000ms
// and several multiples of `push_interval_ms`=200ms, matching the N=5
// assay's own (heavily corrected) debounce window.
const STABLE_CHECKS: u32 = 6;
const STABLE_GAP: Duration = Duration::from_millis(250);

fn cluster_config(node_id: &str, seed_peers: Vec<String>) -> ClusterConfig {
    ClusterConfig {
        enabled: true,
        secret: Some(secrecy::SecretString::from(SECRET.to_owned())),
        bind_addr: "127.0.0.1:0".to_owned(),
        seed_peers,
        push_interval_ms: 200,
        suspicion_timeout_ms: 1_000,
        node_id: Some(node_id.to_owned()),
        ..ClusterConfig::default()
    }
}

fn app_config(cluster: ClusterConfig) -> AutumnConfig {
    AutumnConfig {
        cluster,
        ..AutumnConfig::default()
    }
}

/// A running node's cluster handle + shutdown control. Deliberately holds no
/// `TestClient`: `TestClient` carries a `HashMap<TypeId, Box<dyn Any + Send>>`
/// extensions map that is `Send` but not `Sync`, and every assertion helper
/// below takes `&[Node]` across an `.await` inside a `tokio::spawn`ed
/// (Send-bound) future — a shared reference is `Send` only if its referent
/// is `Sync`, so a `TestClient` field here would make the whole apparatus
/// fail to compile. The `TestClient`s themselves still need to stay alive
/// for the test's duration (the background gossip/transport tasks are
/// spawned off their own state, but nothing in this codebase promises a
/// `TestClient` is safe to drop early), so `spawn_star_cluster`/
/// `install_node` return them separately, owned (not referenced) by
/// `run_assay`, which only ever holds them, never touches or shares them.
struct Node {
    handle: Arc<ClusterHandle>,
    shutdown: CancellationToken,
}

fn install_node(node_id: &str, seed_peers: Vec<String>) -> (TestClient, Node) {
    let shutdown = CancellationToken::new();
    let config = cluster_config(node_id, seed_peers);
    let app = TestApp::new().config(app_config(config.clone())).build();
    install_from_config(app.state(), &config, &shutdown)
        .unwrap_or_else(|error| panic!("node {node_id} must install: {error}"));
    let handle = app
        .state()
        .extension::<ClusterHandle>()
        .unwrap_or_else(|| panic!("node {node_id} must expose a ClusterHandle"));
    (app, Node { handle, shutdown })
}

/// Star topology: node 0 has no seeds, nodes 1..N seed only from node 0's
/// observed `local_addr()` — the minimal seed-list shape the guide describes
/// and the exact shape the N=5 assay used. Returns the owned `TestApp`s
/// (kept alive, never referenced again) separately from the `Node`s that
/// the rest of the apparatus actually operates on.
fn spawn_star_cluster(n: usize) -> (Vec<TestClient>, Vec<Node>) {
    let mut apps: Vec<TestClient> = Vec::with_capacity(n);
    let mut nodes = Vec::with_capacity(n);
    let (app0, node0) = install_node("node-0", Vec::new());
    let seed = node0.handle.local_addr().to_string();
    apps.push(app0);
    nodes.push(node0);
    for i in 1..n {
        let (app, node) = install_node(&format!("node-{i}"), vec![seed.clone()]);
        apps.push(app);
        nodes.push(node);
    }
    (apps, nodes)
}

/// One handle's membership view, read exactly once, so a cardinality check
/// and an identity check derived from it can never see two different live
/// states (the N=5 assay's twenty-second correction: reading `members()`
/// twice per observation let a transition land between the two reads).
fn snapshot(handle: &Arc<ClusterHandle>) -> Vec<ClusterMemberInfo> {
    handle.members()
}

/// `(id, addr, incarnation)` triples, sorted — the full documented,
/// replicated identity. `status` is deliberately excluded: the guide
/// documents it as a local, never-replicated overlay that can legitimately
/// differ transiently between healthy nodes (confirmed against
/// `docs/guide/clustering.md` and `ClusterHandle::members()` by the N=5
/// assay before adopting this exclusion; not re-derived here).
fn identity(members: &[ClusterMemberInfo]) -> Vec<(String, String, u64)> {
    let mut rows: Vec<_> = members
        .iter()
        .map(|m| (m.id.clone(), m.addr.clone(), m.incarnation))
        .collect();
    rows.sort();
    rows
}

/// Debounced poll: the condition must hold on `STABLE_CHECKS` consecutive
/// observations `STABLE_GAP` apart before counting as converged. Classifies
/// against both `timeout` (soft line) and `timeout * 3` (the pre-
/// registration's own divergence threshold) immediately after each read,
/// before any sleep — evaluating after an unconditional trailing sleep can
/// misclassify a read that landed inside a deadline as having landed
/// outside it (the N5 assay's fourteenth correction). A positive streak
/// that has already begun by the hard deadline is allowed to finish rather
/// than being killed mid-stream (the N=5 assay's twentieth/twenty-third
/// corrections) — classified by when the streak *began*, not by how long
/// its own confirmation took.
async fn poll_until_stable<F>(label: &str, timeout: Duration, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = tokio::time::Instant::now();
    let hard_deadline = start + timeout * 3;
    let mut streak: u32 = 0;
    let mut streak_start = start;
    loop {
        let now = tokio::time::Instant::now();
        let ok = condition();
        if ok {
            if streak == 0 {
                streak_start = now;
            }
            streak += 1;
            if streak >= STABLE_CHECKS {
                let late = streak_start > start + timeout;
                let diverged = streak_start > hard_deadline;
                if diverged {
                    eprintln!(
                        "{label}: streak of {STABLE_CHECKS} completed but only after the hard \
                         deadline (streak began at {:?} past start, hard deadline was {:?} past \
                         start) — DIVERGED",
                        streak_start - start,
                        hard_deadline - start
                    );
                    return false;
                }
                if late {
                    eprintln!(
                        "{label}: LateConverged — streak began at {:?} past start, past the \
                         soft {timeout:?} line but at-or-before the hard {:?} divergence line",
                        streak_start - start,
                        hard_deadline - start
                    );
                } else {
                    eprintln!("{label}: Converged within {timeout:?}");
                }
                return true;
            }
        } else {
            streak = 0;
            if now >= hard_deadline {
                eprintln!(
                    "{label}: no stable streak reached before the hard deadline ({:?} past \
                     start) — DIVERGED",
                    hard_deadline - start
                );
                return false;
            }
        }
        tokio::time::sleep(STABLE_GAP).await;
    }
}

fn describe_all(nodes: &[Node]) -> String {
    nodes
        .iter()
        .map(|n| {
            format!(
                "{}: members={:?} counter={}",
                n.handle.node_id(),
                n.handle.members(),
                n.handle.counter(COUNTER).get()
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

async fn assert_stable_membership(label: &str, nodes: &[Node], expected_n: usize, timeout: Duration) {
    let ok = poll_until_stable(label, timeout, || {
        let snapshots: Vec<_> = nodes.iter().map(|n| snapshot(&n.handle)).collect();
        if snapshots.iter().any(|s| s.len() != expected_n) {
            return false;
        }
        let first = identity(&snapshots[0]);
        snapshots.iter().all(|s| identity(s) == first)
    })
    .await;
    assert!(
        ok,
        "{label}: nodes must converge on an identical {expected_n}-member view; {}",
        describe_all(nodes)
    );
}

async fn assert_stable_counter_sum(label: &str, nodes: &[Node], expected_sum: u64, timeout: Duration) {
    let ok = poll_until_stable(label, timeout, || {
        nodes
            .iter()
            .all(|n| n.handle.counter(COUNTER).get() == expected_sum)
    })
    .await;
    assert!(
        ok,
        "{label}: every node must read the exact sum {expected_sum}; {}",
        describe_all(nodes)
    );
}

async fn run_assay() {
    // Step 1: cold-start convergence to a full N-member view. `_apps` is
    // never touched again after this point — it exists solely to keep the
    // `TestApp`s (and their background gossip/transport tasks) alive for
    // the rest of the function; see the `Node` doc comment for why it must
    // stay a separate binding rather than living inside `Node`.
    let (mut _apps, nodes) = spawn_star_cluster(N);
    assert_stable_membership("step1-cold-start", &nodes, N, CONVERGE_TIMEOUT).await;

    // Step 2: N genuinely concurrent increments (barrier-synced spawned
    // tasks — a plain sequential loop cannot support a "concurrent" claim,
    // the N=5 assay's thirteenth correction), total = 1+2+...+N.
    let barrier = std::sync::Arc::new(Barrier::new(N));
    let mut joins = Vec::with_capacity(N);
    for (i, node) in nodes.iter().enumerate() {
        let handle = node.handle.clone();
        let barrier = barrier.clone();
        joins.push(tokio::spawn(async move {
            barrier.wait().await;
            handle.counter(COUNTER).increment_by((i as u64) + 1);
        }));
    }
    for join in joins {
        join.await.expect("increment task must not panic");
    }
    let expected_sum: u64 = (1..=N as u64).sum();
    assert_stable_counter_sum("step2-counter-merge", &nodes, expected_sum, CONVERGE_TIMEOUT).await;
    assert_stable_membership("step2-post-counter-membership", &nodes, N, CONVERGE_TIMEOUT).await;

    // Step 3: clean departure of the last node. `pop()`, not a slice — the
    // departed node must actually leave `nodes` (not merely be ignored by a
    // slice bound), or step 4 would silently carry N+1 nodes forward.
    let mut nodes = nodes;
    let departed = nodes.pop().expect("N-1 index must exist");
    let departed_id = departed.handle.node_id().to_owned();
    departed.shutdown.cancel();
    assert_stable_membership("step3-departure", &nodes, N - 1, DEPARTURE_TIMEOUT).await;
    assert_stable_counter_sum("step3-post-departure-counter", &nodes, expected_sum, DEPARTURE_TIMEOUT).await;
    assert_stable_membership("step3-post-counter-membership", &nodes, N - 1, DEPARTURE_TIMEOUT).await;
    drop(departed);

    // Step 4: rejoin with the SAME identity (not a new member), re-seeded
    // from node 0 — the N=5 assay's eighteenth correction: an
    // entropy-derived fresh id would test a new member joining, not a
    // genuine rejoin.
    let seed = nodes[0].handle.local_addr().to_string();
    let (rejoined_app, rejoined) = install_node(&departed_id, vec![seed]);
    assert_eq!(
        rejoined.handle.node_id(),
        departed_id,
        "the rejoined node must reuse the departed node's exact identity"
    );
    _apps.push(rejoined_app);
    nodes.push(rejoined);
    assert_eq!(nodes.len(), N, "rejoin must bring the cluster back to exactly N members");
    assert_stable_membership("step4-rejoin", &nodes, N, CONVERGE_TIMEOUT).await;
    assert_stable_counter_sum("step4-post-rejoin-counter", &nodes, expected_sum, CONVERGE_TIMEOUT).await;
    assert_stable_membership("step4-post-counter-membership", &nodes, N, CONVERGE_TIMEOUT).await;

    for node in &nodes {
        node.shutdown.cancel();
    }
}

#[test]
fn cluster_converges_correctly_at_n10() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build a multi-thread runtime");
    let result = runtime.block_on(async {
        let join = tokio::spawn(run_assay());
        tokio::time::timeout(TOTAL_TEST_TIMEOUT, join).await
    });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => std::panic::resume_unwind(panic.into_panic()),
        Err(_) => panic!(
            "assay did not complete within the {TOTAL_TEST_TIMEOUT:?} whole-test kill line \
             (panic/deadlock/hang)"
        ),
    }
    runtime.shutdown_timeout(Duration::from_secs(5));
}
RUST_EOF

cargo test -p autumn-web --features test-support --test integration_tests cluster_two_node
cargo test -p autumn-web --features test-support --test prospect_cluster_scale_n10 -- --nocapture

# Cleanup (done before this report's final commit):
rm autumn/tests/prospect_cluster_scale_n10.rs
# ...and remove the temporary [[test]] entry from autumn/Cargo.toml.
```
