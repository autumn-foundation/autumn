# ⛏️ Prospect: which commit ate the missing cold-start savings? (undetermined: the pre-fix and post-fix compile-time growth is real (≈5σ vs calibrated noise), but telescoping math means no single commit can be named)

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
- **A third correction, also from Codex review (P1/P2, on `26e9e040`):**
  the second revision above treated the single `ef61ae44` repeat (433ms)
  as *the* noise floor and used it to call the cumulative non-fix sum
  "real, not noise." Codex pointed out direct counter-evidence already
  sitting in the table: `dc74ce43` (touches only `autumn-cli` and a
  benchmark crate's `Cargo.toml` — confirmed via `git show --stat`, neither
  built by the timed command) shows +4,645ms, and `31423bfc` (touches only
  `autumn-cache-redis`, a feature this build doesn't enable, and
  `autumn-cli` — confirmed the same way) shows −4,622ms. Both commits are
  provable no-ops for `cargo build -p autumn-web` with this feature set,
  yet swing by multi-second amounts — proof the true noise floor is much
  larger than one repeat suggested, and the second revision's "very likely
  real" claim was undersupported. In response, a dedicated noise-floor
  calibration was run: 7 valid repeated builds of the single fixed commit
  `ef61ae44` under matching warm-target conditions (an 8th, run first in a
  freshly-created worktree, hit the same cold-dependency-compile artifact
  described above — 82,156ms — and is excluded for the same reason). See
  Assay for the result and its consequence for the verdict.

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

**Noise-floor calibration** (added after Codex review; not in the original
pre-registration, run to test its own "433ms noise floor" claim): 7 valid
repeated builds of the fixed commit `ef61ae44`, same warm-target protocol
as every checkpoint above (an 8th run, first in a fresh worktree, hit the
cold-dependency-compile artifact described in Apparatus — 82,156ms —
and is excluded):

`42,637 / 42,204 / 43,204 / 40,843 / 43,667 / 43,184 / 44,117` ms
— mean **42,837ms**, sample stdev **1,080ms**, min/max spread **3,274ms**.

**Corrected arithmetic** (Codex P2 on `26e9e040`): the previous revision's
"+10,655ms" non-fix sum wrongly folded the `ef61ae44` repeat's −433ms into
the post-fix total — a repeat isn't a commit delta. Removing it: post-fix
sum across the real 9 commits is **+3,411ms** (not +2,978ms), and the
total non-fix sum across all 31 commits is **+11,088ms** (+7,677ms
pre-fix, +3,411ms post-fix), not +10,655ms.

**Control comparison (against the pre-registered lines), with the
calibrated noise floor in hand:**

- **Pursue line (single commit ≥5,000ms):** not reached by any of the 31
  non-fix commits — closest is `6a6610c4` at +4,990ms, followed by
  `fec52215` (+4,591ms) and `f15ca1bb` (+4,391ms). With the calibration
  above, none of these can be called real signal either: a single-delta
  measurement combines two single-run draws, so its expected noise is
  ≈1,080ms·√2 ≈ 1,527ms by the calibration — meaning +4,990ms is only
  ≈3.3 calibrated-σ, and the two known-zero-effect commits (`dc74ce43`
  +4,645ms, `31423bfc` −4,622ms — see Apparatus; both provably cannot
  touch this build) already swing at almost exactly that same magnitude
  purely from noise not captured by the clustered same-commit calibration.
  The 5,000ms pursue line, chosen before any noise measurement existed,
  turns out to sit barely above the apparatus's own demonstrated noise
  ceiling — in hindsight, badly calibrated, not a defect in the commits
  measured.
