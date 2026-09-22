# 🚦 Semaphore: CI health follow-up — `crate_path` flake fixed and closed; `sqlite_jobs_scheduler_e2e` rerun harness built

Follow-up to `docs/reports/2026-09-21-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. This pass had
working network and toolchain access in its own sandbox (unlike several prior
passes), which let it follow through directly on the 2026-09-21 report's own
recommended next steps rather than deferring them again: the
`crate_path::tests::resolve_autumn_web_name_dashed_rename_is_sanitized` flake
(n=1, macOS-only) is root-caused, fixed, verified, and closed this pass; a
same-commit rerun harness for `sqlite_jobs_scheduler_e2e::sqlite_job_backend_tracks_job_status_durably`
(n=2, still open) is built, matching the pattern that closed
`job_tracking_stores_integration` — and, since a workflow_dispatch harness
isn't runnable until it reaches `trunk-dev`, its exact protocol was also run
locally by hand as an interim data point (`0/50` — does not close the entry,
see Measurement). Organic-hit sampling this pass found no new hits on any
other tracked signature.

## 🎯 Verdict path

Unchanged: `trunk-dev` is green; the required gate is `Test suite`
(`test-gate`), fed by `[test, trybuild, test-features, test-docker]`, plus
`Supply chain (cargo-deny)`. `manual-macos-contention-check.yml` remains
dispatch-only — still zero `workflow_dispatch` runs, now a **14th consecutive
idle pass** (~330.5 hours, past 13.75 days, since it became dispatchable
2026-09-08T15:07:44Z). Not dispatched this pass: new macOS CI spend needs a
human sign-off per this role's own rules, unavailable in this unattended run.

## 🌡️ Symptom

**Organic-hit sampling**, 2026-09-21T09:55:07Z (exclusive) to
2026-09-22T06:24:50Z (~20.5h), one `perPage=100`/`page=1` query whose own span
(2026-09-20T21:36:26Z–2026-09-22T06:24:50Z) fully covered the window with
margin on both ends — 60 `pull_request`-triggered `ci.yml` runs: 42 cancelled
/ 12 success / **6 failure**. All 6 triaged at job/log level:

| Run | Branch | Failing job(s) | Finding |
|---|---|---|---|
| 35671338439 | `dependabot/cargo/validator-0.21.0` | `Supply chain (cargo-deny)`, `Test (Docker)` | cargo-deny: already-documented `fuzz/Cargo.lock` `--locked` staleness. Docker: **own subject matter** — the `validator` 0.21 bump breaks `PostForm: IntoChangeset`'s `Validate` bound in `examples/reddit-clone/src/routes/posts.rs:539`, a genuine compile break from this PR's own dependency bump, unmerged. |
| 35640531737 | `dependabot/cargo/infer-0.22.0` | `Supply chain (cargo-deny)` | same already-documented `fuzz/Cargo.lock` staleness. |
| 35640495229 | `dependabot/cargo/jsonwebtoken-11.1.0` | `Lint`, `MSRV (1.88.0)`, `Supply chain (cargo-deny)` | Lint/MSRV: **own subject matter** — jsonwebtoken 11.1's `AlgorithmParameters` enum gained a variant, breaking an existing non-exhaustive `match` in `autumn/src/auth.rs:1447`. cargo-deny: same staleness pattern. |
| 35646111998 / 35630780507 | `claude/bold-heisenberg-t5yxtw` (two runs) | `Lint`, `SQLite runtime` | **branch-owned WIP** — a `cargo fmt` failure on one run, a genuine `autumn-web` lib compile error near `autumn/src/auth.rs:804` on the other; both this branch's own in-progress diff. |
| 35626501894 | `claude/intelligent-wright-vvhnue` | `Windows Tier 1 journey` | n=1, `get_job_logs` 404'd (likely log-retention/eviction) — not independently diagnosable this pass, logged as a gap rather than silently assumed branch-owned. |

None of the 6 match `live_upgrade`, `cache_stampede`, `sim_fault_plan`,
`job_tracking_stores_integration`, or `sqlite_job_backend_tracks_job_status_durably`.

## 🔍 Diagnosis

**`crate_path::tests::resolve_autumn_web_name_dashed_rename_is_sanitized` —
mechanism confirmed by reading `proc_macro_crate` 3.5.0's actual source, not
left as a hypothesis.** The 2026-09-21 entry's leading hypothesis (a
`proc_macro_crate` caching or locking gap) does not survive contact with the
dependency's own code: its internal cache is keyed by the literal
`CARGO_MANIFEST_DIR` string plus `Cargo.toml`'s mtime, and its one external
process call (`cargo locate-project --workspace --manifest-path=...`) is
spawned with an explicit `--manifest-path`, not a second env-var read — so
this crate's own cache was never the mechanism.

The real defect is in this repo's own test helper.
`with_fixture_manifest` (`autumn-macros-support/src/crate_path.rs`) calls
`tempfile_dir()` and writes the fixture `Cargo.toml` **before** entering
`temp_env::with_var`'s serializing lock. `tempfile_dir()`'s uniqueness came
entirely from `format!("...{}-{:?}", process::id(), SystemTime::now()...as_nanos())`
— every test in one `cargo test` binary shares one `pid`, so uniqueness rested
solely on the nanosecond timestamp differing across concurrent threads, which
is not guaranteed at whatever resolution the platform's clock offers under
contention. Two of this module's four `with_fixture_manifest` tests (which
`cargo test` runs concurrently by default) racing to the same directory name
would race their unprotected `fs::write` calls, letting one test's fixture
silently become another's. The one organic hit (`left: "autumn_web", right:
"autumn_web_05"`) is exactly consistent with this: `"autumn_web"` is
`DEFAULT_NAME`, precisely what
`resolve_autumn_web_name_falls_back_when_dependency_absent`'s own fixture (no
`autumn-web` dependency at all) would produce.

**Test-vs-product verdict, rendered before touching the fix: test defect.**
`tempfile_dir()` is a private helper inside `autumn-macros-support`'s own
`#[cfg(test)]` module, used by exactly the four tests in this file (confirmed
by grep); no production macro-expansion path or downstream crate is affected.
`resolve_autumn_web_name`'s real behavior and the `proc_macro_crate` dependency
itself were never implicated.

