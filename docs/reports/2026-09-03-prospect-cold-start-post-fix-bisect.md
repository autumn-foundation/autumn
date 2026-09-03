# ⛏️ Prospect: which commit ate the missing cold-start savings? (undetermined: the ~13,000ms total window effect is this assay's strongest evidence, but a rigorous noise floor for this apparatus was never established, so no confidence figure — total, span-level, or per-commit — can be honestly stated)

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
account for a material share of the shortfall between the fix's own
confirmed relative saving and the real CI trajectory (**correction, see
below: ~15,051ms is CI's own observed saving, not the gap to explain — but
the actual gap cannot be stated as a precise millisecond or
percentage-point figure either**, for the same reason the parent report
gave: `45,748ms`/`-33%` came from PR #2360's local same-box A/B and
`15,051ms`/`-12.5%` from GitHub-hosted weekly CI runs — different
hardware, different workload distribution — so subtracting them doesn't
produce a real, portable shortfall number, only a directional one: CI
moved *far less* than the confirmed same-box saving)? Or does no single
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

**A third, unrelated error in the pre-registration itself** (Codex P2 on
`e5e6eb56`, corrected further on `39ae17a1` — independent of the
shallow-clone bug above): the pre-registration's own "Pursue-bisection-
further line" and "Kill line" text (blockquoted above, not edited) both
call `~15,051ms` "the CI shortfall," and this report's Question section
repeated that framing. `15,051ms` (`120,422−105,371`) is CI's own
**observed saving**, not the gap between what was confirmed and what CI
delivered — that part of the finding stands. What does **not** stand is
this report's own first attempt at a fix, which replaced it with an
equally-precise-sounding `~30,697ms`/`~21.0 percentage points` by directly
subtracting PR #2360's local same-box A/B from CI's weekly numbers — the
exact kind of cross-hardware precision the parent report deliberately
avoided, for the same reason: `45,748ms` and `15,051ms` come from
different runners with a different workload distribution, so their
difference isn't a real, portable "shortfall" figure any more than
`15,051ms` alone was. Corrected a second time in the Question section to
state this directionally, matching the parent report's own established
caution, rather than trading one false precision for another. This does
**not** change the pursue/kill lines themselves (the 5,000ms threshold was
set independently), so it doesn't retroactively invalidate any verdict in
this report.

