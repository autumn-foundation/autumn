# 🚦 Semaphore: CI health follow-up — job_tracking closure holds; new sqlite_jobs_scheduler_e2e signature opened, not yet campaigned

Follow-up to `docs/reports/2026-09-20-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. This pass
confirms the `job_tracking_stores_integration` closure from the last pass is
holding (two organic hits found this pass are both stale-branch artifacts
that predate the fix, not evidence against it), and opens a new,
not-yet-campaigned investigation: `sqlite_job_backend_tracks_job_status_durably`
failed identically on two independent branches — one of them a pure docs
change — in the `SQLite runtime (feature=sqlite)` job. No fix PR opens this
pass; the hard gate isn't cleared (n=2, no Tier 1 baseline, mechanism not
confirmed).

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
| 35540844428 | `claude/friendly-ritchie-d36hku` (PR #2842, docs-only) | `Test (macos-latest)`, `SQLite runtime` | macos: unrelated branch-owned unit test; sqlite: **new signature**, see below |
| 35523247491 | `claude/macro-split-decomposition-jalk90` (WIP, no PR) | `SQLite runtime`, `Test (Docker)` | sqlite: same new signature; Docker: `job_tracking_stores_integration` repeat — **pre-fix branch**, not a reopening |
| 35530941996 | `claude/epic-meitner-eh6w1m` (PR #2870) | `Test (Docker)` | same pre-fix `job_tracking_stores_integration` repeat |
| 35539828393 | `claude/intelligent-wright-ebjkn4` | `Test (Docker)` | `examples/saas`'s own test, branch-owned, closes a gap the 2026-09-20 report left open |
| 35522888590 | `dependabot/github_actions/dtolnay/rust-toolchain-1.120.0` | `SQLite runtime`, `MSRV` | already-documented own action-pin-bump break, unmerged |

**The `job_tracking_stores_integration` repeats are not a reopening.**
Verified by git ancestry, not just timestamp: `git merge-base --is-ancestor
0a0986b <PR #2870's base sha>` exits 1 — the TTL fix (PR #2867, merged
2026-09-20T19:35:35Z UTC) is not an ancestor of either failing branch's base.
Both panics are also at the **pre-fix** line number
(`job_tracking_stores_integration.rs:264:5`, `"record should be past its
configured TTL"`), not the post-fix poll-based version. Both branches were
cut from `trunk-dev` before the fix/close (PRs #2867/#2874) landed, so they
are still carrying the known, already-diagnosed, already-fixed ~2% flake —
consistent with, not contradicting, last pass's 0/50 closure.

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

Leading, **unconfirmed** hypothesis: a readiness race between the fresh
per-test SQLite pool's migrations (presumably what creates this partial
index) and `job::start_runtime`/`enqueue_tracked` being able to submit work
before that migration completes. `create_pool` (`autumn/src/db.rs:1768`) is
synchronous and does not itself run migrations, so nothing in the pool
construction call guarantees the index exists before `start_runtime` returns
— but tracing exactly where/when the migration runs relative to readiness
was time-boxed out of this pass.

**Ruled out**: cross-test interference via the process-global
`GLOBAL_JOB_CLIENT` this test depends on. Every test in
`sqlite_jobs_scheduler_e2e.rs` that calls `job::start_runtime` holds
`global_job_runtime_test_lock()` first; the tests that don't hold it build
their own scoped coordinator/lock/store against their own local pool, never
the global client — so they don't appear able to race this test's
global-state window (checked within this file; not exhaustively checked
against every other file that might share the same test binary).

**Test-vs-product verdict: not rendered.** Could be a test-local
migration-ordering gap, or a real readiness gap in `start_runtime`'s public
contract — the latter would be a product defect. Undetermined pending
further tracing.

## 🔧 Treatment

None this pass — the hard gate isn't cleared. n=2 organic hits is not a Tier
1 baseline, the mechanism is a hypothesis not a confirmed cause, and no
test-vs-product verdict has been rendered. Opening a fix now would be a
retry in disguise. Logged in
`docs/ci-health/quarantine-ledger.md`'s "Under active investigation, not yet
quarantined" section with full intake-quality detail instead.

**Next step**: build a same-commit rerun harness for this test against the
`SQLite runtime (feature=sqlite)` feature set — same pattern as
`.github/workflows/manual-job-tracking-rerun-check.yml` — to get a Tier 1
baseline, and trace `job::start_runtime`'s migration/readiness ordering
directly to confirm or rule out the hypothesis above before proposing any
fix.

## 📊 Measurement

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `job_tracking_stores_integration` | Closed 2026-09-20 (0/50 post-fix) | 2 organic hits, both confirmed pre-fix by git ancestry — not a reopening | Unchanged, still closed |
| `sqlite_job_backend_tracks_job_status_durably` | Not tracked | **New**, n=2 organic (both this pass), source-located, mechanism unconfirmed | Under active investigation — needs a rerun harness before any fix |
| `live_upgrade` (3 signatures) | Uncampaigned | No new organic hits this pass | Unchanged |
| `cache_stampede` | Uncampaigned | No new organic hits this pass | Unchanged |
| `sim_fault_plan` | n=1, uncampaigned | No new organic hits this pass | Unchanged |
| `manual-macos-contention-check.yml` dispatches | 0 (12 idle passes) | 0 (13th idle pass, ~306.8h) | Needs human sign-off for CI spend |

No revert check this pass — no fix was proposed, so there is nothing to
revert-check.

## 🔬 Reproduce

Confirm the two new-signature hits:

```
get_job_logs(owner=autumn-foundation, repo=autumn, job_id=106158118450,
             return_content=true, tail_lines=140)
get_job_logs(owner=autumn-foundation, repo=autumn, job_id=106110875320,
             return_content=true, tail_lines=60)
# both -> "sqlite job enqueue failed: ON CONFLICT clause does not match any
#          PRIMARY KEY or UNIQUE constraint" at sqlite_jobs_scheduler_e2e.rs:1301:6
```

Confirm the `job_tracking_stores_integration` repeats are pre-fix, by
ancestry rather than timestamp alone:

```
git fetch origin 9800221460975e7b3ee75a8490e392cb4b489f82
git merge-base --is-ancestor 0a0986b 9800221460975e7b3ee75a8490e392cb4b489f82
echo $?   # -> 1 (not an ancestor): this branch's base predates the fix
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