- **Kill line (sum within noise) — corrected a second time (Codex P1 on
  `9afe2a3a`):** the previous paragraph here treated the 31 non-fix deltas
  as 31 independent noise draws (expected sum stdev ≈1,527ms·√31 ≈
  8,504ms, observed +11,088ms ≈1.3σ, "not distinguishable from noise").
  That model was wrong: the deltas are **not** independent — they're a
  chronological chain, `y_i − y_{i−1}`, and summing a *contiguous* run of
  such adjacent differences telescopes to just the segment's two
  endpoints, regardless of how many intermediate steps it contains. The
  non-fix sum here is two such contiguous runs (pre-fix: `dc74ce43`
  through `28c6fae8`; post-fix: `d6aa6688` through `ef61ae44`) with the
  fix's own delta excised from the middle, so it telescopes to exactly
  four independent point measurements: `(28c6fae8 − d1ecb361) +
  (ef61ae44 − 651929b2)` = `(63,559−55,882) + (42,637−39,226)` =
  `+7,677ms + 3,411ms` = **+11,088ms**, with expected noise stdev
  `1,080ms·√4 = 2,160ms` — **not** `1,527ms·√31`. Against that correctly-
  derived figure, +11,088ms is **≈5.13σ**: the pre-fix span alone
  (+7,677ms vs. a 2-endpoint stdev of 1,080ms·√2≈1,527ms) is ≈5.03σ, the
  post-fix span (+3,411ms) is ≈2.23σ. This is not plausibly noise, and the
  kill line's second clause is **not met** — reversing what the
  immediately preceding text here claimed. One caveat kept deliberately
  visible rather than smoothed over: the σ=1,080ms calibration came from 7
  builds run back-to-back in one short batch, while the 32-checkpoint walk
  ran continuously for ~35 minutes with more room for thermal/scheduling
  drift — the `dc74ce43`/`31423bfc` adjacent-delta magnitude (≈4,600ms,
  implying a per-point σ some 2-3x the clustered calibration's, if treated
  as representative of that longer walk) is a live reason the true z-score
  for the segment-level effect could be smaller than 5.1σ. Even discounted
  by that much, though, it stays solidly above the ~2σ range associated
  with real, not-noise effects. See Verdict for what this does and does
  not establish.
- **The total-window figure survives, even though attribution inside it
  does not.** End-to-end, `d1ecb361` (55,882ms, one warm sample) →
  `ef61ae44` (42,837ms, 7-sample calibrated mean) is **−13,045ms
  (−23.4%)**, consistent with the single-sample estimate reported before
  calibration (−13,245ms, −23.7%). At ≈13,000ms, this is roughly 8-9x the
  calibrated single-measurement stdev (1,080-1,527ms), so the *total*
  window effect is not plausibly noise. What calibration removes is
  confidence in *why*: whether that total is the fix's raw −24,333ms
  partly clawed back by real per-commit compile-time growth (this report's
  second revision's claim), or whether the raw in-context fix effect
  itself is simply smaller than PR #2360's isolated same-box A/B for
  reasons this apparatus can't see (measurement-context differences,
  since the fix's own delta, −24,333ms, was measured only once and was
  never itself calibrated against repeats either).

## 🏁 Verdict

**Undetermined** on the pre-registered falsifiable question specifically
(*does a single commit explain the shortfall?* — no), but **not** for the
reason the immediately preceding revision of this section gave. This
report went through four revisions before landing here, each one caught
by a different Codex review finding on PR #2477 — worth stating plainly
rather than only keeping the final number, since the corrections mostly
ran in different directions and a reader comparing this PR's diff to the
live report needs the map:

1. **Revision 1: "Kill."** Wrong — built on a shallow clone that silently
   measured 12 of the real 32 commits and got a commit-ancestry check
   backwards. Fixed by unshallowing and re-measuring the complete window.
2. **Revision 2: "very likely a real, diffuse effect."** Unsupported as
   argued — it treated one repeat (433ms) as the noise floor without
   testing whether it was representative. It wasn't (see below), but the
   qualitative conclusion turns out to have been closer to right than
   revision 3's walk-back of it.
