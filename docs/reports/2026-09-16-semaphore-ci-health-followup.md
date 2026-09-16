# 🚦 Semaphore: CI health follow-up — two `live_upgrade` hits in one day, one a new signature

Follow-up to `docs/reports/2026-09-15-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR from
this pass — neither hit below clears this role's own rerun-campaign bar for a
determinism PR — but the day's sampling broke a six-pass streak of zero
organic `live_upgrade` hits, and one of the two hits is a signature this
ledger has never recorded before.

## 🎯 Verdict path

Unchanged: `trunk-dev` is green, and the required gate developers wait on is
`Test suite` (`test-gate`), fed by `[test, trybuild, test-features,
test-docker]`, plus `Supply chain (cargo-deny)`.
`manual-macos-contention-check.yml` remains dispatch-only — **still zero
`workflow_dispatch` runs**, now an 8th consecutive idle pass (~186.5 hours,
well over a week, since it became dispatchable at 2026-09-08T15:07:44Z,
checked 2026-09-16T~09:5xZ).

## 🌡️ Symptom

Sampled `ci.yml` `pull_request`-triggered runs from the 2026-09-15 report's
own cutoff (2026-09-15T09:39:00Z, exclusive) to 2026-09-16T09:40:05Z (~24h,
two `perPage=100` pages covering the window with margin on both ends) — 119
runs: 88 cancelled, 18 success, 13 failure.

All 13 run-level failures triaged by job/log inspection:

- **9 were a WIP branch's own `Clippy`/`Lint` failure** — ordinary
  in-progress work, not a CI health issue.
  `claude/determined-bardeen-unefhv` alone accounts for 4 (iterating on the
  same fix across pushes); `claude/eager-turing-gqo7ng` 2; three more
  single-branch hits (`claude/inspiring-ramanujan-95ccd6` once,
  `vesper/bugbash-2801-pdf-depth-warn` once,
  `vesper/bugbash-2370-dump-cache-coherence-env` once).
- **1 was `vesper/macro-crate-split`'s own multi-job break** — `Lint`,
  `MSRV`, `Plugin API contract`, `SQLite runtime`, `Edge capsule
  conformance`, `Sim sweep`, `Supply chain`, and `Migration guide
  coverage`'s `Markdown link gate` all failed on the same run, consistent
  with an in-progress crate-split refactor breaking many things at once
  rather than a CI infrastructure issue.
- **1 was a repo-hygiene self-check catching its own branch's in-progress
  edit**: `claude/inspiring-ramanujan-95ccd6`'s
  `pg_relative_delay_ci_coverage::pg_relative_delay_tests_are_named_in_ci`
  failed on `Test (windows-latest)` with `"job.rs no longer has a \`mod pg\`
  block inside \`mod tests\`"` — the same category as this ledger's own
  `edge_conformance_ci_coverage`/`cli_tests_cold_start_ignored_tests_are_ci_named`
  self-checks elsewhere: it exists to catch exactly this, and it did.

**The remaining 2 are both `live_upgrade`, both on `Coverage (workspace)`,
both organic** (neither triggering branch touches hot-upgrade code):

1. **Run 35044877808** (branch `claude/charming-planck-ag3glz`, completed
   2026-09-16T03:36:41Z): panic at
   `examples/hot-upgrade/tests/live_upgrade.rs:714:5`: `"the new build must
   accept writes after the cutover"` — **a signature not previously recorded
   in this ledger.** The connection-failure counter line printed immediately
   above the panic reads `refused=0 hard_failures_after_retry=0
   mid_flight_resets_retried=0 startup_barrier_hits_retried=0`. **Correction
   (post-review, via a Codex review comment on this PR): these four counters
   do not rule out all three of PR #2645's named mechanisms.** `refused` and
   `hard_failures_after_retry` are connection-outcome counters this ledger
   already attributes to PR #2510, not #2645; `mid_flight_resets_retried`
   predates #2645 too (the pre-existing `with_reset_retry` path). Of #2645's
   own three mechanisms, only the third (the startup-barrier retry) has a
   dedicated counter — `startup_barrier_hits_retried`, which reads 0 here,
   ruling that one out for this occurrence. The other two (the
   `wait_until_ready` startup-barrier poll, and the adaptive post-cutover
   wait replacing the old fixed 3.5s window) have no counter at all, so
   whether either was active when this write was rejected is unknown from
   this log alone. `test result: FAILED. 5 passed; 1 failed`, same shape as
   every other hit on this test.
