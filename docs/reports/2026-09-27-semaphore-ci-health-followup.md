# 🚦 Semaphore: CI health follow-up — `Windows Tier 1 journey` cargo-metadata diagnostics gap found and closed; MinIO quarantine's remediation already in flight

Follow-up to `docs/reports/2026-09-25-semaphore-ci-health-followup.md`. That
pass quarantined three MinIO-dependent tests after `quay.io/minio/minio` cut
off anonymous pulls, and merged (#2953, 2026-09-25T13:14:35Z). This pass found
that remediation is already underway independently (PR #2968, open and CI-
green), and a new, previously-invisible failure signature on `Windows Tier 1
journey`: `autumn setup` fails "✗ Failed to read cargo metadata" with the
underlying `cargo` error never captured in the log. n=2 organic hits, ~32h
apart, on two unrelated branches. Fixed the observability gap (not the
underlying cause, which the gap itself made undiagnosable) this pass.

## 🎯 Verdict path

Required gate is still `Test suite` (`test-gate`), fed by `[test, trybuild,
test-features, test-docker]`, plus `Supply chain (cargo-deny)`. `Windows Tier
1 journey` is a separate, non-`test-gate` required check (platform-support
policy, issue #1616 AC-3) — not aggregated, so a red run there blocks merge on
its own. `manual-macos-contention-check.yml` remains dispatch-only — still
`total_count: 0`, an **18th consecutive idle pass** (~450h, past 18.7 days).
Not dispatched this pass: new macOS CI spend needs a human sign-off,
unavailable in this unattended run.

## 🌡️ Symptom

Sampled `ci.yml` `pull_request` runs, `perPage=30`, no `status` filter (the
`perPage=100` staleness the 2026-09-25 report flagged was not re-tested this
pass; `perPage=30`/page 1 returned current data reliably): 30 runs spanning
2026-09-25T18:13:15Z–2026-09-27T08:43:45Z (~38.5h) — 17 cancelled (all one
branch, `claude/friendly-ritchie-ip7w4v`, repeated pushes superseding their own
in-progress runs, not a health signal), 4 success, 9 failure. All 9 triaged at
job/log level:

| Run | Branch | Failing job(s) | Finding |
|---|---|---|---|
| 108204416273 (run 36172024269) | `vesper/bugbash-2288-commentable-author-name` | `Windows Tier 1 journey` | **New signature** — see below. |
| 108534929493 (run 36287398343) | `vesper/bugbash-2415-multipart-type-case` | `Windows Tier 1 journey` | Same signature as above, ~32h later. |
| 36283305996 | `dependabot/github_actions/dtolnay/rust-toolchain-1.120.0` | `MSRV (1.88.0)`, `Test (ubuntu/macos/windows-latest)` | Already-documented action-pin-bump break (own subject matter, unmerged). |
| 36279686228 | `fix/2282-rename-into-comments-keeps-columns` | `Lint`, `Test suite` | Branch-owned WIP, not triaged further (own diff). |
| 36267570138 | `vesper/bugbash-2292-translatable-drift` | `Lint`, `Test suite` | Branch-owned WIP. |
| 36216288860 | `vesper/bugbash-2662-parent-pk-override` | `Lint`, `SQLite runtime (feature=sqlite)`, `Test suite` | Branch-owned WIP. |
| 36172117934 | `vesper/bugbash-2445-capacity-probe-count` | `Lint`, `Test suite` | Branch-owned WIP. |
| 36172085068 | `vesper/bugbash-2291-admin-translatable-model-ast` | `Lint`, `Test suite` | Branch-owned WIP. |
| 36172056884 | `vesper/bugbash-2285-cross-record-refusal` | `SQLite runtime (feature=sqlite)`, `Lint`, `Test suite` | Branch-owned WIP. |

The two `Windows Tier 1 journey` hits are the new finding: identical output
both times, immediately after `autumn doctor` completes normally (29
passed/4 warned/1 failed — only the expected pre-setup `tailwind_binary`
warning) —

```
✗ Failed to read cargo metadata
Exception: ...\6.ps1:6
autumn setup failed with 1
##[error]Process completed with exit code 1.
```

Neither triggering branch's own diff touches Windows code, `autumn setup`, or
the cargo-metadata helpers in `autumn-cli`.

## 🔍 Diagnosis

**Mechanism: unconfirmed — the missing capture *is* the finding.** `autumn
setup` resolves the scaffolded `tier1_app`'s manifest via `cargo metadata
--format-version=1 --no-deps` (`autumn-cli/src/build.rs:875`'s
`read_cargo_metadata`, duplicated near-identically in
`autumn-cli/src/routes.rs:280` and `autumn-cli/src/dev.rs:1916`). All three
call sites, on a non-zero `cargo` exit, printed only a generic `"✗ Failed to
read cargo metadata"` and exited — `cargo`'s own stderr was captured by
`Command::output()` but never written anywhere, so neither occurrence's CI log
shows *why* `cargo metadata` failed (network/index-fetch failure against a
scaffold's fresh `Cargo.toml`, Windows path-length or antivirus-lock
contention, disk pressure, or something else). A related, less-diagnosable gap
was already flagged once before for this same job: the ledger's 2026-09-22
update noted `claude/intelligent-wright-vvhnue`'s `Windows Tier 1 journey`
failure whose logs 404'd entirely and were never investigated — a different
proximate cause (log retention vs. a swallowed stderr), but the same "this job
failed and nobody can see why" shape, now confirmed twice with readable logs
that still didn't show the reason.

**Test-vs-product: not renderable yet.** A scaffold-generation defect
(product) and a transient runner/network issue (infrastructure) are
indistinguishable from the generic message alone — asserting either without
the actual `cargo` error would be exactly the folklore this role's rules
exist to prevent.

## 🔧 Treatment

**An observability fix, not a flake fix — the distinction the hard gate
exists to enforce.** No rerun campaign, no retry, no tolerance widened.
`read_cargo_metadata`/`find_binary_in_profile`/`cargo_metadata` in `build.rs`,
`routes.rs`, and `dev.rs` now print `String::from_utf8_lossy(&output.stderr)`
alongside the existing message before exiting, so the next occurrence's actual
`cargo` error reaches the CI log. Left untouched: `try_cargo_metadata`
(`dev.rs:1933`), the deliberately-silent best-effort path lifecycle commands
like `autumn serve stop` rely on to keep working against a broken manifest —
printing there would defeat its documented purpose.

This is also a real, if narrow, CLI-user-facing improvement outside CI: anyone
running `autumn setup`/`build`/`routes` locally against a broken workspace
manifest previously got no indication of what `cargo` objected to. Noted in
`changelog.d/cargo-metadata-stderr-diagnostics.md`.

Filed as a new entry under "Under active investigation, not yet quarantined"
in `docs/ci-health/quarantine-ledger.md`, n=2, mechanism unconfirmed, with the
fix and the reasoning above recorded in full. Not quarantined: `Windows Tier 1
journey` keeps running unchanged on every PR.

**Separately, and not this pass's own work**: PR #2968 ("Pull MinIO from
Chainguard's free image and restore the quarantined S3 tests"), opened
2026-09-27T00:05:56Z by another session at the repo owner's request, already
addresses the open MinIO quarantine entry from the 2026-09-25 report —
independent of this ledger's own tracking, the same "whoever hits the failure
fixes it" pattern the escape entries above have already documented twice.
Its own `ci.yml` run (108281464452 → workflow run 36281464452) completed
`success` at its current head (`5cc14ebf`), which is the same sha
`pull_request_read`'s status check reports as this PR's current head —
i.e. its `Test (Docker)` job has already exercised the real
`cgr.dev/chainguard/minio` pull CI-natively, not just locally. Recorded here
rather than duplicated: once #2968 merges, the three `--skip` lines this
ledger's open MinIO entry names should be removable and that entry closable
in a future pass.

## 📊 Measurement

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `Windows Tier 1 journey` cargo-metadata stderr | Discarded (all 3 call sites) | n=2 organic hits found, both undiagnosable from the log alone | Printed going forward; mechanism still open |
| `live_upgrade`/`cache_stampede`/`sim_fault_plan`/`job_tracking_stores_integration` | Uncampaigned/closed, per 2026-09-25 report | No new hits among the 9 triaged failures this pass | Unchanged |
| MinIO quarantine (offsite_backup x2, sqlite_replication_s3) | Open, revisit-by 2026-10-02 | Remediation PR #2968 open, CI-green at its head | Open — awaiting merge, tracked for a future pass |
| `manual-macos-contention-check.yml` dispatches | 0 (16 idle passes, ~403h) | 0 (18th idle pass, ~450h) | Needs human sign-off for CI spend |

No before/after rerun-rate table for the new `Windows Tier 1 journey` entry:
this pass's fix targets a diagnosability gap, not test determinism, so there
is nothing to rerun yet — the next occurrence is the measurement. Revert
check: not applicable in the usual sense (nothing about pass/fail behavior
changed on the success path); reverting the three `eprintln!` additions would
restore the exact silent-failure shape both 2026-09-25 and 2026-09-27
occurrences hit.

Local verification for the diagnostics fix: `cargo check -p autumn-cli`,
`cargo fmt -p autumn-cli -- --check`, and `cargo clippy -p autumn-cli
--all-targets -- -D warnings` all clean. No Windows runner available in this
sandbox to reproduce the original failure directly — CI-native confirmation
is pending this PR's own `Windows Tier 1 journey` run, which exercises the
changed code path only if it fails again (a passing run proves nothing new
about the fix, since the added `eprintln!` is unreachable on success).

## 🔬 Reproduce

Confirm this pass's sampling:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=ci.yml, event=pull_request, perPage=30, page=1)
# -> 30 runs in [2026-09-25T18:13:15Z, 2026-09-27T08:43:45Z], 9 failures
```

Confirm the `Windows Tier 1 journey` signature:

```
get_job_logs(job_id=108204416273, return_content=true, tail_lines=100)
get_job_logs(job_id=108534929493, return_content=true, tail_lines=100)
# -> both: "✗ Failed to read cargo metadata" / "autumn setup failed with 1",
#    no cargo stderr in either
```

Confirm the fix's reach (three call sites, one deliberately excluded):

```
grep -n "Failed to read cargo metadata" autumn-cli/src/*.rs
# -> build.rs:882, routes.rs:287, dev.rs:1923 (now each followed by an
#    eprintln! of output.stderr); dev.rs's try_cargo_metadata (~1938) has no
#    such message by design and is unchanged
```

Confirm PR #2968's CI-native status:

```
search_pull_requests(query="repo:autumn-foundation/autumn head:claude/free-minio-ci-image-54gt5a")
# -> PR #2968, open
pull_request_read(method=get_status, pullNumber=2968)
# -> sha 5cc14ebf..., state "pending" (no status contexts posted this way)
actions_list(list_workflow_runs, resource_id=ci.yml, event=pull_request,
             workflow_runs_filter={branch: claude/free-minio-ci-image-54gt5a})
# -> run 36281464452, head_sha 5cc14ebf... (matches), conclusion: success
```

Confirm the macOS harness is still undispatched:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=manual-macos-contention-check.yml,
             event=workflow_dispatch)
# -> total_count: 0
```