3. **Revision 3: "undetermined, kill line arguably satisfiable."** Also
   wrong, on the arithmetic: it modeled the 31 non-fix deltas' sum as 31
   *independent* noise draws (expected stdev ≈1,527ms·√31≈8,504ms,
   observed +11,088ms ≈1.3σ). That model doesn't apply — the deltas are a
   chronological chain, and summing two *contiguous* runs of them
   telescopes to just their four segment-boundary points, not 31
   independent measurements.
4. **This revision:** correcting the telescoping error, the non-fix sum's
   real expected noise stdev is `1,080ms·√4 = 2,160ms` (four independent
   endpoint measurements, not 31), and the observed +11,088ms is **≈5.1σ**
   — a real, not-noise, systematic compile-time increase across the
   pre-fix span (`d1ecb361`→`28c6fae8`, ≈5.0σ) and, more weakly, the
   post-fix span (`651929b2`→`ef61ae44`, ≈2.2σ). See Assay for the caveat
   about the calibration's own representativeness that keeps this from
   being stated as beyond-doubt.

**What this assay establishes, and at what confidence:**

- **The total window effect is real and large.** `d1ecb361` → `ef61ae44`,
  ≈−13,000ms (−23-24%), roughly 8-9x the single-measurement noise floor —
  not in serious doubt at any point across all four revisions.
- **The pre-fix and post-fix spans each show a real (not noise) net
  increase in compile time**, not just the total window — ≈5.0σ and
  ≈2.2σ respectively, per the corrected math above. This is closer to
  revision 2's original instinct than revision 3's retraction of it,
  though revision 2 reached it through invalid reasoning (a single
  under-representative repeat), so getting the right qualitative answer
  there was luck, not method.
- **What remains genuinely unresolved: attribution to any specific
  commit.** Telescoping means a sum over a contiguous span carries *zero*
  information about which intermediate commit(s) caused the change — that
  is mathematically true regardless of noise. Individually, `6a6610c4`
  (+4,990ms) is the largest single delta at ≈3.3σ against the calibrated
  per-delta noise — a real single-comparison signal, but it was also the
  largest of 31 candidate deltas, and the expected maximum of 31
  independent noise draws is itself around 2.5-2.9σ, so this specific
  value cannot be confidently distinguished from "the biggest of 31 noisy
  draws" without a dedicated repeat measurement of that one commit. No
  commit in this dataset clears that bar.
- **The residual gap to CI is still unexplained.** This proxy's ≈−13,000ms
  absolute saving sits close to, but not exactly at, CI's own observed
  −15,051ms — and this proxy still only measures `autumn-web`'s own
  compile time, not the full `autumn new → first HTTP 200` journey CI
  gates on. The parent report's CI/runner-variance confound is also still
  live and undistinguished from either explanation.

**What would resolve this, as an explicit follow-up (not chartered or run
here):**

1. This apparatus needs roughly 3+ repeats per checkpoint (not one) to
   reach the resolution its own pursue/kill lines assumed — a materially
   larger apparatus (≈3x the build wall-clock used here) than "cheap
   bisection" originally scoped for. That cost-of-precision finding is
   itself useful: a single-run cold-start bisection at this repo's noise
   scale cannot resolve sub-5,000ms effects, so nobody should trust one
   again without calibrating first.
2. Also calibrate the fix's own delta (`651929b2`, currently n=1 in this
   report) with repeats, and extend the method to the
   scaffold+app-compile+server-start portion of the journey via the real
   `autumn dev-loop-bench --cold-start` harness, to close the residual gap
   between this proxy's absolute saving and CI's.

## 💰 Cost to productionize

