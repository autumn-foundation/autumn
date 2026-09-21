# 🚦 Semaphore: CI health follow-up — job_tracking closure holds; two new signatures opened, not yet campaigned

Follow-up to `docs/reports/2026-09-20-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. This pass
confirms the `job_tracking_stores_integration` closure from the last pass is
holding (two organic hits found this pass are stale-branch artifacts that
predate the fix, not evidence against it), and opens two new,
not-yet-campaigned investigations: `sqlite_job_backend_tracks_job_status_durably`
failed identically on two independent branches — one of them a pure docs
change — in the `SQLite runtime (feature=sqlite)` job; and (added after a
review correction — see below) `crate_path::tests::resolve_autumn_web_name_dashed_rename_is_sanitized`
failed on `Test (macos-latest)` on that same docs-only branch, which this
report's first version wrongly dismissed as branch-owned. No fix PR opens
this pass on either; the hard gate isn't cleared for either (n=2 and n=1,
no Tier 1 baseline, mechanism not confirmed).

**Correction (post-review, via two Codex review comments on PR #2883):**
this report originally (1) misclassified the `crate_path` macOS failure as
branch-owned, when the failing branch (PR #2842) touches no Rust code at
all, and (2) overclaimed that git ancestry verified **both** stale
`job_tracking_stores_integration` hits as pre-fix, when the ancestry command
actually run only checked one branch's base commit. Both are corrected
throughout this report; see the ledger entries for the full detail.

## 🎯 Verdict path

Unchanged: `trunk-dev` is green; the required gate is `Test suite`
(`test-gate`), fed by `[test, trybuild, test-features, test-docker]`, plus
`Supply chain (cargo-deny)`. `manual-macos-contention-check.yml` remains
dispatch-only — still zero `workflow_dispatch` runs, now a **13th consecutive
idle pass** (~306.8 hours, past 12.75 days, since it became dispatchable
2026-09-08T15:07:44Z). Not dispatched this pass: new macOS CI spend needs a
human sign-off per this role's own rules, unavailable in this unattended run.

## 🌡️ Symptom

**Organic-hit sampling**, 2026-09-20T07:33:19Z (exclusive) to
2026-09-21T09:55:07Z (~26.4h), one `perPage=100`/`page=1` query whose own
span fully covered the window — 75 `pull_request`-triggered `ci.yml` runs:
55 cancelled / 15 success / **5 failure**. All 5 triaged at job level:

| Run | Branch | Failing job(s) | Finding |
|---|---|---|---|
| 35540844428 | `claude/friendly-ritchie-d36hku` (PR #2842, docs-only — no Rust source touched) | `Test (macos-latest)`, `SQLite runtime` | macos: **new signature** (organic, not branch-owned — see below); sqlite: **new signature**, see below |
| 35523247491 | `claude/macro-split-decomposition-jalk90` (WIP, branch ref since deleted) | `SQLite runtime`, `Test (Docker)` | sqlite: same new signature; Docker: `job_tracking_stores_integration` repeat — **pre-fix branch**, not a reopening |
| 35530941996 | `claude/epic-meitner-eh6w1m` (PR #2870) | `Test (Docker)` | same pre-fix `job_tracking_stores_integration` repeat |
| 35539828393 | `claude/intelligent-wright-ebjkn4` | `Test (Docker)` | `examples/saas`'s own test, branch-owned, closes a gap the 2026-09-20 report left open |
| 35522888590 | `dependabot/github_actions/dtolnay/rust-toolchain-1.120.0` | `SQLite runtime`, `MSRV` | already-documented own action-pin-bump break, unmerged |

**The `job_tracking_stores_integration` repeats are not a reopening, but the
evidence differs per branch — corrected from the original version of this
report, which conflated the two.** For PR #2870 (`claude/epic-meitner-eh6w1m`),
verified by git ancestry: `git merge-base --is-ancestor 0a0986b
9800221460975e7b3ee75a8490e392cb4b489f82` (that PR's own base sha) exits 1 —
the TTL fix (PR #2867, merged 2026-09-20T19:35:35Z UTC) is not an ancestor of
this branch's base. For `claude/macro-split-decomposition-jalk90`, whose
branch ref no longer exists on origin (so a local ancestry check isn't
possible), the evidence is the head commit's own `committer.date`
(2026-09-20T16:34:05Z, fetched via the GitHub API) and the CI run's start
time (2026-09-20T16:36:30Z), both well before the fix's merge — timestamp
evidence, not ancestry. Both panics are also at the **pre-fix** line number
(`job_tracking_stores_integration.rs:264:5`, `"record should be past its
configured TTL"`), not the post-fix poll-based version, which independently
corroborates both. Both branches predate the fix/close (PRs #2867/#2874), so
they are still carrying the known, already-diagnosed, already-fixed ~2%
flake — consistent with, not contradicting, last pass's 0/50 closure.

**The `crate_path` macOS failure is not branch-owned — corrected from the
original version of this report, which dismissed it as such.** PR #2842's
full file list is `.github/workflows/ci.yml`, `README.md`, a `changelog.d/`
fragment, five `docs/guide/*.md` pages, two new `scripts/check-docs-retrieval*`
files, and `skills/autumn-web/SKILL.md` — no Rust source, let alone
`autumn-macros-support`, so this branch cannot own a failure in that crate's
own unit test. `crate_path::tests::resolve_autumn_web_name_dashed_rename_is_sanitized`
failed with `left: "autumn_web", right: "autumn_web_05"` at
`autumn-macros-support/src/crate_path.rs:708:9` — an organic, undiagnosed
hit, logged as its own new ledger entry below alongside the SQLite one.

**The new signature**: `sqlite_job_backend_tracks_job_status_durably`
(`autumn/tests/sqlite_jobs_scheduler_e2e.rs:1301:6`) panicked identically on
both hits:

```
tracked enqueue: AutumnError { status: 500, inner: StringError("sqlite job
enqueue failed: ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE
constraint"), ... }
```

The two hits are on genuinely independent branches — one (#2842) is a
**docs-only** change ("0 pages added" per its own title, no job/SQLite code
touched) — which rules out either branch's own diff as the cause.

## 🔍 Diagnosis

**Source location confirmed, cause not yet confirmed.** The panic originates
in `SqliteJobBackend`'s enqueue path (`autumn/src/job/sqlite.rs:429-432`):
the insert's `ON CONFLICT (name, unique_key) WHERE unique_key IS NOT NULL AND
status IN ('enqueued', 'running') DO NOTHING` clause targets a **partial
unique index** unconditionally, for every job — including this test's job,
which declares no `JobUniqueness`. SQLite raises this exact error text when
an `ON CONFLICT` target doesn't match an existing index's columns *and*
partial predicate, not on an ordinary duplicate-value violation — so the
index this clause expects did not exist, in the expected shape, on this
connection at execution time.

**Correction (post-review, via a Codex review comment on PR #2883): the
migration/readiness-race hypothesis below is wrong, not just unconfirmed —
ruled out by the queue path itself.** `enqueue_job_at`
(`autumn/src/job/sqlite.rs:391`) calls `queue_handle.ready().await?` before
obtaining a connection or executing the insert; `SqliteJobQueue::ready`
(`autumn/src/job/sqlite.rs:253-258`) awaits `ensure_schema` through a
`tokio::sync::OnceCell`, and `ensure_schema`
(`autumn/src/job/sqlite.rs:269-305`) is what creates
`idx_autumn_jobs_unique_inflight` — the exact partial index the `ON CONFLICT`
clause targets — via a synchronously awaited `CREATE UNIQUE INDEX IF NOT
EXISTS`. Confirmed directly against source, not taken on the reviewer's word:
every enqueue through this queue handle awaits schema creation first, so an
enqueue cannot structurally overtake it. The original (now-withdrawn)
framing follows, struck through for the record: ~~a readiness race between
the fresh per-test SQLite pool's migrations and
`job::start_runtime`/`enqueue_tracked` being able to submit work before that
migration completes~~. The actual mechanism is open again; see the ledger
entry's own correction for the remaining, not-yet-investigated candidates.

**Ruled out**: cross-test interference via the process-global
`GLOBAL_JOB_CLIENT` this test depends on. Every test in
`sqlite_jobs_scheduler_e2e.rs` that calls `job::start_runtime` holds
`global_job_runtime_test_lock()` first; the tests that don't hold it build
their own scoped coordinator/lock/store against their own local pool, never
the global client — so they don't appear able to race this test's
global-state window (checked within this file; not exhaustively checked
against every other file that might share the same test binary).

**Test-vs-product verdict: not rendered.** The readiness-gap framing above is
now ruled out; the open candidates (a second enqueue path that bypasses
`ready()`, a SQLite-version-specific `ON CONFLICT` partial-index matching
quirk, a stale/reused database file) haven't been sorted into test-defect vs.
product-defect yet. Undetermined.

**The second new signature, `crate_path::tests::resolve_autumn_web_name_dashed_rename_is_sanitized`
(source read, mechanism unconfirmed).** The test writes a fixture `Cargo.toml`
declaring a dashed rename (`autumn-web-05 = { package = "autumn-web" }`) and
calls `resolve_autumn_web_name()` with `CARGO_MANIFEST_DIR` pointed at it via
`with_fixture_manifest` (`autumn-macros-support/src/crate_path.rs:619-627`),
which uses `temp_env::with_var` — documented to serialize concurrent callers
via a process-wide lock specifically so this pattern is safe under parallel
test execution. `resolve_autumn_web_name` (`crate_path.rs:94-105`) delegates
to `proc_macro_crate::crate_name("autumn-web")` and falls back to the
unrenamed default on any `Err`/`FoundCrate::Itself` — exactly the value
observed, meaning `crate_name` didn't see the fixture as declaring a rename.
Leading (**unconfirmed**) hypothesis: something in `proc_macro_crate::crate_name`
(a third-party crate; internals not read this pass) reads or caches
`CARGO_MANIFEST_DIR` outside `temp_env`'s lock coverage, or a third,
unaudited call site sets the same env var without going through `temp_env`.
Test-vs-product verdict not rendered — very likely test-only given the code
is a test fixture helper, but not confirmed.

## 🔧 Treatment

None this pass — the hard gate isn't cleared. n=2 organic hits is not a Tier
1 baseline, the mechanism is a hypothesis not a confirmed cause, and no
test-vs-product verdict has been rendered. Opening a fix now would be a
retry in disguise. Logged in
`docs/ci-health/quarantine-ledger.md`'s "Under active investigation, not yet
quarantined" section with full intake-quality detail instead.

**Next step for `sqlite_job_backend_tracks_job_status_durably`**: build a
same-commit rerun harness for this test against the
`SQLite runtime (feature=sqlite)` feature set — same pattern as
`.github/workflows/manual-job-tracking-rerun-check.yml` — to reproduce it on
demand, since the migration/readiness hypothesis is now ruled out by source
and no replacement mechanism has surfaced yet.

**Next step for `crate_path::tests::resolve_autumn_web_name_dashed_rename_is_sanitized`**:
reproduce locally with repeated `cargo test -p autumn-macros-support
crate_path:: -- --test-threads=<N>` runs (the `--` separator matters —
`--test-threads` is a libtest argument, not a cargo one, and omitting it
fails before any test runs) at varying `N` to see whether
parallelism reproduces it, before deciding whether a dedicated harness is
warranted at n=1.

## 📊 Measurement

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `job_tracking_stores_integration` | Closed 2026-09-20 (0/50 post-fix) | 2 organic hits — one confirmed pre-fix by git ancestry (PR #2870), one by commit/run timestamp (branch ref since deleted) — not a reopening | Unchanged, still closed |
| `sqlite_job_backend_tracks_job_status_durably` | Not tracked | **New**, n=2 organic (both this pass), source-located, mechanism unconfirmed | Under active investigation — needs a rerun harness before any fix |
| `crate_path::…_dashed_rename_is_sanitized` | Not tracked | **New**, n=1 organic (this pass, found after a review correction), source-located, mechanism unconfirmed | Under active investigation — needs local repro before any fix |
| `live_upgrade` (3 signatures) | Uncampaigned | No new organic hits this pass | Unchanged |
| `cache_stampede` | Uncampaigned | No new organic hits this pass | Unchanged |
| `sim_fault_plan` | n=1, uncampaigned | No new organic hits this pass | Unchanged |
| `manual-macos-contention-check.yml` dispatches | 0 (12 idle passes) | 0 (13th idle pass, ~306.8h) | Needs human sign-off for CI spend |

No revert check this pass — no fix was proposed, so there is nothing to
revert-check.

## 🔬 Reproduce

Confirm the two `sqlite_job_backend_tracks_job_status_durably` hits:

```
get_job_logs(owner=autumn-foundation, repo=autumn, job_id=106158118450,
             return_content=true, tail_lines=140)
get_job_logs(owner=autumn-foundation, repo=autumn, job_id=106110875320,
             return_content=true, tail_lines=60)
# both -> "sqlite job enqueue failed: ON CONFLICT clause does not match any
#          PRIMARY KEY or UNIQUE constraint" at sqlite_jobs_scheduler_e2e.rs:1301:6
```

Confirm the `crate_path` macOS hit and that PR #2842 touches no Rust source:

```
get_job_logs(owner=autumn-foundation, repo=autumn, job_id=106162503374,
             return_content=true, tail_lines=300)
# -> left: "autumn_web", right: "autumn_web_05" at crate_path.rs:708:9
pull_request_read(get_files, owner=autumn-foundation, repo=autumn, pullNumber=2842)
# -> ci.yml, README.md, changelog.d/, docs/guide/*.md, scripts/check-docs-retrieval*,
#    skills/autumn-web/SKILL.md -- no Rust source
```

Confirm the `job_tracking_stores_integration` repeats are pre-fix — by
ancestry for PR #2870, by timestamp for the branch whose ref no longer
exists:

```
git fetch origin 9800221460975e7b3ee75a8490e392cb4b489f82
git merge-base --is-ancestor 0a0986b 9800221460975e7b3ee75a8490e392cb4b489f82
echo $?   # -> 1 (not an ancestor): PR #2870's base predates the fix

get_commit(owner=autumn-foundation, repo=autumn,
           sha=30729276b2f8a50b76b70110c0aeb4ca9596c59a)
# -> committer.date: 2026-09-20T16:34:05Z, well before the fix's
#    2026-09-20T19:35:35Z UTC merge (branch ref itself is gone from origin)
```

Confirm the ON CONFLICT target and its unconditional partial-index shape:

```
sed -n '380,465p' autumn/src/job/sqlite.rs
# INSERT ... ON CONFLICT (name, unique_key) WHERE unique_key IS NOT NULL
#   AND status IN ('enqueued','running') DO NOTHING  -- always this target
```

Confirm the macOS harness is still undispatched:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=manual-macos-contention-check.yml,
             event=workflow_dispatch)
# → total_count: 0
```
