# 🚦 Semaphore: CI health follow-up — one new n=1 signature, the macOS harness idle a 4th day

Follow-up to `docs/reports/2026-09-10-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR from
this pass — the one new finding is a single organic occurrence with a
plausible but unconfirmed mechanism, which does not clear this role's hard
gate (a fix needs a rerun-rate baseline, not a hypothesis from reading
source). What this pass does: logs that finding in the ledger with a full
mechanism writeup so a repeat is recognized rather than rediscovered, and
records that `manual-macos-contention-check.yml` — the harness the
`live_upgrade`/`cache_stampede`/`sim_fault_plan` investigation has been
waiting on since 2026-09-08 — is still undispatched.

## 🎯 Verdict path

`trunk-dev` is green (`ci.yml`'s own push-triggered runs on the branch
succeed). The check developers actually wait on is `Test suite`, the
shard-result aggregator across `test`/`trybuild`/`test-features`/
`test-docker`/`coverage`. `manual-macos-contention-check.yml` remains
dispatch-only, gated on a human (new macOS CI spend needs sign-off per this
role's own "Ask before" list) — **still zero `workflow_dispatch` runs**
(`total_count: 0`, checked 2026-09-11T~09:5xZ), the same number reported on
2026-09-08, 2026-09-09, and 2026-09-10. Four consecutive daily passes with no
movement, while the investigation it exists to verify has had a fix land
(#2645, 2026-09-10) in the meantime.

## 🌡️ Symptom

Sampled the ~23h since the 2026-09-10 follow-up's cutoff
(2026-09-10T10:30:30Z–2026-09-11T09:09:24Z): 100 `pull_request`-triggered
`ci.yml` runs, 61 cancelled (superseded pushes, not a health signal), 25
success, **14 failure**. Triaged each by job/log inspection:

- **10 are ordinary WIP-branch `Clippy` failures**, not CI health issues:
  8 on a single branch (`claude/upbeat-allen-qn9aoo`, runs between
  2026-09-10T23:17Z and 2026-09-11T03:23Z — one person iterating on
  not-yet-fixed lint issues across successive pushes to the same branch,
  confirmed via `list_workflow_jobs` on the latest of the eight), plus 2
  more on `codex/plan-and-fix-issue-1751-with-tdd-principles` and
  `vesper/bugbash-2678-shortcode-escape` (each run's `Lint` job fails at the
  `Clippy` step specifically, formatting and every other step green).
- **1 is the same known recurring `cargo-deny` failure** already logged in
  the 2026-09-09 and 2026-09-10 passes: `dependabot/cargo/validator-0.21.0`
  (run 34517924462) — the upstream `validator` release still needs code
  changes this dependency-bump branch doesn't carry. Unchanged, not a new
  finding.
- **1 is a genuine WIP product bug, not a CI health issue**: run
  34467823999 (`vesper/bugbash-2635-api-token-error-response`) fails
  `Test (windows-latest)`, `Test (macos-latest)`, `Test (ubuntu-latest)` and
  `SQLite runtime (feature=sqlite)` simultaneously, all at the same panic —
  `autumn/src/auth.rs:5019:9`, test
  `auth::api_token_tests::api_token_error_response_reclassifies_cancelled_statements`.
  A single test failing identically across every OS is the signature of a
  real defect in the branch's own change, not environment flakiness — and
  the branch name and the failing test name both name the same feature
  (`api_token_error_response`), consistent with in-progress work whose new
  test doesn't pass yet. Not campaigned, not logged as a flake.
- **1 is a second, distinct genuine WIP product bug, also not a CI health
  issue**: run 34512432068 (`codex/plan-and-fix-model-registration-issue`)
  fails `Test (windows-latest)` — not `Clippy` — at two `autumn-macros`
  unit tests, `crate_path::tests::resolve_autumn_web_name_honors_cargo_rename`
  and `crate_path::tests::resolve_autumn_web_name_default_when_unrenamed`,
  each asserting `"autumn_web" == "web"` (or the reverse) and failing with
  the two sides swapped — a crate-rename-resolution regression in the
  branch's own in-progress change, not an environment-dependent flake.
  (An earlier version of this report missed this run entirely, which is
  why the tally below didn't reconcile — see the correction note.)
- **1 new signature, logged in the ledger, not yet campaigned**: run
  34517281816 (`vesper/bugbash-2634-spez-normalize-fallback`, itself
  unrelated to job tracking) failed `Test (Docker)` — the bare `--ignored`
  sweep over the `autumn` consolidated `integration_tests` binary — at
  `integration::job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`,
  `"record should be past its configured TTL"` at
  `job_tracking_stores_integration.rs:264:5`. `test result: FAILED. 369
  passed; 1 failed` — a single failure in an otherwise-clean 37-minute sweep.
  Full writeup and mechanism hypothesis in the ledger (new entry, this
  pass).
`Test suite`'s own aggregator on run 34517281816 reports failure too, for
the same underlying `Test (Docker)` failure above — a second failed *job*
in an already-counted run, not a 14th failed *run*, so it is not added to
the tally separately.

**Correction (post-review, via a Codex review comment on PR #2711): the
first version of this bullet list only accounted for 13 of the 14 failed
runs.** Reconciled: 10 Clippy + 1 `cargo-deny` + 1 `api_token_error_response`
+ 1 `crate_path` rename regression (added above) + 1 `job_tracking` = 14.
The missing run was 34512432068, mischaracterized in the original pass as
one of the "2 more" Clippy failures without actually checking its job log —
it fails a `Test (windows-latest)` unit test, not `Clippy`. Fixed above.

No hits this pass on `live_upgrade`, `cache_stampede`, or `sim_fault_plan` —
the three signatures the ongoing macOS/coverage investigation is tracking.

## 🔍 Diagnosis

**`job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`:
a plausible mechanism, not yet a rendered verdict.** Read against
`autumn/tests/integration/job_tracking_stores_integration.rs` and
`autumn/src/job_tracking.rs` rather than guessed from the panic text alone.
The test configures a 1-second TTL, sleeps a fixed 1200ms (`Instant`-backed,
so it cannot fire early — at least 1200ms of real host time genuinely
elapses), then asserts `expires_at <= NOW()` where `expires_at` was stamped
by the *application's* `SystemClock` at write time and `NOW()` is evaluated
by *Postgres's own clock* at read time. With only a ~200ms margin between
the TTL and the sleep, and two independently-advancing clock sources being
compared instead of one, this reads as a textbook thin-margin timing
dependency — the same anti-pattern class as the three `live_upgrade`
mechanisms PR #2645 fixed (a fixed wait with no stated budget against
contention), except here the missing budget is clock-skew tolerance rather
than a retry-on-barrier.

**Correction (post-review, via a Codex review comment on PR #2711): a
second, likely more probable mechanism requires no clock skew at all.**
The test's own job runtime processes the enqueued no-op job during that
1200ms window; `run_job_handler_inner` calls `store.mark_running(key)` on
pickup and `ctx.settle_success()` on completion, and both route through
`PgJobTrackingStore::update`, which unconditionally rewrites `expires_at`
to *that write's own* `now + ttl`. If either write lands roughly
200-1000ms after the test's initial read — ordinary worker dispatch
latency, no contention required — `expires_at` is pushed past the 1.2s
check point on one single, consistent clock. This mechanism and the
clock-skew one are not mutually exclusive, and neither is confirmed; see
the ledger entry for the full comparison, including why the sibling Redis
test isolates one hypothesis but not the other.

The production code path this test is meant to verify never makes the
cross-clock comparison — reads filter on the same `self.clock.now()` used
to write, never against `NOW()` — so under the clock-skew hypothesis this
is a test defect, not a product defect. Refreshing `expires_at` on worker
activity is deliberate production behavior in its own right, so under the
worker-refresh hypothesis the defect is in the test's assumption that a
fixed sleep leaves no room for the job's own worker to touch the record,
not in the store — a test defect either way. **Both are hypotheses, not a
verdict**: n=1, no rerun evidence, and neither has been isolated (e.g. by
asserting on `updated_at` to see which write, if either, actually fired).
Recorded in the ledger rather than acted on, per this role's own bar — a
fix here without a rerun-rate baseline would be exactly the "retry in
disguise" the hard gate exists to block, even though no retry is actually
being proposed.

**`live_upgrade`/`cache_stampede`/`sim_fault_plan`: unchanged.** Zero new
organic hits this pass; the harness that would let any of these three close
is still undispatched.

## 🔧 Treatment

None shipped by this pass — no finding here clears the hard gate for a fix.

- **`docs/ci-health/quarantine-ledger.md` updated**: new entry for
  `job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`
  with the mechanism hypothesis above, explicitly marked n=1/not campaigned/
  not quarantined (the Docker sweep is unmodified — this test keeps running
  on every sweep, since one unconfirmed hit is not grounds to skip it). Also
  added a dated note to the `live_upgrade` entry recording this pass's
  zero-new-hits result and the harness's 4th idle day.
- **Recommendation for a human, unchanged from the last three passes**:
  dispatch `manual-macos-contention-check.yml` against a `trunk-dev` commit
  at or after `8fae8af` (the #2645 fix), `samples: "20"`. It is the only
  rerun harness this investigation has, still macOS-only, and four days
  idle is four days of not even the partial evidence it could already be
  producing.
- **No action on the `api_token_error_response` WIP failure** — it belongs
  to whoever is driving `vesper/bugbash-2635-api-token-error-response`, not
  to CI health.

## 📊 Measurement

No before/after — this pass is organic sampling plus one new source-level
diagnosis, not a rerun campaign of its own.

| Test | Hits (this pass) | Cumulative organic hits | Status |
|---|---|---|---|
| `live_upgrade` (all three tracked signatures) | 0 | unchanged from 2026-09-10 | Verdict rendered for 1 of 3 signatures (the Linux "new build never served" hit, PR #2645's mechanism 2); the macOS connect-error cluster is attributed to the earlier #2510, and the third (`status: 0`) signature remains unattributed to any fix, per the ledger's own corrected attribution. CI-native verification still blocked on the undispatched harness |
| `cache_stampede` (line 501) | 0 | 2 (2026-09-03, 2026-09-09) | Undiagnosed |
| `sim_fault_plan` | 0 | 1 (2026-09-03) | Undiagnosed |
| `job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it` | 1 | 1 (new, 2026-09-10T19:54Z) | New; mechanism hypothesis recorded, not campaigned |

`manual-macos-contention-check.yml`: 0 → 0 `workflow_dispatch` runs, 4th
consecutive pass with no change (idle since it became dispatchable
2026-09-08T15:07:44Z — now ~66.5 hours).

## 🔬 Reproduce

```
actions_list(list_workflow_runs, ci.yml, event=pull_request, status=completed,
             perPage=100, page=1)
# → filtered to created_at > 2026-09-10T10:30:30Z (this pass's window):
#   100 runs, 61 cancelled / 25 success / 14 failure
# each failure's jobs via list_workflow_jobs(run_id, filter=latest),
# each failing job's log via get_job_logs(job_id, return_content=true)
```

The new finding:

```
get_job_logs(job_id=103013664273, return_content=true)
# → integration::job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it ... FAILED
# → panicked at autumn/tests/integration/job_tracking_stores_integration.rs:264:5:
#   record should be past its configured TTL
```

Confirm the harness is still undispatched:

```
actions_list(list_workflow_runs, manual-macos-contention-check.yml, event=workflow_dispatch)
# → total_count: 0
```