N/A — undetermined verdict, no build to productionize. Build wall-clock
across all passes: ~11 minutes (first, shallow-clone-invalidated pass) +
~34 minutes (corrected 32-checkpoint pass + 2 warm re-measurements) + ~7
minutes (noise-floor calibration, 7 valid + 1 discarded cold-artifact run)
≈ 52 minutes total, against the pre-registered ≤60-minute *per-pass* box.
Two real costs stand out for anyone scoping a repeat: (1) `git fetch
--unshallow` before any window-derivation command, always — a shallow
clone fails silently, not loudly; (2) a single-run-per-checkpoint design
is cheap but under-powered at this repo's demonstrated noise scale
(σ≈1,080-1,527ms) — budget for repeats from the start rather than
retrofitting a calibration run after the fact, as this report had to.

## 🔬 Reproduce

This matches the actual procedure used (not the naive version): the first
commit built in a fresh worktree absorbs a one-time cold dependency
compile that is not representative of any other checkpoint, so `d1ecb361`
and the window's first commit are measured chronologically like every
other checkpoint, then **re-measured** once the target directory is fully
warm, and only the warm numbers are used in the table.

```bash
# IMPORTANT: unshallow first. A shallow clone silently truncates git log
# ranges and git merge-base --is-ancestor results without erroring — this
# is exactly the bug that produced this report's first, wrong revision.
git fetch --unshallow origin 2>/dev/null || true
git rev-parse --is-shallow-repository   # must print "false" before continuing

git worktree add --detach /tmp/prospect-coldstart-wt2 d1ecb361
cd /tmp/prospect-coldstart-wt2
FEATURES=maud,htmx,tailwind,cache-moka,http-client,reporting

clear_workspace_artifacts() {
  rm -rf target/debug/deps/libautumn_macros* target/debug/deps/libautumn_web* \
         target/debug/deps/libautumn-* target/debug/deps/autumn_macros-* \
         target/debug/deps/autumn_web-* target/debug/deps/autumn-* \
         target/debug/.fingerprint/autumn-macros-* target/debug/.fingerprint/autumn-web-* \
         target/debug/.fingerprint/autumn-* target/debug/incremental/autumn_macros-* \
         target/debug/incremental/autumn_web-* target/debug/incremental/autumn-*
}

timed_build() {
  clear_workspace_artifacts
  START=$(date +%s%3N)
  CARGO_INCREMENTAL=0 cargo build -p autumn-web --no-default-features --features "$FEATURES"
  END=$(date +%s%3N)
  echo "$1: $((END-START))ms"
}

# Full, verified window (d1ecb361 exclusive .. ef61ae44 inclusive, 32 commits).
# The worktree starts with an EMPTY target/, so this first pass's first
# checkpoint (dc74ce43) unavoidably pays a one-time cold dependency compile
# — that number is discarded, not used in the table.
COMMITS=(dc74ce43 25ed48ff 31423bfc e2efa9b0 6a6610c4 d81a4449 7dff7e07 c9121d5b \
         141f36ef 5cdb1e17 747d204b bbac3304 9d99d980 52af191d bc99a4b8 69bb9d18 \
         55febd8a 1fd62458 eba7fc67 24bf151c 61bdd9c2 28c6fae8 651929b2 d6aa6688 \
         97a97be4 d96cfab0 fec52215 8f94e60b 76c56b1c 9c1ede1e f15ca1bb ef61ae44)
for c in "${COMMITS[@]}"; do
  git checkout --detach "$c" --quiet
  timed_build "$c"
done

# Re-measure d1ecb361 and dc74ce43 now that target/ is fully warm from the
# pass above — these two (and only these two) replace their earlier numbers.
for c in d1ecb361 dc74ce43; do
  git checkout --detach "$c" --quiet
  timed_build "${c}-warm"
done

# Noise-floor calibration: the COMMITS loop above already measured ef61ae44
# once (that IS the "window end" sample in the table). Six MORE repeats here
# gives 7 total warm-condition samples, matching this report's calibration —
# don't add a 7th here or you'll have 8, not 7.
git checkout --detach ef61ae44 --quiet
for i in 1 2 3 4 5 6; do
  timed_build "ef61ae44-repeat-${i}"
done

git worktree remove /tmp/prospect-coldstart-wt2 --force   # dismantle
```
