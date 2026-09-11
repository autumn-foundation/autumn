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
succeed). The check developers actually wait on is `Test suite`
(`test-gate`), the required aggregator whose own `needs:` in `ci.yml` is
exactly `[test, trybuild, test-features, test-docker]` — `coverage` is a
separate lane (`needs: [test, meta]`) that reports its own check and does
not feed `test-gate`, so a coverage failure does not fail the required
gate. (An earlier version of this line included `coverage` in the
aggregation; corrected per a Codex review comment, checked against
`ci.yml` directly.) `manual-macos-contention-check.yml` remains
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
success, **14 failure**. **Correction (post-review, via a fifth Codex
review comment on PR #2711): this window silently left a 42-minute gap.**
The 2026-09-10 report's own recorded cutoff was `2026-09-10T09:48:19Z`,
not `10:30:30Z` — checked directly against that report rather than
re-typed from memory. Queried the gap itself
(`2026-09-10T09:48:19Z`–`10:30:30Z`): 9 `pull_request`-triggered `ci.yml`
runs, **all 9 cancelled** (superseded pushes on two WIP branches), zero
successes and zero failures. The gap doesn't change the 14-failure tally
or any conclusion below, but it should have been queried and stated
rather than silently skipped. Triaged each of the 14 by job/log
inspection:

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
**Correction (post-review, via a second Codex review comment on PR #2711):
the missing run's provenance was stated wrong in the first fix.** Run
34512432068 was not mischaracterized as one of the "2 more" Clippy
failures — those two were explicitly named as the `issue-1751` and
`shortcode-escape` branches, and the ten-run Clippy count is still exactly
those two plus the eight `upbeat-allen` runs. 34512432068 (branch
`codex/plan-and-fix-model-registration-issue`) was simply omitted from the
tally altogether — not checked, not bucketed as anything — which is why
the stated 14 only broke down to 13. It fails a `Test (windows-latest)`
unit test, not `Clippy`; added as its own bucket above.

No hits inside the sampled window on `live_upgrade`, `cache_stampede`, or
`sim_fault_plan` — the three signatures the ongoing macOS/coverage
investigation is tracking. **Late addition: one did land, live, on this
PR's own CI after the sampled window closed.** Run 34591670807 (`Test
(ubuntu-latest)`, this PR's branch, 2026-09-11T11:51:57Z) hit
`live_upgrade`'s previously-unattributed `status: 0` signature a second
time — same assertion, same shape as the 2026-09-09T13:59Z hit, now also
on a plain (non-coverage) job. Docs-only PR, so this is organic noise
unrelated to this diff; full writeup in the ledger's `live_upgrade` entry,
including why this weakens the coverage-instrumentation hypothesis for
that signature.

## 🔍 Diagnosis

**`job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`:
a plausible mechanism, not yet a rendered verdict — and the leading
candidate changed twice under review.** Read against
`autumn/tests/integration/job_tracking_stores_integration.rs` and
`autumn/src/job_tracking.rs` rather than guessed from the panic text alone.
The test configures a 1-second TTL, sleeps a fixed 1200ms (`Instant`-backed,
so it cannot fire early — at least 1200ms of real host time genuinely
elapses), then asserts `expires_at <= NOW()`, evaluated by Postgres.

The original hypothesis in this pass framed this as a dual-clock-source
race: `expires_at` stamped by the app's `SystemClock`, `NOW()` evaluated
by Postgres's own clock, with only a ~200ms margin between the TTL and the
sleep — read as needing Postgres's clock to lag the app host's under
runner contention. **Correction (post-review, via a second Codex review
comment on PR #2711): drop contention-induced clock skew as a candidate.**
The test process and its `testcontainers`-managed Postgres share the same
runner's `CLOCK_REALTIME` (no time namespace is configured), so scheduling
contention can only delay *when* a value is read, never make the value
read back lag true elapsed time — both sides are the same clock. A real
skew here would need a discrete clock step (e.g. an NTP correction), a
different and far less likely mechanism than originally proposed; see the
ledger for the full reasoning.

**Correction (post-review, via a first Codex review comment on PR #2711,
and now the primary candidate): the test's own job runtime can rewrite
`expires_at` with no clock disagreement of any kind.** `run_job_handler_inner`
calls `store.mark_running(key)` on pickup and `ctx.settle_success()` on
completion of the enqueued no-op job, and both route through
`PgJobTrackingStore::update`, which unconditionally rewrites `expires_at`
to *that write's own* `now + ttl`. If either write lands roughly
200-1000ms after the test's initial read — ordinary worker dispatch
latency — `expires_at` is pushed past the 1.2s check point legitimately,
on one single clock. This is the same worker/update race the reviewer
notes the Redis sibling test also permits in principle, though no organic
hit has been observed there — that clean history doesn't help isolate
this hypothesis either way.

**Correction (post-review, via a fourth Codex review comment on PR #2711):
"production never makes the cross-process comparison" was flatly wrong —
a separate production code path makes exactly that comparison,
deliberately.** `pg_cleanup_expired_tracking_rows`
(`autumn/src/job.rs:9333-9358`), run periodically off a
`tracking_cleanup_interval.tick()`, executes `DELETE FROM
autumn_job_tracking WHERE expires_at <= NOW()` — Postgres's own `NOW()`
against an `expires_at` stamped by the app's `SystemClock`, the same
cross-process shape this test's assertion uses. The codebase's own test
comments (`autumn/src/job.rs:16704-16707`) already document this choice
explicitly ("`pg_cleanup_expired_tracking_rows` compares against
Postgres's real `NOW()`"), so it is deliberate design, not an oversight —
this test independently re-derives a comparison the product already makes
elsewhere, it doesn't invent a comparison production never makes.

**Correction (post-review, via a tenth Codex review comment on PR #2711,
carrying the ledger's own already-fixed correction into this report,
which still had the stale claim): the *read* path this test also
exercises is not reliably same-clock in production either — that was
only true for this test's single-process shape.** `docs/guide/jobs.md`
documents `web` and `worker` as separate process roles sharing one
Postgres backend; a `web` replica's `job::enqueue_tracked` can stamp
`expires_at` from its own `SystemClock` while a *different* `worker`
replica's `mark_running`/`settle_success` later calls
`PgJobTrackingStore::update` (`autumn/src/job_tracking.rs:1896-1902`)
using that host's own `self.clock.now()` — genuine cross-host skew, the
same shape as the cleanup sweep, no clock step required. Only this test's
own `combined`-role (single-process) shape makes the read path same-clock;
a discrete clock step is not the only way it can disagree with an
`expires_at` stamped elsewhere. `autumn/src/time.rs:105-108`'s point about
wall-clock comparisons lacking a monotonic guarantee still applies and
still matters for the single-host clock-step case, but it is not the only
source of read-path risk. A backward clock step, or ordinary web/worker
skew, between the write and a later read would make production's own TTL
read stale-live too, not just this test's assertion — a real,
product-relevant robustness question, not dismissible as a test artifact.
This pass does not claim the observed failure *was* a clock-related race
of any kind (the worker-refresh mechanism below remains the
better-supported explanation for this specific incident), only that the
scenario's product-vs-test classification was wrong as originally stated.
Refreshing `expires_at` on worker activity is deliberate, sensible
production behavior in its own right, so under the worker-refresh
hypothesis the defect is in the test's assumption that a fixed sleep
leaves no room for the job's own worker to touch the record, not in the
store — a test defect there. **Neither is a rendered verdict**: n=1, no
rerun evidence, and neither has been isolated (e.g. by
asserting on `updated_at` to see which write, if either, actually fired).
Recorded in the ledger rather than acted on, per this role's own bar — a
fix here without a rerun-rate baseline would be exactly the "retry in
disguise" the hard gate exists to block, even though no retry is actually
being proposed.

**`cache_stampede`/`sim_fault_plan`: unchanged. `live_upgrade`: one new
organic hit, live, after the sampled window closed.** **Correction
(post-review, via a twelfth Codex review comment on PR #2711): this
section and the measurement table below still said "zero new hits" and
"unchanged" after the late 34591670807 hit was already logged elsewhere
in this report — inconsistent with itself.** Run 34591670807 (`Test
(ubuntu-latest)`, this PR's own CI, 2026-09-11T11:51:57Z) is a second
occurrence of the `status: 0` signature; see the Symptom section and the
ledger's `live_upgrade` entry for the full writeup, including why a
non-coverage-job hit weakens the coverage-instrumentation hypothesis for
that signature. `manual-macos-contention-check.yml` — macOS-only,
plain `cargo test --workspace` — is still undispatched. **Correction
(post-review, via a third Codex review comment on PR #2711): a clean
dispatch would not "close" `cache_stampede` or `sim_fault_plan` either,
even setting the Linux gap aside.** Both are still undiagnosed — no named
mechanism, no test-vs-product verdict, no fix — and this role's own hard
gate requires a diagnosis and a fix verified against a baseline before an
entry closes; a 0/20 baseline rerun of an undiagnosed test is a data point
toward triage, not the after-measurement half of a fix that doesn't exist
yet. Dispatching the harness would supply exactly that: a same-commit
baseline for both, and (per the ledger's own corrected accounting) partial
evidence for `live_upgrade`'s macOS-observed signature — worth doing on
all three counts, but "close" overstated what a clean run alone can do for
any of them.

## 🔧 Treatment

None shipped by this pass — no finding here clears the hard gate for a fix.

- **`docs/ci-health/quarantine-ledger.md` updated**: new entry for
  `job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`
  with the mechanism hypothesis above, explicitly marked n=1/not campaigned/
  not quarantined (the Docker sweep is unmodified — this test keeps running
  on every sweep, since one unconfirmed hit is not grounds to skip it). Also
  added a dated note to the `live_upgrade` entry recording this pass's
  zero-hits-in-window result, the harness's 4th idle day, and (added
  after the fact) the one hit that landed live on this PR's own CI after
  the window closed.
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
| `live_upgrade` (all three tracked signatures) | 1 (late, post-window: the `status: 0` signature, run 34591670807) | 2 for the `status: 0` signature specifically (2026-09-09, 2026-09-11); unchanged for the other two | Verdict rendered for 1 of 3 signatures (the Linux "new build never served" hit, PR #2645's mechanism 2); the macOS connect-error cluster is attributed to the earlier #2510, and the third (`status: 0`) signature remains unattributed to any fix — now a confirmed repeat, and observed on a non-coverage job for the first time. CI-native verification still blocked on the undispatched harness |
| `cache_stampede` (line 501) | 0 | 2 (2026-09-03, 2026-09-09) | Undiagnosed |
| `sim_fault_plan` | 0 | 1 (2026-09-03) | Undiagnosed |
| `job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it` | 1 | 1 (new, 2026-09-10T19:54Z) | New; mechanism hypothesis recorded, not campaigned |

`manual-macos-contention-check.yml`: 0 → 0 `workflow_dispatch` runs, 4th
consecutive pass with no change (idle since it became dispatchable
2026-09-08T15:07:44Z — now ~66.5 hours).

**Correction (post-review, via an eleventh Codex review comment on PR
#2711): the page-1-only query cannot itself prove the window held only
100 runs.** `perPage=100, page=1` returned exactly 100 rows — the page-size
ceiling — so without checking page 2, a window that actually holds more
than 100 runs would silently drop its oldest entries off page 1, and nothing
in the original query would reveal that. This repo's own `total_count` grew
visibly during this pass (~7369 → ~8191 across successive calls), confirming
continuous concurrent writes that can also shift page boundaries between
one fetch and the next. Checked directly: page 2 of the identical query
(`event=pull_request, status=completed, perPage=100`) returned rows spanning
`2026-09-09T10:07:59Z`–`2026-09-10T10:46:39Z` — overlapping past page 1's
recorded minimum (`10:30:30Z`) up to `10:46:39Z`, exactly the pagination
drift the comment warned about. Every run in that overlap
(`>= 2026-09-10T09:48:19Z`) was inspected: 16 total, all either `cancelled`
or `success` except one `failure` — run 34467823999
(`vesper/bugbash-2635-api-token-error-response`), which is the same run
already identified above as the genuine WIP `api_token_error_response`
regression, not a new, previously-missed failure. So this specific
overlap check found no additional failures and no new tracked-signature
hits, but it does not prove page 1 was complete at its *far* edge (near
the window's stated end, `2026-09-11T09:09:24Z`) — that edge was never
independently checked against a later page, and wall-clock-bounded
pagination against a rapidly, concurrently written table is not a
reproducible sampling method in general. A future pass sampling this
repo should anchor the window to a stable reference (a specific run ID or
commit) rather than wall-clock time plus page count, or explicitly
paginate until reaching a run older than the intended cutoff.

## 🔬 Reproduce

```
actions_list(list_workflow_runs, ci.yml, event=pull_request, status=completed,
             perPage=100, page=1)
# → filtered to created_at > 2026-09-10T10:30:30Z (this pass's window):
#   100 runs, 61 cancelled / 25 success / 14 failure
# each failure's jobs via list_workflow_jobs(run_id, filter=latest),
# each failing job's log via get_job_logs(job_id, return_content=true)
# NOTE: page=1 alone cannot prove completeness under concurrent writes --
# see the correction above. Cross-checked against page=2 of the same
# query, which overlaps this window's near edge; no additional failures
# found there, but the window's far edge was not independently verified.
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
