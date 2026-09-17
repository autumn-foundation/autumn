# 🚦 Semaphore: CI health follow-up — third `live_upgrade` line-686 hit on a plain Test job, plus a new external-dependency signature

Follow-up to `docs/reports/2026-09-16-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR from
this pass — neither finding below clears this role's own rerun-campaign bar
for a determinism PR — but two things are worth recording: a third occurrence
of the `live_upgrade` line-686 signature, this time with the panic location
independently confirmed and, notably, on a plain `Test (ubuntu-latest)` job
rather than `Coverage (workspace)`; and a brand-new external-dependency
failure signature (`postgresql_embedded`'s build script hitting a GitHub API
rate limit) that failed the required `Test suite` gate on an otherwise
unrelated branch.

## 🎯 Verdict path

Unchanged: `trunk-dev` is green, and the required gate developers wait on is
`Test suite` (`test-gate`), fed by `[test, trybuild, test-features,
test-docker]`, plus `Supply chain (cargo-deny)`.
`manual-macos-contention-check.yml` remains dispatch-only — **still zero
`workflow_dispatch` runs**, now a 9th consecutive idle pass (~210.9 hours,
close to 9 days, since it became dispatchable at 2026-09-08T15:07:44Z,
checked 2026-09-17T~09:59Z).

## 🌡️ Symptom

Sampled `ci.yml` `pull_request`-triggered runs from the 2026-09-16 report's
own cutoff (2026-09-16T09:40:05Z, exclusive) to 2026-09-17T09:59:29Z (~24.3h,
combining a `status=completed`-filtered page covering the near edge of the
window with an unfiltered page covering the far/current edge — the
`status=completed` filter combined with `page=1` returned a stale,
weeks-old slice on this pass, a new instance of the pagination instability
this ledger has flagged before; omitting `status` avoided it) — 120 runs: 81
cancelled, 31 success, 7 failure, 1 in-progress (excluded from the failure
count below).

All 7 run-level failures triaged by job/log inspection:

- **5 were ordinary WIP-branch failures**, matching the daily pattern:
  - `codex/locate-density-test-and-separate-metrics` (run 35151718750): `Lint`
    (`Clippy`) and `SQLite runtime (feature=sqlite)` (`Clippy`, sqlite
    backend) both failed on the same run — the branch's own lint issue.
  - `vesper/bugbash-2321-alpn` (run 35172531629, first of two pushes this
    branch hit this window): `Lint` failed at the `Check formatting` step,
    and `Supply chain (cargo-deny)` failed because `cargo-deny`'s advisory
    step needs to resolve the dependency graph and the root `Cargo.lock` was
    stale (`cannot update the lock file .../Cargo.lock because --locked was
    passed`) — this branch's own in-progress dependency/formatting work, not
    a CI infra issue.
  - `vesper/bugbash-2321-alpn` (run 35174726637, second push, ~34 min later):
    four `Test` jobs failed at once — `Test (macos-latest)`, `Test
    (windows-latest)`, `Test tls`, `Test (ubuntu-latest)`. Checked the `Test
    tls` job's log directly: the failure is
    `tls::tests::server_config_with_resolver_and_client_auth_advertise_the_same_alpn`
    panicking at `./src/tls.rs:1545:10` on an `.expect()` over a
    `ClientCertVerifier` construction — squarely this branch's own new ALPN
    test (the branch name is literally `bugbash-2321-alpn`), consistent
    across all four platforms because it's the same assertion firing
    identically everywhere, not a platform-specific flake.
  - `vesper/macro-crate-split` (run 35150105970): another multi-job break on
    this same long-running crate-split branch this ledger already tracks —
    `Migration guide coverage`, `Supply chain (cargo-deny)`, `Test
    (windows-latest)`, `Test (macos-latest)`, `Test (ubuntu-latest)` all
    failed on one run. Checked the `Supply chain` job directly: `cannot
    update the lock file .../fuzz/Cargo.lock because --locked was passed` —
    the crate split hasn't updated `fuzz/`'s separate lockfile yet, the same
    in-progress-refactor shape this branch has shown on prior passes.
  - `vesper/bugbash-2405-prelayer-content-type` (run 35180088661): `Lint`
    (`Clippy`) failed — the branch's own lint issue.
- **1 is a third occurrence of the tracked `live_upgrade` line-686 signature,
  now on a plain `Test` job, not `Coverage`** — see Diagnosis.
- **1 is a brand-new external-dependency signature**, not previously
  recorded in this ledger — see Diagnosis.

### `live_upgrade` — third line-686 hit, first one on a plain `Test (ubuntu-latest)` job

Run 35089021085 (branch `claude/determined-bardeen-unefhv`, job `Test
(ubuntu-latest)`, completed 2026-09-16T12:14:00Z; this branch's own unrelated
`Clippy` fix-iteration later in the window is a separate, already-triaged WIP
failure, not this one). `test result: FAILED. 5 passed; 1 failed`, failing
test `upgrades_in_place_under_load_without_dropping_a_connection_or_the_state`.
The available tail (400 lines) did not reach the `thread '...' panicked at
...` banner line itself — the same truncation limitation prior passes have
hit on this test's voluminous per-request tracing — but the backtrace frame
for the test body itself resolves to **`./tests/live_upgrade.rs:686:5`**,
the exact line this ledger already tracks as the `status: 0`/unparseable-
response signature (first confirmed 2026-09-09/11, a probable third hit
already logged in the 2026-09-16 update). This occurrence's line number is
independently confirmed from the backtrace frame itself (not inferred from
result shape alone, as the 2026-09-16 report's second hit was), and — new
information — it ran on a plain `Test (ubuntu-latest)` job with **no
`cargo llvm-cov` coverage instrumentation at all**, further undermining any
residual hypothesis that this signature needs coverage-build overhead to
manifest (the 2026-09-11 hit already showed this once; this is a second,
independent confirmation).

### New signature: `postgresql_embedded` build script hits a GitHub API rate limit

Run 35126796648 (branch `claude/determined-bardeen-unefhv`, same branch as
the `live_upgrade` hit above, a later push, job `Test (Docker)`, completed
2026-09-16T17:53Z). The build failed compiling `postgresql_embedded v0.19.0`'s
build script:

```
error: failed to run custom build command for `postgresql_embedded v0.19.0`
--- stderr
Error: HTTP status client error (403 rate limit exceeded) for url
(https://api.github.com/repos/theseus-rs/postgresql-binaries/releases?page=1&per_page=100)
```

This is a `cargo build` step, not a test assertion — the crate's build
script fetches PostgreSQL binary release metadata from GitHub's REST API,
unauthenticated, and the shared IP range GitHub Actions runners draw from hit
that endpoint's per-IP rate limit. This failed the required `Test suite` gate
(via `test-docker`) on a PR whose own diff has nothing to do with
`postgresql_embedded` or Postgres tooling. Not previously recorded anywhere
in this ledger (checked by grep for `postgresql_embedded`, `theseus-rs`, and
`rate limit` — no prior hits).

## 🔍 Diagnosis

**`live_upgrade` line-686**: verdict still not rendered — this is now 3
occurrences of the same line/shape (2026-09-09/11 confirmed by message text,
today's confirmed by backtrace line number), still short of a rerun-rate
baseline. What today's occurrence adds is narrower, not conclusory: it rules
out "needs `llvm-cov` instrumentation" as a necessary precondition a second
time, on a different branch/day than the 2026-09-11 hit. It says nothing new
about product-vs-test.

**`postgresql_embedded` rate limit**: mechanism is clear and undisputed —
an unauthenticated third-party GitHub API call from a build script,
rate-limited by request volume from GitHub's own shared runner IP pool. This
is the same *structural* category this ledger already has two closed
examples of (the MinIO/Docker-Hub outage, the RUSTSEC advisory-database
disclosure): an external, this-repo-doesn't-control fact failing the
required gate independent of the triggering PR's own diff. Unlike those two,
this is **n=1** — a single occurrence, not (yet) the dozens-of-PRs pattern
that made the MinIO case a confirmed, deterministic, 100%-reproducing outage.
Whether this recurs depends on ambient GitHub API rate-limit pressure on
Actions runner IPs, which this pass has no way to measure from one data
point. Test-vs-product: neither — pure CI/build infrastructure, no product
code path involved, same as the two prior escapes of this shape.

## 🔧 Treatment

No fix PR — for `live_upgrade`, jumping to a code change off a 3rd
unconfirmed-baseline data point is exactly the "retry in disguise" this role
exists to refuse; the standing recommendation (dispatch
`manual-macos-contention-check.yml` at `samples: "20"`, and build the
still-missing Linux-shaped rerun harness) is unchanged and, at 9 idle passes
running close to 9 days, more overdue than ever.

For the `postgresql_embedded` finding, a fix would be premature at n=1: the
MinIO/RUSTSEC entries in this ledger both earned their fixes only after
confirming the failure was universal/deterministic (MinIO: 20/24 runs across
unrelated branches, a dead upstream registry) or externally verifiable
(RUSTSEC: a published advisory ID). Here there is exactly one occurrence, no
evidence yet of a rate-limit-tightening trend, and no confirmation that this
build step even runs in the required-gate path on every PR (it may depend on
which features/tests pull in `postgresql_embedded`, unlike MinIO's four
call sites which this ledger already mapped precisely). **Recommended next
step, not applied**: if this recurs, the fix shape is well-understood in
advance from the crate's own docs — set `GITHUB_TOKEN` (already present as
`GITHUB_TOKEN`/`secrets.GITHUB_TOKEN` in every Actions job) in the
environment `postgresql_embedded`'s build script reads, which raises the
per-request rate limit from the unauthenticated 60/hour to the authenticated
5,000/hour tier and would very likely make a single-occurrence rate limit hit
disappear. Not applied this pass because a one-off should not spend a
ledger entry's "fixed" status on an unconfirmed rate; logging it now so a
second occurrence is recognized as a repeat rather than re-diagnosed from
scratch.

- **Ledger updated**: `live_upgrade` entry gets a 2026-09-17 dated update
  recording the third line-686 occurrence and its plain-`Test`-job
  confirmation; a new entry is added under "Under active investigation, not
  yet quarantined" for the `postgresql_embedded` rate-limit signature.
- **No action needed** elsewhere — every other failure this pass found is
  squarely branch-owned WIP, including the ALPN test failures newly
  identified by log rather than assumed from the branch name alone.

## 📊 Measurement

| Item | This pass | Status |
|---|---|---|
| `live_upgrade` line-567 ("new build never served") | No occurrence | Unchanged, fixed by #2645 |
| `live_upgrade` `status: 0` / line-686 | 3rd occurrence (run 35089021085), line confirmed via backtrace, first on a plain `Test` job | Escalated, still uncampaigned |
| `live_upgrade` line-714 | No occurrence | Unchanged, n=1, undiagnosed |
| `cache_stampede` | No occurrence in any log inspected this pass | Unchanged, undiagnosed |
| `sim_fault_plan` | No occurrence | Unchanged, undiagnosed |
| `job_tracking_stores_integration` | No occurrence | Unchanged (n=2, not campaigned) |
| `postgresql_embedded` GitHub API rate limit | **New signature, n=1** (run 35126796648) | New, logged, not campaigned |
| `manual-macos-contention-check.yml` dispatches | 0 → 0 | 9th consecutive idle pass, ~210.9h |

## 🔬 Reproduce

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=250427287 (ci.yml's numeric workflow ID),
             event=pull_request, perPage=100, page=1)
# NOTE: adding status=completed to this exact query returned a stale page 1
# (dated 2026-09-03/04) across three repeated attempts this pass; omitting
# `status` and filtering client-side on `created_at`/`conclusion` avoided it.
# Filtered to created_at in (2026-09-16T09:40:05Z, 2026-09-17T09:59:29Z]
# each failure's jobs via list_workflow_jobs(run_id, filter=latest)
# each failing job's log via get_job_logs(job_id, return_content=true, tail_lines>=150)
```

Confirm the harness is still undispatched:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=350840451 (manual-macos-contention-check.yml),
             event=workflow_dispatch)
# → total_count: 0
```
