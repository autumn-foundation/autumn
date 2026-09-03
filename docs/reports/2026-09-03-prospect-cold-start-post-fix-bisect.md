# ⛏️ Prospect: which post-#2309-fix commit ate the missing cold-start savings? (TBD)

## 🎯 Question

The 2026-09-02 verification assay (`docs/reports/2026-09-02-prospect-cold-start-db-gate-verify.md`,
PR #2434) confirmed PR #2360's `db`-feature-gate fix for issue #2309 is real
at the crate level (`autumn-macros` self-compile: 54.8s → 3.34s with `db`
off, ~94%) and at the controlled binary level (PR #2360's own same-box A/B:
136,594ms → 90,846ms, -33%), but found the real CI trajectory only moved
120,422ms → 105,371ms (-12.5%) across the same before/after commits
(`d1ecb361` → `ef61ae44`). That assay explicitly left the gap unexplained
and named it a follow-up: *"a `cargo build --timings` bisection over the
full comparison interval... is the follow-up, not this verification."* It
also named three commits as candidates "because they touch default
features/deps in that window" — but that list turns out to be wrong: one of
the three (`1fd6245`, Ledgered entities) is **not even an ancestor of
`ef61ae44`** (`git merge-base --is-ancestor 1fd6245 ef61ae44` fails), so it
cannot be part of this window's compile-time delta at all. This assay
corrects that and re-derives the real commit set.

**Falsifiable question:** among the commits actually in `d1ecb361..ef61ae44`
(12 commits, verified below), does any single non-fix commit measurably add
compile-time weight to the no-DB daemon feature set (`autumn-web
--no-default-features --features maud,htmx,tailwind,cache-moka,http-client,reporting`,
i.e. exactly `DAEMON_NO_DB_FEATURES` from `autumn-cli/src/new.rs`) — enough
to plausibly account for a material share of the ~15,051ms shortfall between
the fix's own confirmed relative saving and the real CI trajectory? Or does
no single commit explain it, meaning the shortfall is better attributed to
statistical/runner noise (CI's 3-sample p95 vs PR #2360's 1-sample local A/B,
different runners, a week apart) rather than to a specific regression?

**Decision:** whether issue #2309 needs a second, narrowly-scoped follow-up
PR (revert or feature-gate whatever commit is found responsible) versus
whether the issue can stay open purely as a "budget was never realistic for
this dependency graph" tracking item with no further code archaeology
warranted. **Decider:** repo maintainer (same as the parent assay — issue
#2309 has no other owner).

## ⚖️ Pre-registration

Committed in this same commit, before any timed build runs.

- **Actual window** (superseding the parent report's unverified guess),
  oldest → newest, confirmed via `git log --oneline d1ecb361..ef61ae44` and
  `git merge-base --is-ancestor`:
  `61bdd9c` (Web Push, #2334) → `28c6fae` (scaffold reconciliation, #2340) →
  `651929b` (**the #2309 fix itself**, #2360) → `d6aa668` (admin
  impersonation, #2339) → `97a97be` (replay capsule seams, #2348) →
  `d96cfab` (cached-read staleness proof, #2352) → `fec5221` (zero-downtime
  state migration, #2345) → `8f94e60` (CI-only: clippy parallelism, #2361)
  → `76c56b1` (docs/routing example, #2341) → `9c1ede1` (db scrub, #2365) →
  `f15ca1b` (classified-data compile-time proof, #2367) → `ef61ae4` (Bolt
  form-field escaping, #2376 — the post-fix CI-measured commit itself).
- **Pursue-bisection-further line:** a single commit's isolated build delta
  (vs. the immediately preceding checkpoint, same feature set, same warmed
  target dir) is ≥ 5,000ms — large enough to plausibly be a real,
  reproducible contributor to the ~15s CI shortfall rather than noise.
  Report it as the (or a) responsible commit, with the specific dependency
  or codegen path named from `cargo build --timings` output.
- **Kill line (for the "specific commit" hypothesis):** no single commit's
  isolated delta reaches 5,000ms, and the sum of all deltas across the
  window stays within run-to-run noise of a repeated build at one fixed
  commit (measured as a same-commit repeat at the final checkpoint). If so,
  the verdict is that the shortfall is **not** attributable to a specific
  commit in this window and the parent report's "far less than confirmed"
  finding stands without a named cause — CI statistical/runner variance is
  the better explanation, and no further bisection is warranted.
- **Conditions:** this sandbox (4-core, 15GiB, rustc/cargo 1.94.1 — same
  environment class as the parent assay's control). A dedicated git
  worktree, detached at each commit in chronological order. One shared
  `target/` directory reused across all 12 checkpoints (external
  dependency compiles stay warm across the waterfall, matching how CI's
  `Swatinem/rust-cache` persists across weekly runs) — but before every
  timed build, the workspace-local crates' own fingerprints and incremental
  artifacts are cleared (`autumn-macros`, `autumn-web`/`autumn`, and any
  other workspace member whose Cargo.toml changed in that commit), so every
  timed number reflects a genuine from-scratch rebuild of *this repo's own
  code* at that commit, never reused workspace-crate object files.
  `CARGO_INCREMENTAL=0`. Command timed:
  `cargo build -p autumn-web --no-default-features --features maud,htmx,tailwind,cache-moka,http-client,reporting`
  (exactly `DAEMON_NO_DB_FEATURES`, joined). One run per checkpoint (12
  checkpoints, chronological), plus one repeat run at the final checkpoint
  (`ef61ae4`) to give a same-commit noise floor.
- **Time box:** same session. Target ≤ 60 minutes of cumulative build
  wall-clock. If any single checkpoint step exceeds 10 minutes twice in a
  row, or the cumulative budget is exhausted before reaching `ef61ae4`,
  stop and report undetermined with the partial waterfall — that is itself
  a finding (this class of bisection is too expensive to do cheaply on a
  weekly cadence, which bears on whether it's worth CI ever doing this
  automatically).
- **Riskiest assumption tested first:** that the shortfall is explained by
  *one* commit's dependency-graph change at all, rather than being an
  artifact of comparing a 1-sample local A/B (PR #2360) against a 3-sample
  CI p95 on a different runner a week apart (the parent report's own stated
  caveat). The same-commit repeat run directly tests this: if repeat-run
  noise at a single fixed commit is itself comparable to the per-commit
  deltas seen across the window, the whole bisection approach is answering
  a question noise already explains, and that finding is reported as such.
- **Containment:** a detached, uncommitted git worktree
  (`/tmp/prospect-coldstart-wt`) outside the repo's tracked tree; no CI
  triggered; no changes to `trunk`/`trunk-dev`; this report is the only
  artifact that lands in the PR.

## 🔍 Prior art

- `docs/reports/2026-09-02-prospect-cold-start-db-gate-verify.md` (PR
  #2434, merged) — the parent verification assay this one follows up on;
  confirms the fix works in isolation and names the bisection as the open
  follow-up.
- Issue #2309 — root cause, still open, un-recalibrated budget.
- PR #2360 / `651929b` — the fix itself; its commit message's own A/B
  number is the "confirmed saving" baseline this assay is trying to
  reconcile against real CI.
- No other report or open PR revisits this gap since the 2026-09-02 report
  merged (checked via `search_pull_requests` for cold-start/bisect terms
  and by re-reading issue #2309's thread — no new activity).

## 🧪 Apparatus

- One detached git worktree, walked chronologically through the 12 commits
  above, each timed with the command in **Conditions**.
- No stubs beyond what the parent assay already used: this measures a
  library crate's feature-gated build, not a running app — no server is
  started, no HTTP request is made. That is a deliberate, cheaper proxy for
  the full `autumn new` cold-start journey (which also includes scaffold
  templating and the generated app's own thin compile unit) — the parent
  assay's control used the same proxy for `autumn-macros` alone and it
  matched the full-journey CI numbers well in direction/magnitude, though
  not exactly. This assay inherits that same limitation: it can name which
  commit adds compile weight to `autumn-web`'s no-DB feature set, but a
  match to the exact CI millisecond figure is not expected or claimed.

## 📊 Assay

_Filled in after runs complete — see the follow-up commit to this file._
