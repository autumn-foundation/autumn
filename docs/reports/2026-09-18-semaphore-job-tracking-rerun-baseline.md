# 🚦 Semaphore: job_tracking rerun harness dispatched — 1/50 (2%) baseline, same signature, mechanism still open

Same-day follow-up to `docs/reports/2026-09-18-semaphore-ci-health-followup.md`
(which built `.github/workflows/manual-job-tracking-rerun-check.yml` but could
not dispatch it pre-merge) and PR #2845, which merged it to `trunk-dev`. No fix
PR — this run produces a real rerun-rate baseline but does not clear this
role's own hard gate (a named mechanism) for a determinism PR.

## 🎯 Verdict path

Unchanged. This report only concerns the standalone
`manual-job-tracking-rerun-check.yml` harness, not `ci.yml`'s own required
gates.

## 🌡️ Symptom / harness run

Once #2845 merged, the harness became dispatchable. First dispatch (run
35364903427) failed immediately at `actions/checkout` — a self-inflicted
input error: the `sha` input was the short 8-char hash `dd664e8e`, and
`actions/checkout` only takes the exact-commit fetch path for a full 40-char
SHA; a short one is treated as a ref-pattern glob that matches nothing.
Re-dispatched (run 35365077413) with the full SHA
`dd664e8e21be34beddd5f9b27280fde1d86ab6d2` and `iterations: "50"`; it ran
clean.

**Result: 1/50 failed, 49/50 passed** (~2%). Build finished 16:01:15Z, the
50-iteration loop finished 16:03:46Z (~2.6s/iteration once built). Iteration
26 panicked with the identical text this entry's two organic hits already
carry:

```
thread 'integration::job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it'
panicked at autumn/tests/integration/job_tracking_stores_integration.rs:264:5:
record should be past its configured TTL
```

This is the third confirmed occurrence of this exact signature (2026-09-11,
2026-09-15, and this harness run) — the first obtained from a controlled,
reproducible same-commit protocol rather than organic PR traffic. All other
49 iterations passed cleanly.

## 🔍 Diagnosis

Rate confirmed low (2%, comfortably under the 10% line that would have
called for ≥20 instead of ≥50 samples) — validates the ledger's own prior
assumption. **Mechanism still undetermined from this run**: the test's
`assert!(expired, "record should be past its configured TTL")` carries no
interpolated diagnostic value, and nothing in the test prints the row's
`status`/`updated_at` on failure, so this occurrence cannot be attributed to
either the demoted clock-step hypothesis or the better-supported
worker-refresh race (a `mark_running`/`settle_success` write landing inside
the 1.2s window) without more instrumentation than the test currently has.

Test-vs-product verdict: still not rendered.

## 🔧 Treatment

No fix PR. Per this role's own hard gate, a rerun-rate baseline without a
named mechanism does not clear the bar for a determinism PR — jumping to a
code change now would be exactly the "retry in disguise" this role exists to
refuse. Recommended next step, not done this pass: add temporary diagnostic
output only (select and print `status`/`updated_at` alongside
`expires_at`/`NOW()` right before the assert — not a tolerance change) so a
future campaign's failing iteration names which write, if any, touched the
row inside the window. Then re-run the harness at a larger `n` to have a
real chance of catching the mechanism in the log.

- **Ledger updated**: `job_tracking_stores_integration`'s entry gets the full
  dispatch account, the corrected `k/50`, and the instrumentation
  recommendation.

## 📊 Measurement

| Item | Before this pass | After this pass |
|---|---|---|
| `job_tracking_stores_integration` rerun-rate baseline | None (n=2 organic only) | **1/50 (2%), same-commit, same signature** |
| Mechanism | Two candidates from source reading, neither confirmed | Still two candidates, neither confirmed — this run added a confirmed occurrence, not a distinguishing observation |
| Total confirmed occurrences of this exact signature | 2 | 3 |

No revert check: nothing in product or test code changed this pass.

## 🔬 Reproduce

```
actions_run_trigger(run_workflow, owner=autumn-foundation, repo=autumn,
                     workflow_id=manual-job-tracking-rerun-check.yml,
                     ref=trunk-dev,
                     inputs={sha: "<full 40-char trunk-dev tip SHA>",
                             iterations: "50"})
```

Full iteration-by-iteration logs: artifact `job-tracking-rerun-logs` on run
35365077413 (id 10556686252, 14-day retention).