## 🔧 Treatment

Replaced the timestamp with a process-wide monotonic `AtomicU64` counter
(`unique_fixture_dir_name`, same file) — a counter can never repeat within a
process regardless of clock resolution, eliminating the race by construction.
No sleep, retry, or timeout was added anywhere. Also committed a permanent,
fast (no filesystem I/O) regression test,
`unique_fixture_dir_name_never_collides_under_concurrency` (64 threads ×
2,000 iterations, asserting no two generated names collide), so this failure
mode is now guarded by ordinary `cargo test` rather than left to the
Docker/macOS sweep's luck.

Separately, built `.github/workflows/manual-sqlite-jobs-rerun-check.yml` — the
`sqlite_jobs_scheduler_e2e` entry's own explicitly-named next step from
2026-09-21 — mirroring `manual-job-tracking-rerun-check.yml`'s shape (build
once, loop N times against a fresh SQLite file per iteration). Like that
harness, it needs no runner class or CI spend a human must sign off on.
`workflow_dispatch` only runs a workflow already on `trunk-dev`, so — since
this pass had a working toolchain — its exact protocol was also run locally
by hand: 50 iterations, **0/50 failed**. This is a data point, not a Tier 1
baseline: at n=2 organic in roughly a day of ambient traffic, the true rate
is plausibly low enough (~1-2%) that a 50-run miss carries meaningful
probability (≈60-90% under a naive binomial model) even if the bug is still
present — so it neither closes this entry nor changes its status. No fix
opens for that entry this pass: the mechanism (a second, unaudited SQLite
`INSERT ... ON CONFLICT` path, or a version-specific partial-index matching
quirk) is still unconfirmed, and n=2 organic (plus one inconclusive local
0/50) is not a Tier 1 baseline.

