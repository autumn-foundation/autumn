# ⛏️ Prospect: does the gossip cluster still converge correctly at N=10? (pre-registration)

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