**A fourth, more consequential limitation** (Codex P1 on `39ae17a1`): this
report's entire apparatus reuses one warm `target/` directory across the
whole chronological walk, clearing only workspace-crate artifacts between
checkpoints — a deliberate design choice (see Apparatus) to keep external
dependency compiles warm, matching how CI's `Swatinem/rust-cache` persists
*something* across weekly runs. But per `autumn-cli/src/cold_start_driver.rs`
(`compile_cold`, confirmed by reading it directly) and
`.github/workflows/cold-start-latency.yml`, what the CI gate actually
measures is different in kind, not just in what it additionally includes:
**every single CI sample scaffolds a brand-new temp project with its own
empty `target/`, explicitly removes `CARGO_TARGET_DIR`, and disables
compiler wrappers — so CI pays the full compile cost of every external
dependency, from scratch, on every sample, forever.** This report's
warm-target apparatus pays that cost **at most once**, on whichever
checkpoint first introduces a given dependency — after that, it's cached
and invisible in every later delta, even though CI keeps paying it every
week. `git log --name-only -- Cargo.lock` over this window flags **4 of
the 31 non-fix commits as touching `Cargo.lock`** — but "touches the
file" is a crude proxy for "changes *this exact feature set's* resolved
graph," and checking each one against `cargo tree -p autumn-web
--no-default-features --features
maud,htmx,tailwind,cache-moka,http-client,reporting` (Codex P2 on
`d2a980c3`, verified independently rather than taken on trust — see the
per-commit correction below) changes the list:

- `fec52215` (zero-downtime state migration, this report's second-largest
  post-fix delta at +4,591ms) is a **confirmed false positive**: its only
  `Cargo.lock` change adds the unrelated `hot-upgrade` example/workspace
  package, which is absent from this feature set's resolved tree. Its
  delta should **not** be discounted on this basis — if it's real, the
  cause is something else (or noise, per the significance discussion
  above).
- `141f36ef` (a `base64` patch-version bump) — **retracted, a third
  confirmed false positive** (Codex P2 on `cc91a56d`): the coexistence of
  `base64 v0.22.1` and `v0.23.1` I cited as evidence isn't caused by this
  commit — checking both `141f36ef^:Cargo.lock` and `141f36ef:Cargo.lock`
  package-by-package (which package's `dependencies` list names which
  `base64` version) shows `bcrypt` (an unconditional, direct dependency
  of `autumn-web` — confirmed via `cargo tree -i bcrypt`) already
  depended on `base64 0.23.1` **before** this commit, while `axum`,
  `reqwest`, and everything else kept depending on `0.22.1` **after** it
  unchanged. `141f36ef` only re-points `autumn-web`'s *own* direct edge
  from `0.22.1` to `0.23.1` — a version that was already being compiled
  for `bcrypt`'s sake regardless. No new compiled artifact, before or
  after.
- `61bdd9c2` (Web Push) — **confirmed real**, the one commit that
  survives full scrutiny: its actual `Cargo.lock` diff is a single new
  line, `autumn-web` gaining a direct dependency edge to `p256 0.13.2` — a
  version already resolved (and already compiled) via `jsonwebtoken`'s
  transitive dependency on it, so the crate itself isn't new. What is new
  is the *feature set* Cargo resolves for it: the commit's own message
  states it adds `p256`'s `ecdh` feature flag on that new direct edge,
  and Cargo unifies features across every path to a shared dependency
  within one build — so `p256` is compiled once, under the union of
  features every consumer requests, meaning this edge's new feature
  forces a real (if likely modest) recompile of `p256` that wouldn't
  happen otherwise.
- `bc99a4b8` (scaffold DSL constraints) — **retracted, a second confirmed
  false positive** (Codex P2 on `abea7490`, correcting my own prior
  verification, which was sloppy: I saw `reqwest` and `serde_urlencoded`
  both appear somewhere in the tree and stopped there, without checking
  *which* `reqwest`). The exact feature set this report times resolves
  `reqwest v0.12.28` (confirmed: `cargo tree -p autumn-web ... -i
  "reqwest@0.12.28"`); `bc99a4b8^:Cargo.lock` (the parent commit, before
  this change) shows `reqwest 0.12.28` **already** listed
  `serde_urlencoded` as a dependency — it was never added by this commit.
  What `bc99a4b8` actually adds `serde_urlencoded` to is `reqwest
  v0.13.4`, a wholly different resolved version pulled in only by
  `autumn-cli`'s dev-dependencies, never built by `cargo build -p
  autumn-web`. Compounding that: `autumn/Cargo.toml` (`autumn-web`'s own
  manifest) already lists `serde_urlencoded = "0.7"` as a **direct**
  dependency regardless of `reqwest` at all. None of that is touched by
  `bc99a4b8`. My first correction (`abea7490`) was itself wrong to keep
  this flagged — reversed here.

Net, after three rounds of verification against `Cargo.lock`: **1 of the
4** originally-flagged commits (`61bdd9c2`) is confirmed relevant to this
feature set's resolved graph; `fec52215`, `bc99a4b8`, and `141f36ef` are
confirmed false positives.

**But the audit's scope was itself wrong** (Codex P2 on `a54423f1`,
found not by rechecking a candidate but by pointing out an entire
*category* of change this report never looked for): `Cargo.lock` records
resolved *versions*, not enabled *features* — a workspace `Cargo.toml`
edit that changes which Cargo features a dependency compiles with never
touches `Cargo.lock` at all, yet triggers exactly the same warm-cache
bias this section is about. Checked directly: `9d99d980` changes the
workspace's `syn = { features = [...] }` declaration from `["full"]` to
`["full", "visit-mut"]`, and `9c1ede1e` extends it again to `["full",
"visit", "visit-mut"]` — confirmed via `git show <sha> -- Cargo.toml` on
both. `autumn-macros` declares `syn.workspace = true` (confirmed:
`autumn-macros/Cargo.toml`) and is compiled by the timed command, so both
commits force a real `syn` recompile under a newly-expanded feature set —
two more confirmed cases, found by a completely different method than
the `Cargo.lock` audit above, which missed both.

`git log --name-only -- '**/Cargo.toml' Cargo.toml` over the window shows
roughly 9 of the 31 non-fix commits touch *some* `Cargo.toml` (not just
the 4 that touched `Cargo.lock`) — but, as the `bc99a4b8` and `141f36ef`
retractions above already demonstrated, "touches the manifest" is no more
reliable a proxy than "touches the lockfile" was; each one needs the same
package-by-package, before/after verification those two retractions
required, which is not a small check. **Auditing all ~9 is out of this
session's remaining time box.** The honest position is not "1 of 4" or
even "3 of 31" — it is: **at least 3 non-fix commits in this window
(`61bdd9c2`, `9d99d980`, `9c1ede1e`) are confirmed to trigger this
apparatus's warm-cache bias, the true count is unknown and could be
higher, and no exhaustive accounting was completed.** For every commit
this bias touches, confirmed or not yet checked, this report's delta
cannot be trusted as representative of its true, CI-relevant recurring
contribution — not for a purely one-directional reason (see the next
correction). This uncertainty compounds with, rather than replaces, the
open noise-floor question in Assay: individual per-commit deltas in this
dataset are less interpretable on *two* independent axes now, both
pointing the same way — toward the undetermined verdict, not away from
it. It
also means the "total window effect" and "this proxy's absolute saving vs.
CI's" comparisons throughout Assay and Verdict are weaker than stated
there: they are not just measuring a narrower workload (library-only, no
scaffold/server-start) as already disclosed, but a workload measured under
a fundamentally warmer caching regime than CI ever runs under.

**The bias is not one-directional** (Codex P2 on `abea7490`, correcting
the "can only undercount, never overcount" claim this section originally
made): when a checkpoint upgrades or newly introduces a dependency, this
apparatus's warm-target delta charges that checkpoint the *entire* fresh
compile cost of the new/changed dependency, while the *previous*
checkpoint paid nothing for it (it wasn't cached yet, or didn't exist).
A true cold-vs-cold comparison — what CI actually runs — would instead
show the much smaller *marginal* difference between compiling the old
version and the new one, since both a fully-cold pre- and post-commit
build pay for *some* version of that dependency. So at the introducing
checkpoint specifically, this apparatus likely **overcounts** the
commit's true CI-relevant marginal cost; at every checkpoint *after* it,
the dependency is cached and the apparatus shows nothing for it, which
**undercounts** the recurring cost CI keeps paying. Both directions are
real, and this report cannot say which dominates for `61bdd9c2`
specifically (the one commit confirmed graph-relevant above — `141f36ef`
is a confirmed false positive with no compiled artifact change either
way, so this caveat doesn't apply to it) without a genuinely
cold-per-checkpoint rerun. Practically, this makes individual per-commit
deltas in this dataset
*less* interpretable than the "undercount only" framing suggested, not
more — reinforcing the undetermined verdict, not weakening it. Not
corrected by re-running (out of this session's time box — a true
apples-to-apples repeat would need an empty `target/` per checkpoint,
i.e. 32+ fully cold builds, each potentially 60-120s, a materially larger
apparatus); flagged here as a limitation this report cannot resolve, and
carried into the Verdict.

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
- **A seventh limitation, also confirmed (Codex P2 on `269afeb8`): the
  timed feature set isn't an exact match for what CI's own scaffold
  builds, either.** `DAEMON_NO_DB_FEATURES` (this report's feature list)
  omits `flash`, and `autumn-cli/src/new.rs` writes the generated app's
  `autumn-web` dependency line with `default-features = false, features =
  [DAEMON_NO_DB_FEATURES]` accordingly — but the generated scaffold's
  *own* `Cargo.toml` (`autumn-cli/src/templates/Cargo.toml.tmpl`)
  separately declares `default = ["flash"]` / `flash =
  ["autumn-web/flash"]`, and `cold_start_driver.rs`'s `cold_build` runs a
  bare `cargo build` with no `--no-default-features` flag of its own.
  Cargo feature unification then turns `autumn-web/flash` on for the real
  scaffold build regardless of the dependency line's explicit feature
  list. This report's exact command (`--features
  maud,htmx,tailwind,cache-moka,http-client,reporting`) never enables
  `flash`, so it measures a library configuration close to, but not
  identical with, what CI actually compiles. Not corrected by re-running
  (compounds with, doesn't replace, the narrower-workload and
  warm-vs-cold-target limitations already disclosed).

## 📊 Assay

**Environment:** detached git worktree (`/tmp/prospect-coldstart-wt2`,
outside the tracked tree, unshallowed), same sandbox as the parent assay
(4-core, 15GiB, rustc/cargo 1.94.1). One shared `target/` reused across
every checkpoint (external deps stay warm); workspace-local crate
fingerprints and incremental artifacts cleared before every timed build;
`CARGO_INCREMENTAL=0`. Command: `cargo build -p autumn-web
--no-default-features --features maud,htmx,tailwind,cache-moka,http-client,reporting`.
One run per checkpoint, chronological order, plus a same-commit repeat
built into that same pass. **Build count, corrected (Codex P2 on
`480f7ec5`):** this pass ran 35 builds — 32 chronological checkpoints
(`dc74ce43` through `ef61ae44`) + 1 same-commit repeat of `ef61ae44` built
into that same run + 2 warm re-measurements of `d1ecb361`/`dc74ce43` — not
34 as an earlier revision stated (simple arithmetic error: 32+2+1=35). The
dedicated noise-floor calibration below is a **separate** pass of 6 more
builds (5 valid + 1 discarded cold-artifact), run afterward. Its reported
"7 valid samples" are **not** 6 new plus something else — they are the 5
valid new builds from that separate pass **plus 2 samples already produced
by this 35-build pass** (the `ef61ae44` chronological-checkpoint value,
42,637ms, and this pass's own built-in `ef61ae44` repeat, 42,204ms). Total
distinct builds across both passes: 35 + 6 = 41, all exited 0 (the
discarded cold-artifact build succeeded; it's excluded from analysis as
non-representative, not because it failed).

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
  derived figure, +11,088ms is **≈5.13σ *if* the calibration's σ=1,080ms
  represents the noise present throughout the whole 32-checkpoint walk**:
  the pre-fix span alone (+7,677ms vs. a 2-endpoint stdev of
  1,080ms·√2≈1,527ms) would be ≈5.03σ, the post-fix span (+3,411ms) ≈2.23σ.
  **That premise doesn't hold, and carrying the correction through changes
  the answer, not just the caveat** (Codex P1 on `480f7ec5`, correcting
  the previous sentence here, which raised this same caveat and then
  wrongly asserted it didn't matter): the calibration's 7 builds ran
  back-to-back in one short batch, while the 32-checkpoint walk ran
  continuously for ~35 minutes with more room for thermal/scheduling
  drift, and `dc74ce43`/`31423bfc`'s adjacent-delta magnitude (≈4,600ms)
  implies a per-point σ during that longer walk perhaps 2-3x the clustered
  calibration's. Carrying that multiplier through, rather than gesturing
  at it: at 2x, pre-fix drops to ≈2.51σ and post-fix to ≈1.12σ (combined
  ≈2.57σ); at 3x, pre-fix ≈1.68σ, post-fix ≈0.74σ (combined ≈1.71σ). At
  the upper end of the range this report's own evidence supports, **the
  post-fix span is not distinguishable from noise, and even the combined
  figure is only weak evidence, not the ≈5σ headline number.** The honest
  range is ≈1.7σ-5.1σ combined, ≈0.7σ-2.2σ for the post-fix span alone —
  and this assay has no way to determine, from the data collected, which
  end of that range is closer to true. Resolving it needs noise
  calibration interspersed *throughout* a long chronological walk, not a
  clustered batch run separately from it — not done here. See Verdict.
- **The total-window figure is this report's single strongest number, but
  it is not immune from the same open noise question as everything
  else — a claim that it was "solidly real at either end" of a 4.0-5.9σ
  range overstated what's actually established** (Codex P2 on `1c785d56`,
  correcting the previous revision's own correction). End-to-end,
  `d1ecb361` (55,882ms, one warm sample from the 32-checkpoint walk) →
  `ef61ae44` (42,837ms, 7-sample calibrated mean) is **−13,045ms
  (−23.4%)**, consistent with the single-sample estimate reported before
  calibration (−13,245ms, −23.7%). Using the clustered calibration's
  σ=1,080ms at face value, that's ≈8-9σ. Scaling that by the 2-3x range
  used elsewhere in this report gives ≈4.0-5.9σ — but that 2-3x figure
  itself rests on exactly two single-run no-op deltas (`dc74ce43`,
  `31423bfc`, ±4.6s), which can suggest a point estimate but cannot
  establish a rigorous *upper bound* on how noisy the 32-checkpoint walk
  really was, especially with only one `d1ecb361` sample to begin with.
  Nothing in this report rules out the walk's true noise being higher
  than 3x the clustered estimate — in which case even 4.0σ is not a safe
  floor. The honest statement is not "≈4.0-5.9σ, solidly real" but: **the
  applicable noise scale for this comparison is not established by this
  data, the same as the pre-fix and post-fix spans** — what distinguishes
  this number from those is only that its raw effect size (≈13,000ms) is
  roughly 3-4x theirs, so it would take a noise level considerably higher
  than anything this report's evidence suggests to put it in real doubt.
  That is a qualitative reason for somewhat more confidence, not a
  quantitative one, and this report cannot honestly produce a specific σ
  figure for it. What calibration
  removes is
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
4. **Revision 4: corrected the telescoping error, claimed ≈5.1σ, "real,
   not noise."** Half right: the telescoping correction itself
   (`1,080ms·√4 = 2,160ms`, not `1,527ms·√31`) is sound and stands. But
   that revision raised its own caveat about the calibration's
   representativeness over a longer, less-controlled walk, then asserted
   without checking that the conclusion "stays solidly above the ~2σ
   range" regardless — it didn't verify that.
5. **This revision:** carrying that caveat's own numbers through (Codex
   P1 on `480f7ec5`) changes the answer. At a 2-3x noise inflation — the
   range the `dc74ce43`/`31423bfc` evidence implies for the full walk —
   the combined significance ranges **≈1.7σ-5.1σ**, and the post-fix span
   alone ranges **≈0.7σ-2.2σ**, i.e. *not* reliably distinguishable from
   noise at the pessimistic end. See Assay for the numbers at each
   multiplier.

**What this assay establishes, and at what confidence:**

- **The total window effect is this report's strongest single number, but
  it is subject to the same unresolved noise-scale question as the spans
  below, not exempt from it** (corrected twice now, most recently Codex
  P2 on `1c785d56`: a prior revision computed a ≈4.0-5.9σ "floor" using a
  2-3x noise-inflation range, then asserted that floor as solid — but
  that 2-3x range itself rests on only two single-run no-op observations
  and doesn't establish a rigorous upper bound on the walk's true noise).
  `d1ecb361` → `ef61ae44`, ≈−13,000ms (−23-24%), is ≈8-9σ against the
  clustered calibration's tight noise estimate; this report cannot
  honestly state a specific σ figure beyond that, only that the effect
  size is roughly 3-4x larger than the spans below, so it would take a
  noise level well outside what any evidence here suggests to put it in
  real doubt. That is qualitative grounds for more confidence than the
  spans below warrant, not a quantitative guarantee.
- **Whether the pre-fix and post-fix spans individually show real,
  not-noise compile-time growth is genuinely unresolved, not just
  unattributed.** Under the calibration's own tight noise estimate
  (σ=1,080ms from a clustered batch), both spans read as real (≈5.0σ
  pre-fix, ≈2.2σ post-fix). Under the noise scale the report's own
  `dc74ce43`/`31423bfc` evidence implies for a long, less-controlled walk
  (2-3x higher), the post-fix span is not distinguishable from noise and
  the pre-fix span's significance drops substantially too. This assay
  cannot determine which noise estimate is closer to true — that needs
  noise calibration interspersed *throughout* a long walk, not a
  clustered batch run separately from it (not done here). Revision 2's
  "very likely real" framing was closer to the optimistic end of this
  range than revision 3's "not distinguishable from noise" was to the
  pessimistic end, but neither revision had the honest range in hand when
  it was written.
- **What remains genuinely unresolved regardless: attribution to any
  specific commit — but telescoping is not the reason for that, and
  claiming it was is a mistake this section made** (Codex P2 on
  `1c785d56`, correcting the previous paragraph's own logic, not just its
  numbers). Telescoping only nullifies information in a *sum* — it has
  nothing to say about the individual per-checkpoint deltas already
  sitting in the Assay table, each a standalone two-point measurement.
  `6a6610c4` (+4,990ms) is exactly such a measurement, and its own
  `Cargo.toml`/`Cargo.lock` diff touches nothing but `autumn/src/search.rs`
  — no dependency-cache caveat applies to it at all. The real reason it
  (and every other individual delta in the table) can't be confidently
  named as a cause is the **noise/calibration problem already established
  above**: this apparatus took one measurement per checkpoint, and the
  true noise floor for a single such measurement is itself uncertain
  (tight clustered calibration says σ≈1,080ms; the walk itself may run
  noisier). Against the tight estimate, `6a6610c4`'s ≈3.3σ is a real
  single-comparison signal, sitting *above*, not within, the expected
  one-sided maximum of 31 independent draws (a simulation, 200k trials,
  n=31 standard normal: E[max]≈2.06σ, E[max abs]≈2.34σ, `P(max≥3.3σ)`≈1.5%
  — correcting an earlier, wrong ≈2.5-2.9σ claim here, which was closer to
  a ~95th-percentile value than an expectation). That ≈1.5% tail
  probability is suggestive, not dispositive, and — as the total-window
  discussion below now also states plainly — this report never
  established a reliable upper bound on the walk's true noise scale, so
  even this single-comparison signal cannot be confidently separated from
  noise. Telescoping is the correct, load-bearing reason the pre-fix and
  post-fix *aggregate* sums (+7,677ms across 22 commits, +3,411ms across
  9) carry no information about which commit(s) within each span caused
  the shift; the noise/calibration problem is the separate, load-bearing
  reason no *individual* delta — including `6a6610c4`'s — can be
  confidently named either. Both apply; neither is the other.
- **This proxy's ≈−13,000ms is not directly comparable to CI's −15,051ms
  at all, and this report previously overstated how close a match that
  was** (Codex P1 on `39ae17a1`). Two compounding reasons, not one: this
  proxy measures a narrower workload (`autumn-web`'s own compile time,
  not the full `autumn new → first HTTP 200` journey), already disclosed
  — but more fundamentally, it measures that workload under a **warm**
  shared `target/` directory reused across the whole chronological walk,
  while CI's `cold_start_driver.rs` scaffolds a brand-new project with an
  **empty** `target/` and no compiler-wrapper cache on every single
  sample (see Apparatus's fourth correction). This apparatus pays each
  external dependency's compile cost once per introduction; CI pays it on
  every sample, forever. Auditing this by "touched `Cargo.lock`" alone
  (the original approach) found only **1** of the 4 candidates confirmed
  real (`61bdd9c2`; `fec52215`, `bc99a4b8`, `141f36ef` all confirmed
  false positives after three separate rounds of package-by-package
  verification) — but `Cargo.lock` doesn't record *enabled features*,
  only resolved versions, so that audit was itself scoped too narrowly.
  A workspace `Cargo.toml` feature-flag change is invisible to it and
  triggers the identical bias: confirmed directly for `9d99d980` and
  `9c1ede1e` (both change the workspace's `syn` feature set;
  `autumn-macros` inherits it via `syn.workspace = true` and is compiled
  by this build). **At least 3 non-fix commits in this window are
  confirmed to trigger this bias** (`61bdd9c2`, `9d99d980`, `9c1ede1e`);
  roughly 9 of the 31 touch *some* `Cargo.toml`, each needing the same
  depth of verification the retractions above required, which a full
  audit of all of them was not completed in this session's time box — so
  the true count is unknown and plausibly higher. Every affected commit's
  delta cannot be trusted as CI-representative, but not in a single,
  simple direction: the apparatus likely *overcounts* its cost at its own
  introducing checkpoint (charging the full fresh-compile cost rather
  than CI's much smaller cold-vs-cold marginal difference) and
  *undercounts* it at every checkpoint after (nothing shown where CI pays
  every sample). Which effect dominates for this window's total is not
  resolvable from this data. The parent report's CI/runner-variance
  confound (1-sample local A/B vs. 3-sample CI p95, different runners) is
  also still live and undistinguished from any
  of this.

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
   `autumn dev-loop-bench --cold-start` harness.
3. **Use an empty `target/` per checkpoint, matching CI's own
   `cold_start_driver.rs` methodology, not a warm shared one** — the
   single biggest change needed before this proxy's numbers can be
   compared to CI's at all. This is a materially larger apparatus (32+
   fully cold builds instead of one warm chain; each of the confirmed
   cold-build samples in this report took 60-120s, so a fully-cold
   32-checkpoint pass could run 30-60+ minutes on its own) — but without
   it, any commit that actually changes this feature set's resolved
   dependency graph (not merely "touches `Cargo.lock`" — confirmed real
   for only 3 of the ~13 candidates checked, `61bdd9c2`/`9d99d980`/
   `9c1ede1e`, with several more `Cargo.toml`-touching commits unchecked;
   see Apparatus) cannot be trusted, and neither can this proxy's
   total-window comparison to CI's absolute saving.

## 💰 Cost to productionize

N/A — undetermined verdict, no build to productionize. Build wall-clock
across all passes: ~11 minutes (first, shallow-clone-invalidated pass) +
~34 minutes (main 35-build pass: 32 chronological checkpoints + 1 in-pass
repeat + 2 warm re-measurements) + ~7 minutes (separate noise-floor pass:
6 builds, 5 valid + 1 discarded cold-artifact, contributing 5 of the
calibration's 7 total samples — the other 2 come from the main pass) ≈ 52
minutes total, against the pre-registered ≤60-minute *per-pass* box.
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
set -euo pipefail   # fail loudly, don't silently build the wrong commit

# IMPORTANT: unshallow first, and treat failure as fatal. A shallow clone
# silently truncates git log ranges and git merge-base --is-ancestor
# results without erroring — this is exactly the bug that produced this
# report's first, wrong revision. `--unshallow` itself errors (exit 128,
# "does not make sense") on an already-complete clone, so only run it when
# actually shallow — but still verify and fail loudly afterward either way.
if [ "$(git rev-parse --is-shallow-repository)" = "true" ]; then
  git fetch --unshallow origin
fi
if [ "$(git rev-parse --is-shallow-repository)" != "false" ]; then
  echo "FATAL: repository is still shallow after fetch --unshallow" >&2
  exit 1
fi

REPO_ROOT=$(pwd)   # so we can cd back out before removing a worktree
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

# This same pass also includes one same-commit repeat of the final
# checkpoint, taken immediately (still in this same warm worktree) —
# this is the 35th build of the main pass, and one of the 7 calibration
# samples (see the separate calibration pass below).
timed_build "ef61ae44-repeat-in-pass"

# Re-measure d1ecb361 and dc74ce43 now that target/ is fully warm from the
# pass above — these two (and only these two) replace their earlier numbers.
# Main pass total: 32 + 1 + 2 = 35 builds.
for c in d1ecb361 dc74ce43; do
  git checkout --detach "$c" --quiet
  timed_build "${c}-warm"
done

cd "$REPO_ROOT"   # must leave the worktree before removing it, or the next
                  # `git worktree add` fails with "Unable to read current
                  # working directory"
git worktree remove /tmp/prospect-coldstart-wt2 --force

# Noise-floor calibration is a SEPARATE pass, in a fresh worktree (empty
# target/, same cold-build artifact as the main pass's first checkpoint —
# that's why run 1 below is expected to be an outlier and gets discarded,
# not because anything is wrong with it). 6 builds here + the 2 ef61ae44
# samples already produced by the main pass above (the "window end"
# checkpoint and the in-pass repeat) = 7 total calibration samples,
# matching this report's numbers.
git worktree add --detach /tmp/prospect-noise-wt ef61ae44
cd /tmp/prospect-noise-wt
for i in 1 2 3 4 5 6; do
  timed_build "ef61ae44-calibration-${i}"   # run 1 is the expected cold outlier — discard it
done
cd "$REPO_ROOT"
git worktree remove /tmp/prospect-noise-wt --force
```
