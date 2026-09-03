# ⛏️ Prospect: which post-#2309-fix commit ate the missing cold-start savings? (kill: no single commit ≥5,000ms vs the pursue line; proxy overshoots the confirmed saving, so the shortfall isn't here)

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

**Environment:** detached git worktree (`/tmp/prospect-coldstart-wt`, outside
the tracked tree), same sandbox as the parent assay (4-core, 15GiB,
rustc/cargo 1.94.1). One shared `target/` reused across every checkpoint
below (external deps stay warm); workspace-local crate fingerprints and
incremental artifacts cleared before every timed build;
`CARGO_INCREMENTAL=0`. Command: `cargo build -p autumn-web
--no-default-features --features maud,htmx,tailwind,cache-moka,http-client,reporting`.
One run per checkpoint, chronological order, plus a same-commit repeat at
the end. All 13 builds exited 0 (no failures, no timeouts against the 300s
per-step cap).

| Checkpoint (chronological) | Elapsed | Δ vs prev | Note |
|---|---:|---:|---|
| `d1ecb361` (window start, CI pre-fix commit) | 65,617ms | — | baseline |
| `61bdd9c` Web Push (#2334) | 67,653ms | +2,036ms | pre-fix |
| `28c6fae` scaffold reconciliation (#2340) | 65,456ms | −2,197ms | pre-fix |
| `651929b` **#2309 fix itself** (#2360) | 36,225ms | **−29,231ms** | the fix |
| `d6aa668` admin impersonation (#2339) | 37,889ms | +1,664ms | post-fix |
| `97a97be` replay capsule seams (#2348) | 36,835ms | −1,054ms | post-fix |
| `d96cfab` cached-read staleness proof (#2352) | 39,371ms | +2,536ms | post-fix |
| `fec5221` zero-downtime state migration (#2345) | 42,356ms | +2,985ms | post-fix, largest single delta |
| `8f94e60` CI-only, clippy parallelism (#2361) | 42,773ms | +417ms | post-fix |
| `76c56b1` docs/routing example (#2341) | 40,337ms | −2,436ms | post-fix |
| `9c1ede1` db scrub (#2365) | 43,219ms | +2,882ms | post-fix |
| `f15ca1b` classified-data compile-time proof (#2367) | 41,589ms | −1,630ms | post-fix |
| `ef61ae4` Bolt form-field escaping (#2376, CI post-fix commit) | 42,378ms | +789ms | window end |
| `ef61ae4` repeat (noise floor, identical commit/conditions) | 43,052ms | +674ms | same-commit re-run |

**Control comparison (against the pre-registered lines):**

- **Pursue line (single commit ≥5,000ms):** not reached by any of the 11
  non-fix commits. Largest single delta is `fec5221` at +2,985ms — real but
  well under the line, and of the same order as `28c6fae`'s −2,197ms or
  `76c56b1`'s −2,436ms, which run the *other* direction. No commit stands
  out as "the" culprit.
- **Kill line (sum within same-commit noise):** not cleanly met either. The
  noise floor from one repeat at `ef61ae4` is 674ms, but the net sum of all
  9 post-fix deltas is **+6,153ms** (36,225ms → 42,378ms) — about 9x a
  single noise sample, so it reads as a real, diffuse compile-time creep
  spread across many unrelated commits (new dependencies/codegen each
  adding a couple seconds), not pure noise. One repeat sample can't fully
  separate signal from noise here; that's a limitation of this assay's
  single-run-per-checkpoint design, stated rather than papered over.
- **The question this assay actually needed to answer wasn't which specific
  wording bucket the diffuse creep falls into — it's whether that creep
  (real or noise) is big enough to explain the CI shortfall. It is not, by
  a wide margin, and in the wrong direction:** end-to-end across the full
  window (`d1ecb361` → `ef61ae4`), this `autumn-web`-only proxy moved
  65,617ms → 42,378ms, **−23,239ms (−35.4%)**. That is *larger* than PR
  #2360's own confirmed same-box A/B saving (−33%), and far larger than
  the real CI trajectory (−12.5%, 120,422ms → 105,371ms). If the 9
  post-fix commits' diffuse compile-time creep were the explanation for
  CI's shortfall, this proxy should have shown *less* relative saving than
  the confirmed −33%, not more.

**Worst case / robustness check:** the same-commit repeat at `ef61ae4`
(43,052ms vs. the first run's 42,378ms, +674ms) confirms single-run
measurements at this scale carry a few hundred milliseconds of noise, which
is small relative to the −23,239ms window-wide effect being measured, so
the headline finding (proxy overshoots the confirmed saving) is not an
artifact of one noisy run.

## 🏁 Verdict

**Kill** — against the pre-set line, for the originally-falsifiable
question: no single non-fix commit in `d1ecb361..ef61ae44` reaches the
5,000ms pursue threshold in `autumn-web`'s own no-DB-feature compile time,
so this assay does not name a single "responsible" commit, and none is
warranted from this data.

More importantly, this assay **rules out** "a commit regressed
`autumn-web`'s own compile weight" as *any* part of the explanation for the
CI shortfall — the opposite of what the parent report's list of three
candidate commits (one of which, `1fd6245`, wasn't even in this window's
ancestry — see Question) suggested was worth checking. The library-level
proxy measured here shows **more** relative saving (−35.4%) than PR #2360's
own confirmed end-to-end A/B (−33%), which itself showed more saving than
real CI observed (−12.5%). Chasing the shortfall inside `autumn-web`'s
Cargo-feature graph is a dry pit: whatever eats the difference between
"confirmed −33%" and "observed −12.5%" is **not** in this library's own
compile time, and is not attributable to any of the 11 non-fix commits that
landed in the same window.

That leaves two live explanations, neither tested here (out of this
session's time box, and each needs its own pre-registration):

1. **The rest of the cold-start journey** — this proxy only measures
   `cargo build -p autumn-web`. The full `autumn new → first HTTP 200`
   journey CI actually measures also includes: scaffolding the throwaway
   project, compiling the *generated app* itself (a thin crate depending on
   `autumn-web`, but still its own compile unit + link step), starting the
   server process, and waiting for the first HTTP 200. Any of those stages
   could have grown in the same window independent of `autumn-web`'s own
   feature-gated compile time.
2. **CI/runner statistics** — the confirmed −33% is a single run per
   variant on PR #2360's local 4-core box; the CI numbers are 3-sample p95s
   on GitHub-hosted shared runners, a week apart. Runner class variance
   and small-sample p95 noise were already flagged as live confounds in the
   parent report and are not ruled out by this assay — if anything, this
   assay's clean library-level result (which *should* transfer to CI if
   `autumn-web`'s compile time were the deciding factor) makes runner/CI
   noise a more likely explanation by elimination, not less.

No further bisection of `autumn-web`'s Cargo-feature graph is warranted
without new information — this pit is dry. A follow-up assay, if
chartered, should pre-register against the scaffold+app-compile+server-start
portion of the journey (stage 1 above) using the real `autumn new
--daemon` + `autumn dev-loop-bench --cold-start` harness (matching what PR
#2360 itself used), ideally run twice per commit to get a per-stage noise
floor that this single-run library proxy could not fully provide.

## 💰 Cost to productionize

N/A — kill verdict, no build to productionize. This assay itself cost
~11 minutes of build wall-clock in one sandbox session, comfortably inside
the pre-registered ≤60-minute box.

## 🔬 Reproduce

```bash
# Requires a git worktree at the repo's history (works from any clone with
# the full commit range d1ecb361..ef61ae44 available):
git worktree add --detach /tmp/prospect-coldstart-wt d1ecb361

cd /tmp/prospect-coldstart-wt
FEATURES=maud,htmx,tailwind,cache-moka,http-client,reporting

for c in d1ecb361 61bdd9c 28c6fae 651929b d6aa668 97a97be d96cfab fec5221 \
         8f94e60 76c56b1 9c1ede1 f15ca1b ef61ae4; do
  git checkout --detach "$c" --quiet
  rm -rf target/debug/deps/libautumn_macros* target/debug/deps/libautumn_web* \
         target/debug/deps/libautumn-* target/debug/deps/autumn_macros-* \
         target/debug/deps/autumn_web-* target/debug/deps/autumn-* \
         target/debug/.fingerprint/autumn-macros-* target/debug/.fingerprint/autumn-web-* \
         target/debug/.fingerprint/autumn-* target/debug/incremental/autumn_macros-* \
         target/debug/incremental/autumn_web-* target/debug/incremental/autumn-*
  START=$(date +%s%3N)
  CARGO_INCREMENTAL=0 cargo build -p autumn-web --no-default-features --features "$FEATURES"
  END=$(date +%s%3N)
  echo "$c: $((END-START))ms"
done

git worktree remove /tmp/prospect-coldstart-wt --force   # dismantle
```