## 📊 Measurement

**`crate_path` — before/after from a purpose-built Tier 1 stress harness, plus
a real (not structural) revert check.**

| Protocol | Pre-fix | Post-fix |
|---|---|---|
| Standalone naming-scheme stress harness, 64 threads × 2,000 iters/run, 6 runs (768,000 samples total) | **158/768,000 collisions** (~0.021%; 26, 32, 29, 38, 33, 27 per run) | **0/768,000 collisions**, all 6 runs |
| Committed regression test, `--release`, 15 same-commit reruns | **15/15 FAILED** (revert check: old scheme reinstated, new test kept) | **15/15 passed** (fix restored) |

`cargo fmt -p autumn-macros-support -- --check` and `cargo clippy -p
autumn-macros-support --all-targets -- -D warnings` both clean (the one
warning present, `unknown lint: clippy::unused_async_trait_impl`, is the same
pre-existing, unrelated warning documented elsewhere in this ledger).
`cargo test -p autumn-macros-support` (full package, 40 tests) passes.

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `crate_path::…_dashed_rename_is_sanitized` | n=1, mechanism unconfirmed | Root-caused, fixed, 158/768,000→0/768,000, revert check 15/15→0/15 fail | **Closed** |
| `sqlite_job_backend_tracks_job_status_durably` | n=2, uncampaigned | No new organic hits; rerun harness built (not yet dispatchable — not on `trunk-dev` yet); harness's own protocol run locally by hand, `0/50` (data point, not a baseline — see below) | Under active investigation |
| `job_tracking_stores_integration` | Closed 2026-09-20 | No organic hits this pass | Unchanged, still closed |
| `live_upgrade` (3 signatures) | Uncampaigned | No new organic hits | Unchanged |
| `cache_stampede` | Uncampaigned | No new organic hits | Unchanged |
| `sim_fault_plan` | n=1, uncampaigned | No new organic hits | Unchanged |
| `manual-macos-contention-check.yml` dispatches | 0 (13 idle passes) | 0 (14th idle pass, ~330.5h) | Needs human sign-off for CI spend |

## 🔬 Reproduce

Confirm the `proc_macro_crate` cache is not the mechanism:

```
sed -n '150,300p' ~/.cargo/registry/src/*/proc-macro-crate-3.5.0/src/lib.rs
# cache keyed by CARGO_MANIFEST_DIR string + Cargo.toml mtime; the one
# subprocess call uses an explicit --manifest-path, not a second env read.
```

Reproduce the pre-fix collision rate and the post-fix absence of it:

```
cargo test -p autumn-macros-support --release \
  crate_path::tests::unique_fixture_dir_name_never_collides_under_concurrency \
  -- --exact
# post-fix: passes every time.
# pre-fix (temporarily restore the old
# format!("...{}-{:?}", pid, SystemTime::now()...as_nanos()) scheme,
# keeping the new test): failed 15/15 same-commit reruns in this pass's own
# sandbox.
```

Confirm the six triaged failures in this pass's window and the `crate_path`
fix's clean lint/test status:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=250427287, event=pull_request, perPage=100, page=1)
# -> 60 runs in [2026-09-21T09:55:07Z, 2026-09-22T06:24:50Z], 6 failures,
#    all six triaged above (own subject matter / branch-owned WIP / one
#    404'd-log gap, none matching a tracked signature)

cargo fmt -p autumn-macros-support -- --check
cargo clippy -p autumn-macros-support --all-targets -- -D warnings
cargo test -p autumn-macros-support
```

Confirm the macOS harness is still undispatched:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=manual-macos-contention-check.yml,
             event=workflow_dispatch)
# → total_count: 0
```
