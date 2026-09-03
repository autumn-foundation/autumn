# ⛏️ Prospect: which commits ate the missing cold-start savings? (undetermined: no single commit ≥5,000ms, but +10,655ms of diffuse creep across 31 commits closely tracks the CI shortfall in absolute terms)

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
features/deps in that window."

**Falsifiable question:** among the commits in `d1ecb361..ef61ae44`, does
any single non-fix commit measurably add compile-time weight to the no-DB
daemon feature set (`autumn-web --no-default-features --features
maud,htmx,tailwind,cache-moka,http-client,reporting`, i.e. exactly
`DAEMON_NO_DB_FEATURES` from `autumn-cli/src/new.rs`) — enough to plausibly
account for a material share of the ~15,051ms shortfall between the fix's
own confirmed relative saving and the real CI trajectory? Or does no single
commit explain it, meaning the shortfall is better attributed to
statistical/runner noise (CI's 3-sample p95 vs PR #2360's 1-sample local
A/B, different runners, a week apart) rather than to a specific regression?

**Decision:** whether issue #2309 needs a second, narrowly-scoped follow-up
PR (revert or feature-gate whatever commit is found responsible) versus
whether the issue can stay open purely as a "budget was never realistic for
this dependency graph" tracking item with no further code archaeology
warranted. **Decider:** repo maintainer (same as the parent assay — issue
#2309 has no other owner).

## ⚖️ Pre-registration

Committed in commit `cf3ffdd`, before any timed build runs. Reproduced
verbatim below — **do not edit this section**; the correction discovered
after committing it is recorded separately, immediately after.

> - **Actual window** (superseding the parent report's unverified guess),
>   oldest → newest, confirmed via `git log --oneline d1ecb361..ef61ae44` and
>   `git merge-base --is-ancestor`:
>   `61bdd9c` (Web Push, #2334) → `28c6fae` (scaffold reconciliation, #2340) →
>   `651929b` (**the #2309 fix itself**, #2360) → `d6aa668` (admin
>   impersonation, #2339) → `97a97be` (replay capsule seams, #2348) →
>   `d96cfab` (cached-read staleness proof, #2352) → `fec5221` (zero-downtime
>   state migration, #2345) → `8f94e60` (CI-only: clippy parallelism, #2361)
>   → `76c56b1` (docs/routing example, #2341) → `9c1ede1` (db scrub, #2365) →
>   `f15ca1b` (classified-data compile-time proof, #2367) → `ef61ae4` (Bolt
>   form-field escaping, #2376 — the post-fix CI-measured commit itself).
> - **Pursue-bisection-further line:** a single commit's isolated build delta
>   (vs. the immediately preceding checkpoint, same feature set, same warmed
>   target dir) is ≥ 5,000ms — large enough to plausibly be a real,
>   reproducible contributor to the ~15s CI shortfall rather than noise.
>   Report it as the (or a) responsible commit, with the specific dependency
>   or codegen path named from `cargo build --timings` output.
> - **Kill line (for the "specific commit" hypothesis):** no single commit's
>   isolated delta reaches 5,000ms, and the sum of all deltas across the
>   window stays within run-to-run noise of a repeated build at one fixed
>   commit (measured as a same-commit repeat at the final checkpoint). If so,
>   the verdict is that the shortfall is **not** attributable to a specific
>   commit in this window and the parent report's "far less than confirmed"
>   finding stands without a named cause — CI statistical/runner variance is
>   the better explanation, and no further bisection is warranted.
> - **Conditions:** this sandbox (4-core, 15GiB, rustc/cargo 1.94.1 — same
>   environment class as the parent assay's control). A dedicated git
>   worktree, detached at each commit in chronological order. One shared
>   `target/` directory reused across all checkpoints (external dependency
>   compiles stay warm across the waterfall, matching how CI's
>   `Swatinem/rust-cache` persists across weekly runs) — but before every
>   timed build, the workspace-local crates' own fingerprints and incremental
>   artifacts are cleared, so every timed number reflects a genuine
>   from-scratch rebuild of *this repo's own code* at that commit, never
>   reused workspace-crate object files. `CARGO_INCREMENTAL=0`. Command
>   timed: `cargo build -p autumn-web --no-default-features --features
>   maud,htmx,tailwind,cache-moka,http-client,reporting` (exactly
>   `DAEMON_NO_DB_FEATURES`, joined). One run per checkpoint, chronological,
>   plus one repeat run at the final checkpoint (`ef61ae4`) to give a
>   same-commit noise floor.
> - **Time box:** same session. Target ≤ 60 minutes of cumulative build
>   wall-clock. If any single checkpoint step exceeds 10 minutes twice in a
>   row, or the cumulative budget is exhausted before reaching `ef61ae4`,
>   stop and report undetermined with the partial waterfall.
> - **Riskiest assumption tested first:** that the shortfall is explained by
>   *one* commit's dependency-graph change at all, rather than being an
>   artifact of comparing a 1-sample local A/B (PR #2360) against a 3-sample
>   CI p95 on a different runner a week apart.
> - **Containment:** a detached, uncommitted git worktree, outside the
>   repo's tracked tree; no CI triggered; no changes to `trunk`/`trunk-dev`;
>   this report is the only artifact that lands in the PR.

### 🛠️ Correction (found during Codex review, before merge)

The first measurement pass used a **shallow clone** (`git rev-parse
--is-shallow-repository` → `true`) without noticing it. That silently
broke two things the pre-registration relied on:

1. `git log --oneline d1ecb361..ef61ae44` returned only **12** commits
   instead of the true **32** (`git rev-list --count`, confirmed after
   `git fetch --unshallow`) — 20 real commits were never in the measured
   window at all, collapsed into one invisible jump from `d1ecb361` straight
   to `61bdd9c`.
2. `git merge-base --is-ancestor 1fd6245 ef61ae44` reported failure inside
   the shallow clone; after unshallowing it correctly reports success —
   `1fd6245` (Ledgered entities, #2318) **is** an ancestor of `ef61ae44`
   and **is** part of the real window. The claim in this report's first
   pushed revision that it wasn't was wrong, caused by the same shallow
   clone, not by the actual repository history.

Both errors were caught by `chatgpt-codex-connector`'s review of PR #2477
(two separate P1 findings) before this report merged. **Section "🔍 Prior
art" below is corrected to match; the window used in the "🧪 Apparatus" and
"📊 Assay" sections below is the full, verified 32-commit list, superseding
the incomplete 12-commit list quoted in the pre-registration blockquote
above.** The pre-registered *criteria, lines, conditions, and time box* are
unchanged and still govern the verdict — only the commit list the
pre-registration itself got wrong from tooling error, not from a change in
the question, is corrected. Re-running against the corrected, complete
window (rather than quietly patching the pre-registration's commit list to
match what was actually measured) is what "committed before measurement"
is supposed to prevent — so this correction reflects fixing a tooling bug
discovered *after* results existed, not re-drawing the window to fit them.

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
- **Correction:** the parent report's three named "candidate" commits
  included `1fd6245`, based on the same shallow-clone-induced ancestry
  error this report initially repeated. `1fd6245` **is** in the real
  window (see Correction above) and is measured below (checkpoint
  `1fd62458`, Δ +805ms — not a standout contributor).

## 🧪 Apparatus

- One detached git worktree, walked chronologically through the full,
  verified 32-commit window (`d1ecb361` exclusive → `ef61ae44` inclusive),
  each timed with the command in **Conditions**. `git fetch --unshallow`
  run first this time.
- Two additional checkpoints beyond the pre-registered plan, added when the
  data required them, not to move any line: the worktree's *first* build
  (`dc74ce43`) unavoidably paid a one-time cold dependency-compile cost
  (103,881ms) that the rest of the chronological walk did not, because
  external `target/` artifacts hadn't been populated yet — an apparatus
  artifact of build *order*, not of that commit's code. Both `d1ecb361`
  (the baseline) and `dc74ce43` (the first step) were re-measured once the
  target directory was fully warm from the other 31 builds, under the
  exact same clear-workspace-fingerprints-only protocol as every other
  checkpoint, so all 33 numbers used in the Assay table below are mutually
  comparable. The stale cold-build numbers (65,617ms for a differently-warm
  `d1ecb361` in the first, shallow-clone run; 103,881ms for `dc74ce43`) are
  reported here for transparency but not used in any delta.
- No stubs beyond what the parent assay already used: this measures a
  library crate's feature-gated build, not a running app — no server is
  started, no HTTP request is made. That is a deliberate, cheaper proxy for
  the full `autumn new` cold-start journey (which also includes scaffold
  templating and the generated app's own thin compile unit) — the parent
  assay's control used the same proxy for `autumn-macros` alone. This
  assay inherits that same limitation, stated plainly in the Verdict.

## 📊 Assay

**Environment:** detached git worktree (`/tmp/prospect-coldstart-wt2`,
outside the tracked tree, unshallowed), same sandbox as the parent assay
(4-core, 15GiB, rustc/cargo 1.94.1). One shared `target/` reused across
every checkpoint (external deps stay warm); workspace-local crate
fingerprints and incremental artifacts cleared before every timed build;
`CARGO_INCREMENTAL=0`. Command: `cargo build -p autumn-web
--no-default-features --features maud,htmx,tailwind,cache-moka,http-client,reporting`.
One run per checkpoint, chronological order, plus a same-commit repeat at
the end. All 34 builds (32 checkpoints + 2 warm re-measurements of
`d1ecb361`/`dc74ce43` + 1 final-checkpoint repeat) exited 0.

| Checkpoint (chronological) | Elapsed | Δ vs prev | Note |
|---|---:|---:|---|
| `d1ecb361` (window start, warm target) | 55,882ms | — | baseline |
| `dc74ce43` CI-feedback fixups (#2297) | 60,527ms | +4,645ms | pre-fix |
| `25ed48ff` Bolt: skip HTML-escaping constants (#2298) | 61,948ms | +1,421ms | pre-fix |
| `31423bfc` CI-feedback: macro-scaling + Redis glob (#2299) | 57,326ms | −4,622ms | pre-fix |
| `e2efa9b0` CI-feedback: CSRF cache-guard (#2303) | 57,664ms | +338ms | pre-fix |
| `6a6610c4` Reserve "all" search index name (#2305) | 62,654ms | +4,990ms | pre-fix, closest to the pursue line |
| `d81a4449` Bound response-body collection, edge capsule (#2307) | 60,223ms | −2,431ms | pre-fix |
| `7dff7e07` Fix TLS migration panic; named plugin migrations (#2306) | 60,866ms | +643ms | pre-fix |
| `c9121d5b` Bolt: defer error_id allocation (#2304) | 64,564ms | +3,698ms | pre-fix |
| `141f36ef` chore(deps): bump base64 (#2301) | 62,046ms | −2,518ms | pre-fix |
| `5cdb1e17` Ledger: commit hook claim/ack profile (#2300) | 61,829ms | −217ms | pre-fix |
| `747d204b` fix(cli): Azure bootstrap scaled to zero (#2247) | 56,915ms | −4,914ms | pre-fix |
| `bbac3304` Ledger: batch search-store upserts (#2308) | 57,589ms | +674ms | pre-fix |
| `9d99d980` Prove a route's DB query count at compile time (#2315) | 59,179ms | +1,590ms | pre-fix |
| `52af191d` docs: agents.md | 58,768ms | −411ms | pre-fix |
| `bc99a4b8` Prove scaffold DSL constraints at runtime (#2317) | 58,035ms | −733ms | pre-fix |
| `69bb9d18` docs(seo): SEO guide + reddit-clone wiring (#2325) | 59,912ms | +1,877ms | pre-fix |
| `55febd8a` Scaffold CSV import route (#2324) | 60,156ms | +244ms | pre-fix |
| `1fd62458` Ledgered entities (#2318) | 60,961ms | +805ms | pre-fix — corrects the "not an ancestor" error |
| `eba7fc67` Mirror live traffic to shadow build (#2327) | 64,087ms | +3,126ms | pre-fix |
| `24bf151c` examples: flagship 0.7.0 subsystems (#2338) | 62,793ms | −1,294ms | pre-fix |
| `61bdd9c2` Web Push (#2334) | 64,788ms | +1,995ms | pre-fix |
| `28c6fae8` scaffold reconciliation (#2340) | 63,559ms | −1,229ms | pre-fix |
| `651929b2` **#2309 fix itself** (#2360) | 39,226ms | **−24,333ms** | the fix |
| `d6aa6688` admin impersonation (#2339) | 39,720ms | +494ms | post-fix |
| `97a97be4` replay capsule seams (#2348) | 36,970ms | −2,750ms | post-fix |
| `d96cfab0` cached-read staleness proof (#2352) | 37,930ms | +960ms | post-fix |
| `fec52215` zero-downtime state migration (#2345) | 42,521ms | +4,591ms | post-fix |
| `8f94e60b` CI-only: clippy parallelism (#2361) | 42,289ms | −232ms | post-fix |
| `76c56b1c` docs/routing example (#2341) | 40,946ms | −1,343ms | post-fix |
| `9c1ede1e` db scrub (#2365) | 38,107ms | −2,839ms | post-fix |
| `f15ca1bb` classified-data compile-time proof (#2367) | 42,498ms | +4,391ms | post-fix |
| `ef61ae44` Bolt form-field escaping (#2376, window end) | 42,637ms | +139ms | window end |
| `ef61ae44` repeat (noise floor) | 42,204ms | −433ms | same-commit re-run |

**Control comparison (against the pre-registered lines):**

- **Pursue line (single commit ≥5,000ms):** not reached by any of the 31
  non-fix commits. Closest is `6a6610c4` at +4,990ms — 10ms under the line
  — followed by `fec52215` (+4,591ms) and `f15ca1bb` (+4,391ms). None
  crosses it, so per the pre-registered rule, no single commit is named as
  "the" culprit.
- **Kill line (sum within same-commit noise):** the only valid same-run,
  same-conditions noise sample is the `ef61ae44` repeat: 433ms. (A
  cross-run comparison of `651929b`'s two measurements — 36,225ms in the
  first, shallow-clone run vs. 39,226ms here — looked like a 3,001ms swing
  at first, but the two runs reached that commit through different warm-cache
  histories, i.e. different conditions, so it is **not** a valid noise
  sample and is excluded rather than used as evidence either way.) Against
  the one valid 433ms sample, the sum of all 31 non-fix deltas is
  **+10,655ms** (+7,677ms pre-fix across 22 commits, +2,978ms post-fix
  across 9 commits) — about 25x the noise sample. This is not noise by any
  reasonable reading, so the kill line's second clause is **not** met
  either.
- **Neither pre-registered line is met.** No single commit crosses the
  pursue threshold, but the cumulative non-fix effect is far too large to
  call noise. The pre-registration did not provide for this outcome, and
  per Prospect's own rule against moving the kill line after seeing the
  data, this assay does not force one — see Verdict.
- **What the complete data changes vs. the (wrong) partial-window read:**
  end-to-end across the real window, `d1ecb361` (55,882ms, warm) →
  `ef61ae44` (42,637ms) is **−13,245ms (−23.7%)**, or −13,678ms (−24.5%)
  using the repeat. That is a real, large improvement — but it sits
  *between* PR #2360's confirmed −33% same-box A/B and the real CI
  trajectory's −12.5%, not above the confirmed figure as the incomplete
  12-commit run wrongly concluded. In **absolute** terms (the number that
  should track CI's absolute 15,051ms saving more directly than a
  relative percentage computed against two very different baselines —
  this proxy's ~56-65k ms vs CI's ~105-120k ms full-journey baseline), this
  proxy's −13,245ms to −13,678ms is within ~1,373-1,806ms of CI's own
  −15,051ms saving. The fix's own raw in-context contribution is
  −24,333ms; **+10,655ms of that gets eaten back by the other 31 commits
  in the same window, mostly (+7,677ms) before the fix even landed.**

**Worst case / robustness check:** the same-commit repeat at `ef61ae44`
(42,204ms vs. the first run's 42,637ms, −433ms) shows single-run noise at
this scale is on the order of a few hundred milliseconds — small next to
the ±5,000ms line and tiny next to the +10,655ms cumulative effect, so
that cumulative figure is not an artifact of one noisy run. It cannot,
however, rule in or rule out any *individual* sub-5,000ms delta in the
table as signal vs. noise — that needs repeated measurement per commit,
not done here (see Verdict).

## 🏁 Verdict

**Undetermined**, against the pre-set lines exactly as written — not
"kill," which the first, incomplete-window revision of this report
incorrectly claimed, and not "pursue," since no single commit clears the
line either. Reporting this as a clean kill would have been moving the
goalposts after seeing the data (exactly what Prospect's rules forbid);
the honest reading of the pre-registration's two binary outcomes is that
neither fired.

What this assay does establish, and why it changes the parent report's
open question materially:

- **No single commit is "the" cause.** The closest, `6a6610c4` (Reserve
  "all" as a search index name, #2305) at +4,990ms, is a plausible
  candidate for a follow-up single-commit re-measurement (repeated runs to
  separate it from noise), but nothing here names it with confidence.
- **The CI shortfall is very likely a real, diffuse effect, not
  measurement noise or an artifact of the earlier incomplete window.**
  +10,655ms of cumulative compile-time creep, spread across 31 unrelated
  commits merged in the same week (22 before the fix, 9 after), nets the
  fix's raw −24,333ms saving down to −13,245ms in this library-only proxy
  — and that number lands within ~1.8s of CI's own observed −15,051ms
  absolute saving. That is a materially different, and much better
  grounded, conclusion than the incomplete-window revision's claim that
  library compile weight could be ruled out entirely: it cannot. This
  reverses that claim, per the two Codex review findings that caught it.
- **This proxy still cannot close the loop on its own.** It measures
  `autumn-web`'s own compile time, not the full `autumn new → first HTTP
  200` journey CI actually gates on (scaffolding, the generated app's own
  compile+link, server start, first request) — the ~1.8s residual gap
  between this proxy's absolute saving and CI's could easily live there,
  or in ordinary CI/runner variance (the parent report's already-flagged
  confound: a 1-sample local A/B vs. 3-sample p95s on shared runners a
  week apart). Nothing here distinguishes those two.

**What would resolve this, as an explicit follow-up (not chartered or run
here):**

1. Repeat each of the 31 non-fix checkpoints 2-3x to build real per-commit
   noise floors and re-test `6a6610c4`, `c9121d5b` (+3,698ms), `eba7fc67`
   (+3,126ms), `fec52215` (+4,591ms), and `f15ca1bb` (+4,391ms)
   specifically against the pursue line with that noise floor in hand.
2. Extend the same chronological-waterfall method to the
   scaffold+app-compile+server-start portion of the journey, using the
   real `autumn dev-loop-bench --cold-start` harness (matching what PR
   #2360 itself used), to close the residual ~1.8s gap between this
   proxy's absolute saving and CI's.

## 💰 Cost to productionize

N/A — undetermined verdict, no build to productionize. This assay's build
wall-clock: ~11 minutes for the first (shallow-clone, invalidated) pass,
~34 minutes for the corrected 32-checkpoint pass plus the two warm
re-measurements — about 45 minutes total across both passes, against the
pre-registered ≤60-minute *per-pass* box. The shallow-clone bug and its
correction cost real time; that overhead is itself a data point for anyone
scoping a repeat of this method (`git fetch --unshallow` first, always).

## 🔬 Reproduce

```bash
# IMPORTANT: unshallow first. A shallow clone silently truncates git log
# ranges and git merge-base --is-ancestor results without erroring — this
# is exactly the bug that produced this report's first, wrong revision.
git fetch --unshallow origin 2>/dev/null || true
git rev-parse --is-shallow-repository   # must print "false" before continuing

git worktree add --detach /tmp/prospect-coldstart-wt2 d1ecb361
cd /tmp/prospect-coldstart-wt2
FEATURES=maud,htmx,tailwind,cache-moka,http-client,reporting

# Full, verified window (d1ecb361 exclusive .. ef61ae44 inclusive, 32 commits):
COMMITS=(dc74ce43 25ed48ff 31423bfc e2efa9b0 6a6610c4 d81a4449 7dff7e07 c9121d5b \
         141f36ef 5cdb1e17 747d204b bbac3304 9d99d980 52af191d bc99a4b8 69bb9d18 \
         55febd8a 1fd62458 eba7fc67 24bf151c 61bdd9c2 28c6fae8 651929b2 d6aa6688 \
         97a97be4 d96cfab0 fec52215 8f94e60b 76c56b1c 9c1ede1e f15ca1bb ef61ae44)

# Warm baseline first (matches all other checkpoints' conditions):
for c in d1ecb361 "${COMMITS[@]}"; do
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

git worktree remove /tmp/prospect-coldstart-wt2 --force   # dismantle
```
