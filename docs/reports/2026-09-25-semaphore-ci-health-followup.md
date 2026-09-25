# 🚦 Semaphore: CI health follow-up — `quay.io/minio/minio` anonymous pulls cut off, failing every PR reaching the Docker sweep

Follow-up to `docs/reports/2026-09-24-semaphore-ci-health-followup.md`. That
pass found zero new hits and confirmed the `sqlite_job_backend_tracks_job_status_durably`
fix (#2925) holding. This pass found a new, total, deterministic
external-dependency outage dominating every CI failure sampled: `Test (Docker)`
— and therefore the required `Test suite` (`test-gate`) aggregator — currently
fails on every PR whose run reaches it, independent of that PR's own diff,
because `quay.io/minio/minio` (the fallback registry #2740 switched to in
September after Docker Hub deleted the `minio/minio` org outright) has also
stopped serving anonymous pulls. This pass quarantines the three affected
tests with full paperwork and flags the remediation decision (pay for
authenticated `quay.io` pulls, or host a mirror) for a human, per this role's
own "ask before new CI spend" rule.

## 🎯 Verdict path

`trunk-dev`'s own most recent push (the 2026-09-24 follow-up's PR #2942, a
docs-only ledger change) itself went red on `Test (Docker)` after merging —
direct evidence this is not attributable to any PR's diff. The required gate
is still `Test suite` (`test-gate`), fed by `[test, trybuild, test-features,
test-docker]`, plus `Supply chain (cargo-deny)`. `manual-macos-contention-check.yml`
remains dispatch-only — still zero `workflow_dispatch` runs, now a **16th
consecutive idle pass** (~403 hours, past 16.8 days). Not dispatched this
pass: new macOS CI spend needs a human sign-off, unavailable in this
unattended run.

## 🌡️ Symptom

**Tool-reliability note first**: `list_workflow_runs(event=pull_request)` at
`perPage=100` (with or without `status=completed`) consistently returned a
stale page this pass — runs from 2026-09-03/04, not current, on repeated
calls with varying `total_count` each time. Dropping to `perPage=30` with no
`status` filter returned current data reliably, but only at page 1; page 2
again jumped back to 2026-09-03. This is a worse instance of the pagination
instability the 2026-09-24 report already flagged (its own page-2 call
landed in 2026-09-07–09). Sampled the one reliable window available: 30
`ci.yml` `pull_request` runs spanning 2026-09-24T20:21:13Z–2026-09-25T08:05:14Z
(~11.7h) — 5 failures. **Coverage gap, recorded rather than hidden**: the
~10.6h between this window's start and the prior pass's cutoff
(2026-09-24T09:42:24Z–20:21:13Z) was not independently sampled — the
`perPage=100` staleness left no reliable way to reach it this pass. Cancelled
runs inside the sampled window were not inspected at job level (same caveat
as every pass since 2026-09-15).

All 5 failures, plus `trunk-dev`'s own push of PR #2942, triaged at job/log
level:

| Run | Branch | Failing job(s) | Finding |
|---|---|---|---|
| 36004498723 | `trunk-dev` (push, PR #2942) | `Test (Docker)`, `Test suite` | `quay.io/minio/minio` anonymous pull rejected — docs-only PR, proves the failure tracks the dependency, not the diff. |
| 36111010522 | `claude/brave-goldberg-h3c4cr` | `Test (Docker)`, `Test suite` | Same signature. |
| 36104177541 | `claude/wizardly-wright-dyva9u` | `Test (macos-latest)`, `Test (Docker)`, `Test suite` | Same `Test (Docker)` signature (the `macos-latest` failure was not triaged separately — out of scope for this outage, not yet attributed). |
| 36104008311 | `claude/friendly-ritchie-nv7uw7` | `Test (Docker)`, `Windows Tier 1 journey`, `Test suite` | Same `Test (Docker)` signature (the Windows failure likewise not triaged separately). |
| 36089093184 | `claude/busy-cerf-0i7k5y` | `Test (Docker)`, `Test suite` | Same signature. |
| 36054289214 | `claude/tender-galileo-q43orv` | `Test (Docker)`, `Test suite` | Same signature. |

**6 for 6.** Every `Test (Docker)` failure sampled, across 5 independent WIP
branches plus this repo's own trunk-dev push, panics identically at
`autumn-cli/tests/integration/offsite_backup.rs:82:10` (both `offsite_backup`
tests) with:

```
start MinIO — is Docker running?: Client(PullImage { descriptor:
"quay.io/minio/minio:RELEASE.2025-09-07T16-13-09Z", err:
DockerResponseServerError { status_code: 500, message: "unauthorized: access
to the requested resource is not authorized" } })
```

## 🔍 Diagnosis

**Confirmed directly against the registry, not inferred from the CI error
text alone.** An anonymous `quay.io/v2/auth` token request for
`repository:minio/minio:pull` succeeds (200) but the returned JWT's `access`
grant carries `"actions":[]` — empty, no `pull` permission — and a manifest
`GET` against `quay.io/v2/minio/minio/manifests/RELEASE.2025-09-07T16-13-09Z`
with that token still 401s. The identical probe against an unrelated public
quay.io repository, `quay.io/prometheus/prometheus`, succeeds normally (200,
real manifest) — so this is scoped specifically to `minio/minio`, not a
quay.io-wide policy change or outage. The repository's own web page
(`quay.io/repository/minio/minio`) still returns 200, so it exists and is
browsable; only anonymous registry pulls are cut off.

**Mechanism**: unpinned/vanished external dependency, the same category
already tracked in this ledger's closed MinIO/Docker-Hub entry — but this
time it recurs against the very fallback (`quay.io`) that entry's own fix
(#2740) switched to in response to Docker Hub deleting the `minio/minio` org
outright in October 2025. MinIO Inc. appears to have now closed off anonymous
pulls on its last remaining public registry too.

**Test-vs-product verdict**: neither. Pure external CI/test infrastructure
dependency (a third-party vendor's container-distribution policy); no
product code path is implicated.

**Remediation search, before quarantining rather than after**:
- `docker.io/minio/minio` — still gone (the 2025 org deletion; not
  re-verified in depth this pass).
- `docker.io/bitnami/minio` — Docker Hub's repository API reports it active
  and non-private with 58M+ historical pulls, but its registry tags list
  (`GET /v2/bitnami/minio/tags/list`, with and without a token, and via
  `hub.docker.com`'s own tags API) returns **zero tags** — consistent with
  Broadcom's 2025 Bitnami Secure Images move, which pulled free-tier tags
  behind a paid catalog while leaving the repository shell in place.
- `ghcr.io/minio/minio` and `public.ecr.aws/minio/minio` — both 401.

No anonymously-pullable MinIO-compatible replacement was found this pass.

## 🔧 Treatment

**Quarantined, not fixed with a registry swap**, per two constraints this
pass could not clear: (1) no Docker daemon is available in this sandbox to
verify a replacement image's compatibility with
`testcontainers_modules::minio::MinIO`'s wait strategy and env-var
expectations before committing to one — and (2) this repo's own history
shows what a blind registry swap costs: the original Docker Hub→quay.io fix
(#2740) collided with three other independent same-day fixes for the
identical outage, needing two separate reconciliation commits and ~8.3h of
spurious Lint failures across 5 branches (see the closed escape entry in the
ledger). Swapping again without a confirmed, verified target risks repeating
that, and this time no verified target exists.

Instead, added `--skip <exact test name>` for the three affected tests to
`ci.yml`'s two consolidated Docker sweeps — the same "skip by exact name"
convention CLAUDE.md already documents for the non-Docker generator-shaped
skips in the same blocks:

- `cli_tests` sweep: `offsite_backup_upload_then_restore_round_trips`,
  `offsite_backup_uploads_large_artifact_via_multipart`.
- `integration_tests` sweep: `replicates_to_and_restores_from_a_real_s3_endpoint`.

`examples/reddit-clone/tests/avatar_s3_integration.rs`'s
`avatar_blob_store_roundtrip` hits the identical outage but was never part of
either CI sweep to begin with (per the closed MinIO entry), so no `ci.yml`
change was needed there.

Filed as a new **Open entry** in `docs/ci-health/quarantine-ledger.md` with
the full intake form: owner named as the repo owner (the remediation needs a
business/infra decision — pay for authenticated `quay.io` pulls, or host a
mirror — that this role cannot make unilaterally), a 2026-10-02 revisit date,
and the skip mechanism named explicitly. This is this ledger's **first**
entry to actually use the intake form's quarantine mechanism rather than
being closed same-pass.

Also added 2026-09-25 dated verification updates to the `live_upgrade`,
`cache_stampede`, and `sim_fault_plan` entries (zero new hits in the reliable
window, with a note that this outage made the sample less informative than
usual for those signatures specifically — since a hit hiding behind an
earlier `Test (Docker)` panic in the same run can't be ruled out from a
failure-only triage), and to the closed `sqlite_job_backend_tracks_job_status_durably`
entry (still holding).

## 📊 Measurement

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `Test (Docker)` required-gate failures, sampled window | — | 6/6 (5 PRs + trunk-dev), 100% | Quarantined; should no longer block unrelated PRs once this PR merges |
| `quay.io/minio/minio` anonymous pull | Working (since #2740, 2026-09-12) | Confirmed broken via direct registry probe (empty-actions token, 401 on manifest GET) | Open — needs a human remediation decision |
| `live_upgrade`/`cache_stampede`/`sim_fault_plan` | Uncampaigned, 15 consecutive clean passes | No new organic hits among inspected runs (sample confounded by the outage above) | Unchanged |
| `sqlite_job_backend_tracks_job_status_durably` | Closed 2026-09-23, holding | 0 hits (all 6 in-window failures are the new signature) | Unchanged, still closed |
| `manual-macos-contention-check.yml` dispatches | 0 (15 idle passes, ~378.6h) | 0 (16th idle pass, ~403h) | Needs human sign-off for CI spend |

No before/after rerun-rate table for the new entry: this is a deterministic
100% external outage, not a stochastic flake, so a rerun campaign would not
add information beyond the 6/6 clustering and the direct registry probes
already gathered. Revert check: not applicable in the usual test-determinism
sense (nothing about test logic changed), but the failure this quarantine
routes around is fully reproducible pre-fix (all 6 run IDs above, plus the
direct `curl` probes in the ledger entry) and specific to this one registry
repository, not a red herring — restoring the two skipped tests without a
working registry would reproduce the identical failure immediately.

## 🔬 Reproduce

Confirm this pass's sampling and its 6-for-6 clustering:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=ci.yml, event=pull_request, perPage=30, page=1)
# -> 30 runs in [2026-09-24T20:21:13Z, 2026-09-25T08:05:14Z], 5 failures

actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=ci.yml, workflow_runs_filter={branch: trunk-dev, event: push})
# -> run 36004498723 (PR #2942's own trunk-dev push), conclusion: failure

get_job_logs(job_id=<Test (Docker) job id for each run above>)
# -> identical panic at autumn-cli/tests/integration/offsite_backup.rs:82:10
#    for all 6, quay.io/minio/minio pull, 500 "unauthorized"
```

Confirm the registry-level cause directly:

```
curl -sS "https://quay.io/v2/auth?service=quay.io&scope=repository:minio/minio:pull"
# -> 200, but the returned JWT's "access" grant has "actions":[] (no pull)

curl -sS -H "Authorization: Bearer <that token>" \
     "https://quay.io/v2/minio/minio/manifests/RELEASE.2025-09-07T16-13-09Z"
# -> 401, www-authenticate: Bearer ...

# Control — an unrelated public quay.io repo, same probe shape:
curl -sS -H "Authorization: Bearer <token scoped to prometheus/prometheus:pull>" \
     "https://quay.io/v2/prometheus/prometheus/manifests/latest"
# -> 200, real manifest — confirms this is scoped to minio/minio, not quay.io-wide
```

Confirm the macOS harness is still undispatched:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=manual-macos-contention-check.yml,
             event=workflow_dispatch)
# → total_count: 0
```