2. **Run 35069353632** (branch `claude/friendly-ritchie-wol314`, completed
   2026-09-16T09:15:56Z): panic at `./tests/live_upgrade.rs:686:5`, `test
   result: FAILED. 5 passed; 1 failed` — same line number and same
   pass/fail shape as the 2026-09-11 `status: 0` hit (run 34591670807)
   already in this ledger. A 220-line tail of this job's log captured the
   backtrace and the preceding request-log spam but not the actual `thread
   '...' panicked at ...:686:5: <message>` banner line — it fell outside
   even that window, so the exact assertion text is **not independently
   re-confirmed this pass**. Recorded as consistent with, not confirmed as,
   the same `status: 0` signature, on line number and result shape alone.

Two hits in one day, breaking six straight zero-hit passes
(2026-09-09 through 2026-09-15), is itself a data point worth flagging even
though neither hit alone clears the rerun-rate bar.

## 🔍 Diagnosis

Verdict not rendered for either hit — both remain hypotheses pending a rerun
campaign, per this role's own bar. The line-714 signature is genuinely new:
it doesn't match either of the other two open signatures this ledger already
tracks (line-567 "new build never served," line-686 `status: 0`), and its
counters rule out one of PR #2645's three named mechanisms (the
startup-barrier retry) but not the other two, which have no counter to check
against (see the correction above). Whether it's a fourth independent timing
sensitivity in the test's own load-window design, or a real product-side
race in the write path specifically (as opposed to the read path the other
signatures concern), is unknown from a single occurrence — exactly the
ambiguity a rerun campaign exists to resolve before anyone touches code.

## 🔧 Treatment

No fix PR — neither hit has a rerun-rate baseline, and jumping to a code
change off n=1 (line-714) or an unconfirmed n=2 (line-686) would be the
"retry in disguise" this role exists to refuse. The standing recommendation
is unchanged in kind but more urgent in fact: dispatch
`manual-macos-contention-check.yml` (`samples: "20"`) against a `trunk-dev`
commit at or after `8fae8af`, and build the still-missing Linux-shaped rerun
harness for this test (the 2026-09-09/10/11 reports already called for one;
today's line-714 hit is a fourth reason). Both require a human decision —
new CI spend for the macOS harness, and engineering time to build the Linux
one — neither of which this pass can authorize on its own.

- **Ledger updated**: `live_upgrade` entry gets a 2026-09-16 dated update
  recording both hits, the new line-714 signature, and the harness's now
  8th-consecutive-idle-pass status.
- **No action needed** elsewhere — every other failure this pass found is
  squarely branch-owned WIP.

## 📊 Measurement

| Item | This pass | Status |
|---|---|---|
| `live_upgrade` line-552/567 | No occurrence | Unchanged |
| `live_upgrade` line-686 (`status: 0`) | Possible 2nd occurrence (run 35069353632) — line/shape match, exact message not re-confirmed | Escalated, unconfirmed |
| `live_upgrade` line-714 | **New signature, n=1** (run 35044877808) | New, undiagnosed |
| `cache_stampede` | No occurrence in any log inspected this pass | Unchanged, undiagnosed |
| `sim_fault_plan` | No occurrence | Unchanged, undiagnosed |
| `job_tracking_stores_integration` | No occurrence | Unchanged (n=2, not campaigned) |
| `manual-macos-contention-check.yml` dispatches | 0 → 0 | 8th consecutive idle pass, ~186.5h |

## 🔬 Reproduce

```
actions_list(list_workflow_runs, resource_id=250427287 (ci.yml's numeric ID
             — the "ci.yml" filename resolved to a stale/different page on
             this pass; the numeric workflow ID did not), event=pull_request,
             status=completed, perPage=100, page=1 and page=2)
# → filtered to created_at in (2026-09-15T09:39:00Z, 2026-09-16T09:40:05Z]
# each failure's jobs via list_workflow_jobs(run_id, filter=latest)
# each failing job's log via get_job_logs(job_id, return_content=true, tail_lines>=150)
#   (150-220 lines was enough to reach the backtrace on the Coverage
#   (workspace) jobs but not always the panic banner itself — the test's own
#   per-request access-log tracing is voluminous enough to push it out even
#   at tail_lines=220; a still-wider tail or the raw log blob URL is needed
#   to confirm exact panic message text, the same caveat prior passes noted)
```

Confirm the harness is still undispatched:

```
actions_list(list_workflow_runs, manual-macos-contention-check.yml, event=workflow_dispatch)
# → total_count: 0
```
