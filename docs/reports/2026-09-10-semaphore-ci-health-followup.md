# 🚦 Semaphore: CI health follow-up — the `live_upgrade` fix landed, the contention harness is still undispatched

Follow-up to `docs/reports/2026-09-09-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR from
this pass — someone else already shipped the fix this ledger has been
tracking evidence toward, and the hard gate for a *different* fix (verifying
it, closing the gap on the one unattributed signature below, or fixing
`cache_stampede`) still isn't cleared. The headline: PR #2645, merged onto
`trunk-dev` at `8fae8af` (2026-09-10T04:56:32Z), fixed three named timing
races in the `live_upgrade` test itself — test-defect, not product-defect,
per its own local contention-reproduction evidence. Of the three tracked
`live_upgrade` signatures, #2645's commit message credibly explains **one**
(the Linux "new build never served" hit, mechanism 2) — **correction
(post-review): not two**, as an earlier version of this paragraph claimed.
The macOS connect-error cluster is tied to a different, earlier fix (#2510,
per the ledger's own pre-existing text); a third signature (below) does not
match any of #2645's three named predicates and remains unattributed to any
fix at all. None of this yet clears this role's own closure bar (CI-native
≥20/≥50 same-commit rerun evidence), because the verification harness built
for exactly this purpose is still sitting undispatched — and, as corrected
below, only covers part of what needs verifying even once it runs.

## 🎯 Verdict path

`trunk-dev`'s tip (`8fae8af`) is green; the push-triggered `ci.yml` run for
the merge that landed the fix (34433144461) succeeded. `manual-macos-
contention-check.yml` — `actionlint`-clean and dispatchable since #2627
(merged 2026-09-08T15:07:44Z) — **still has zero `workflow_dispatch` runs**
(`total_count: 0`, checked 2026-09-10T09:5x UTC, roughly 43 hours after it
became dispatchable). That number has not moved across three consecutive
daily passes (2026-09-08, 2026-09-09, 2026-09-10) despite the investigation
it exists to serve reaching a diagnosed-and-fixed mechanism in the meantime.

**Correction (post-review): this harness does not cover both platforms.**
An earlier version of this report claimed dispatching it would verify the
fix "CI-natively across both platforms in one dispatch." Checked against
`.github/workflows/manual-macos-contention-check.yml` itself rather than
asserted from memory of its name: its `test` job is
`runs-on: macos-latest` only, running a plain `cargo test --workspace` (no
Linux leg, no `cargo llvm-cov` instrumentation). It can produce CI-native
rerun evidence for the macOS-observed connect-error cluster, and — per the
fix's own in-code comment on mechanism 2 (`live_upgrade.rs:597-603`: the
window can still expire "under an instrumented coverage binary … or a
contended CI runner") — plausibly for mechanism 2 under ordinary contention
too, without `cargo llvm-cov` necessarily being required. What it cannot do
is reproduce the Linux/`Coverage (workspace)` job shape either tracked
Linux signature was actually observed under, since this harness only ever
runs macOS VMs — a platform gap, not necessarily an instrumentation one;
whether `cargo llvm-cov` is strictly required for either Linux signature to
manifest, versus plain contention on a Linux runner sufficing, remains
untested. Dispatching it is still
worth doing (it is the only rerun harness that exists for this
investigation today, and a clean run narrows the macOS cluster), but it is
not the full verification this report originally implied — the 2026-09-09
follow-up already flagged a second, Linux/coverage-shaped harness as a
future need; this pass should not have re-conflated the two.

## 🌡️ Symptom

Sampled the 86 `pull_request`-triggered `ci.yml` runs completed since the
2026-09-09 follow-up's cutoff (2026-09-09T09:47:12Z–2026-09-10T09:48:19Z):
55 cancelled (superseded pushes, not a health signal), 23 success, **8
failure**. Triaged each by job/log inspection:

- **6 are ordinary WIP-branch failures**, not CI health issues: `Clippy`
  failures across four `codex/*` branches' in-progress commits (runs
  34433194077, 34413188863, 34413165387, 34411840985 — three of these are
  the same `Clippy` step failing on not-yet-fixed lint issues in new code;
  34411840985 also failed `Supply chain (cargo-deny)`'s `Dependency
  advisory gate` in the same run, a second WIP finding not a second run),
  a `Migration guide coverage`/`Guide reachability gate` failure on
  `codex/add-documentation-for-confidential-fields-and-threats` (run
  34411864694), and the same known `dependabot/cargo/validator-0.21.0`
  `cargo-deny` failure already logged in the 2026-09-09 pass (run
  34394862903, new commit on the same branch — the upstream `validator`
  release still needs code changes this branch doesn't have).
- **1 new, unrelated single-occurrence signature, logged for recognition
  only**: run 34406822202 (`codex/plan-and-fix-model-registration-issue`)
  failed `Test (windows-latest)` at the build-linking stage, not in any
  test body: `LINK : fatal error LNK1104: cannot open file
  '...\target\debug\deps\seed.exe'` while building the `todo-app` example's
  `seed` binary. LNK1104 "cannot open file" on Windows is characteristically
  a file-lock race (antivirus scanning the just-written `.exe`, or another
  process transiently holding it) rather than a code defect — but this is
  n=1, no rerun, no signature match against any prior entry in this ledger.
  Not quarantined, not campaigned — noted here only so a repeat is
  recognized rather than rediscovered from scratch, per this role's own
  standard for a first hit.
- **1 is the `live_upgrade` hit fully written up in the ledger update
  below** (run 34360601529, job `Coverage (workspace)`, 2026-09-09T13:59Z) —
  a third distinct assertion/signature on the same test, on the same
  Linux/coverage job shape as the prior day's line-567 hit. **Correction
  (post-review):** an earlier version of this report claimed this hit was
  "retroactively explained by one of the three mechanisms PR #2645 fixed."
  Checked against the merged source rather than asserted from the log
  alone, it is not: the fix's mechanism 3 (`is_startup_barrier_response`)
  only retries an exact HTTP 503 with body `"Service is still starting
  up"`, and this hit's observations are `status: 0` with an empty body —
  not an HTTP 503 response either way — so that predicate does not match
  and the retry would not have fired. (A second correction on review: don't
  read `status: 0` as proof of "no response received" / a connection-level
  failure — `get()`'s source shows it's also what an `Ok` response gets
  assigned whenever the reply can't be parsed into a status line, which a
  malformed or truncated non-empty reply could produce just as well as an
  empty one; call it unparseable/unknown, not connection-level.) This
  signature stays unattributed to any of the three named mechanisms; see
  the ledger entry for the full correction.
- **1 test-suite failure gate closing failed jobs already counted above**
  (run 34413188863's `Test suite` gate reports failure because its own
  `Clippy`/`SQLite runtime` jobs failed — not a distinct failure, not
  double-counted; `Test suite` is a shard-result aggregator, not an
  independent test run, in every run sampled this pass).

No new `cache_stampede` or `sim_fault_plan` hits this pass.

## 🔍 Diagnosis

**`live_upgrade`: verdict rendered for three named mechanisms — test defect,
not product defect. Whether those three mechanisms cover every organic hit
this ledger has logged is a separate, still-open question**, per the
correction above. Full mechanism writeup copied into
`docs/ci-health/quarantine-ledger.md`'s `live_upgrade` entry rather than
duplicated here. In short: two of the three races involve a request landing
on a startup barrier (the seed request racing v1's own barrier; a cutover
read racing the successor's barrier) that the test treated as an
unconditional hard failure instead of a bounded-retry case the way it
already treated a mid-flight connection reset; the third is a fixed 3.5s
post-cutover window with no budget behind the number, replaced with an
adaptive wait bounded to the same 30s budget used elsewhere in this test
file. All three are textbook "fixed sleep/assumption about environment
speed, breaks under contention or an instrumented binary" — exactly the
`sleep()`-as-synchronization anti-pattern this role's charter names, except
here it's diagnosed and replaced with condition-polling rather than a wider
timeout, which is the sanctioned direction. The 2026-09-09T13:59Z hit's
`status: 0` observations don't fit any of the three predicates as written
(see the correction above) — either a fourth, still-unnamed mechanism
remains, or mechanisms 1/2 happen to resolve it as a side effect despite not
naming it directly. Only a rerun shows which.

The fix's own evidence (9+ consecutive local, targeted `cargo llvm-cov -p
hot-upgrade --test live_upgrade` runs across two contention levels) is real
diagnostic and directional confirmation, but it is self-reported, local,
and — **correction (post-review)** — not the same build as CI's own
coverage lane: `ci.yml`'s "Generate coverage (workspace catch-all)" step
runs `cargo llvm-cov --workspace --exclude autumn-web --exclude autumn-cli
--all-features --no-report`, a full-workspace all-features build, not the
single-package targeted command the fix's commit message quotes. This
role's own bar for treating a ledger entry as *closed* — not merely
*diagnosed and fixed* — is ≥20 (≥50 for the macOS cluster's historically
sub-10% rate) 0-failure reruns from a harness anyone can rerun, against
the actual CI build. `manual-macos-contention-check.yml` was built toward
that for the macOS side and still hasn't been dispatched even once — and
even a full dispatch only reaches 20 samples (its `samples` input caps at
`"20"`), short of the ≥50 bar on its own; no harness at all exists yet for
either Linux/coverage signature.

**`cache_stampede` and `sim_fault_plan`: unchanged, still undiagnosed.** No
new hits this pass; still short of a rerun campaign.

**Windows LNK1104: unclassified, n=1.** Not enough to diagnose a mechanism
(infra file-lock race vs. something else) from a single occurrence with no
rerun.

## 🔧 Treatment

None shipped by this pass — the mechanism this investigation has been
building toward was fixed by someone else's PR before this pass started, and
opening a duplicate fix would be pure waste. What this pass does instead:

- **`docs/ci-health/quarantine-ledger.md` updated**: the third `live_upgrade`
  signature (2026-09-09T13:59Z hit) written up in full, PR #2645's fix
  recorded against the two mechanisms it credibly explains, the third left
  explicitly unattributed after checking the predicate against the merged
  source, and the entry kept **open** (not moved to Closed) pending
  CI-native verification — diagnosed-and-fixed is not the same claim as
  closed-per-this-role's-bar, and conflating them is exactly the "merged is
  not the same as verified" trap the 2026-09-09 report already called out
  once for a different PR (#2510) in this same investigation.
- **Recommendation for a human, still more urgent than the prior two
  passes, but narrower than this report first claimed**: dispatch
  `manual-macos-contention-check.yml` against a `trunk-dev` commit at or
  after `8fae8af`. It is macOS-only, plain `cargo test --workspace` — it
  can produce CI-native rerun evidence for the macOS connect-error cluster
  (tracked against #2510, not #2645 — see the corrected table row above),
  and, per the fix's own in-code comment (`live_upgrade.rs:597-603`, "under
  an instrumented coverage binary … or a contended CI runner"), plain
  contention alone may be enough to probe mechanism 2 too, without needing
  `cargo llvm-cov` specifically. What it cannot do is reproduce either
  Linux/`Coverage (workspace)` job shape those two signatures were actually
  observed under — this harness only ever spins macOS VMs, so a clean
  macOS run narrows the macOS cluster but says nothing directly about the
  two Linux hits. The macOS cluster's own measured rate (3/17, or 3/30
  folding in the 13/13 clean samples #2548 already banked — exactly 10%,
  not below it) puts it on this role's standard ≥20 bar, not the ≥50
  reserved for genuinely low-rate flakes, so a single 20-sample dispatch
  (the input's max) can reach it in one run. It is still the only rerun harness
  this investigation has, and 43 hours idle is 43 hours of not even that
  much evidence. A second, Linux/coverage-shaped harness — flagged as
  future work in the 2026-09-09 report — remains not built. New macOS CI
  spend still needs sign-off; that has not changed.
- **Windows LNK1104 logged, not actioned.** One occurrence, no rerun,
  plausible infra cause. Revisit if it repeats.

## 📊 Measurement

No before/after from this pass — organic sampling plus recording an
already-shipped fix, not a rerun campaign this pass ran itself.

| Test | Hits (this pass) | Cumulative organic hits | Platforms seen | Status |
|---|---|---|---|---|
| `live_upgrade` (connection-error assertion) | 0 | 3/17 macOS (2026-09-03/04) | macOS only | **Correction (post-review):** not #2645 — `refused_errors`/`hard_failures` only increment on the `Err` path from the request call, which neither mechanism 1 (runs before the load loop starts) nor mechanism 3 (handles the `Ok(Observation)` 503 case, not `Err`) touches; already tied to #2510's `refused`/`hard` reclassification per this entry's own pre-existing text below. Unverified CI-natively either way; `manual-macos-contention-check.yml` can produce that evidence, still undispatched |
| `live_upgrade` (new-build-never-served) | 0 | 1 (2026-09-09) | Linux (coverage) | Mechanism #2 in #2645 — directly supported by the fix's own commit message; unverified CI-natively (no Linux/coverage harness exists) |
| `live_upgrade` (every-read-must-be-served, refused=0/hard=0) | 1 | 1 (2026-09-09) | Linux (coverage) | **Not attributed to any of the three mechanisms** — `status: 0` observations don't match mechanism 3's exact-503 predicate (checked against merged source); unverified CI-natively (no Linux/coverage harness exists) |
| `cache_stampede` (line 501) | 0 | 2 (2026-09-03, 2026-09-09) | macOS only | Undiagnosed |
| `sim_fault_plan` | 0 | 1 (2026-09-03) | macOS only | Undiagnosed |
| Windows `LNK1104` (link-stage, `todo-app`/`seed`) | 1 | 1 (2026-09-09) | Windows | New, unclassified, n=1 |

`manual-macos-contention-check.yml`: 0 → 0 `workflow_dispatch` runs, third
consecutive pass with no change (idle since it became dispatchable
2026-09-08T15:07:44Z).

## 🔬 Reproduce

```
list_workflow_runs(ci.yml, event=pull_request, status=completed, perPage=100, page=1)
# → filtered to created_at > 2026-09-09T09:47:12Z (the prior pass's cutoff):
#   86 runs, 55 cancelled / 23 success / 8 failure
# each failure's jobs via list_workflow_jobs(run_id, filter=latest),
# each failing job's log via get_job_logs(job_id, return_content=true);
# for the Coverage(workspace) hit, return_content's tail truncated before the
# panic line even at tail_lines=8000 (this test's per-request tracing spam is
# that large) — fetched the untruncated log via
# get_job_logs(job_id, return_content=false) → logs_url, then curl'd that
# blob URL directly and grepped for "panicked at".
```

Confirm the harness is still undispatched:

```
list_workflow_runs(manual-macos-contention-check.yml, event=workflow_dispatch)
# → total_count: 0
```

Confirm the fix and its own evidence:

```
git show 8fae8af --stat   # PR #2645 merge on trunk-dev, includes the
                           # "Fix live_upgrade test: three real timing races,
                           # not flakes" commit
```

Dispatch (still requires sign-off — new macOS CI spend): `sha` pinned to
`trunk-dev` at or after `8fae8af`, `samples: "20"` — the input's max choice,
short of the ≥50 this role's own bar calls for on its own, so treat one
dispatch as a first sample batch toward that total, not sufficient by
itself, and expect to dispatch again to accumulate the rest.
