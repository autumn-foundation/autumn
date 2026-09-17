# 🚦 Semaphore: CI health follow-up — a fourth `live_upgrade` line-686 hit, and a `test-docker` GitHub-rate-limit gap fixed

Follow-up to `docs/reports/2026-09-16-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR for
`live_upgrade` — it still doesn't clear this role's own rerun-campaign bar for
a determinism PR — but this pass **does** ship one small, mechanism-confirmed
CI infra fix: `test-docker`'s "Run Docker-dependent tests" step was missing
the `GITHUB_TOKEN` authentication that this exact same file's `coverage` job
and Windows Tier 1 journey job already carry for `postgresql_embedded`'s
GitHub-API-calling build script, and it hit the resulting 403 rate limit
today. Also recorded: a fourth observed occurrence of the tracked
`live_upgrade` line-686 signature (third if counting only occurrences
confirmed by exact panic text; today's is confirmed by backtrace line number
instead).

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
combining two queries — see Reproduce for both exactly as run — because a
`status=completed`-filtered `page=1` query returned a stale, weeks-old slice
on this pass, a new instance of the pagination instability this ledger has
flagged before; the workaround was a `status=completed`, `page=2` query for
the window's near edge plus an unfiltered `page=1` query for its far/current
edge, non-overlapping by timestamp) — 120 runs: 81 cancelled, 31 success, 7
failure, 1 in-progress (excluded from the failure count below). **Caveat
carried from the 2026-09-14/15 updates**: only the 7 run-level
failures/1 in-progress run were inspected at job level; the 81
`cancelled`-overall runs were not, so — since `ci.yml` runs with
`cancel-in-progress: true` and a job can fail before its run is superseded
and marked `cancelled` — the "no repeat" claims below for
`cache_stampede`/`sim_fault_plan`/`job_tracking_stores_integration` are
scoped to the 8 runs actually inspected, not proven-exhaustive across the
full 120-run window.

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
- **1 is a fourth observed occurrence of the tracked `live_upgrade` line-686
  signature** — see Diagnosis.
- **1 is a `test-docker` build failure with a known, already-fixed-elsewhere
  mechanism** — see Diagnosis and Treatment (this one gets a fix, not just a
  log entry).

### `live_upgrade` — a fourth observed line-686 hit

Run 35089021085 (branch `claude/determined-bardeen-unefhv`, job `Test
(ubuntu-latest)`, completed 2026-09-16T12:14:00Z; this branch's own unrelated
`Clippy` fix-iteration later in the window is a separate, already-triaged WIP
failure, not this one). `test result: FAILED. 5 passed; 1 failed`, failing
test `upgrades_in_place_under_load_without_dropping_a_connection_or_the_state`.
The available tail (400 lines) did not reach the `thread '...' panicked at
...` banner line itself — the same truncation limitation prior passes have
hit on this test's voluminous per-request tracing — but the backtrace frame
for the test body itself resolves to **`./tests/live_upgrade.rs:686:5`**, the
exact line this ledger already tracks as the `status: 0`/unparseable-response
signature. **Correcting the count against the ledger's own prior entries**:
2026-09-09 and 2026-09-11 are each confirmed by exact panic message text (2
confirmed); the 2026-09-16 report's run 35069353632 matched only on
line/shape, message text not confirmed (a 3rd *observed*, not confirmed,
hit); today's run adds a 4th observed hit, with the line independently
confirmed via the backtrace frame rather than inferred from result shape
alone, but still not by exact message text — the same evidentiary tier as
the 2026-09-16 hit, not a step up to "confirmed" the way the text-matched
2026-09-09/11 hits were. **Also correcting**: this is not the first
occurrence on a plain (non-`Coverage`) `Test` job — the 2026-09-11 hit was
already on `Test (ubuntu-latest)` per this ledger's own entry. Today's run
is a second occurrence on a plain `Test` job, reinforcing (not newly
establishing) that this signature isn't coverage-instrumentation-specific.

### `postgresql_embedded` build script hits a GitHub API rate limit — a gap in an already-known fix

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
`postgresql_embedded` or Postgres tooling.

**This is not a novel mechanism — `ci.yml` already names and fixes it twice,
just not at this third call site.** Grepping `ci.yml` for `postgresql_embedded`
turns up the `coverage` job's own comment: `` `postgresql_embedded`'s build
script downloads Postgres binaries via the GitHub API; unauthenticated it
hits the 60 req/hr rate limit and fails the build with a 403. Authenticate
with the job's token to get the higher rate limit. `` (job-level
`GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}`), and the Windows Tier 1 journey
job's "the app builds on Windows" step carries the identical fix with a
comment citing the coverage job by name (`"as the coverage job already
does"`). **The `test-docker` job's "Run Docker-dependent tests" step — which
runs `feature_flags_pg_integration` with the `managed-pg-bundled` feature,
the same feature that pulls in `postgresql_embedded` — had no such `env:`
block.** Confirmed by reading `ci.yml` directly: `test-gate`'s `needs:` is
`[test, trybuild, test-features, test-docker]`, unconditional, and
`test-docker`'s "Run Docker-dependent tests" step is gated only on
`runner.os == 'Linux'` (which `heavy_runs_on` always resolves to for
`pull_request` events) — not on any feature flag. So every PR reaching this
required shard compiles `postgresql_embedded` unauthenticated, exactly the
exposure Codex's review comment on this PR's own diff flagged as broader
than the original draft of this report stated.

## 🔍 Diagnosis

**`live_upgrade` line-686**: verdict still not rendered — 2 occurrences
confirmed by exact message text (2026-09-09/11), 2 more observed at the same
line/shape but not text-confirmed (2026-09-16, today), still short of a
rerun-rate baseline. What today's occurrence adds is narrower, not
conclusory: it's a second data point (after 2026-09-11) that this signature
doesn't need `llvm-cov` instrumentation to manifest. It says nothing new
about product-vs-test.

**`postgresql_embedded` rate limit**: mechanism is fully confirmed, not just
plausible — this is the *identical* known cause `ci.yml` itself already
documents and fixes in two other jobs (`coverage`, the Windows Tier 1
journey), just missing at this third call site (`test-docker`). Unlike the
MinIO/Docker-Hub and RUSTSEC entries this ledger closed only after
confirming universality across many runs, this fix doesn't need that step:
the root cause here isn't "does this happen often," it's "this job doesn't
authenticate a GitHub API call that two sibling jobs in the same file
already do, for the documented reason that it's otherwise rate-limited" —
established by reading the workflow file itself, not by a rerun campaign.
Test-vs-product: neither — pure CI/build infrastructure, no product code
path involved, same as the two prior escapes of this shape.

## 🔧 Treatment

No fix PR for `live_upgrade` — jumping to a code change off two
text-unconfirmed line/shape matches is exactly the "retry in disguise" this
role exists to refuse; the standing recommendation (dispatch
`manual-macos-contention-check.yml` at `samples: "20"`, and build the
still-missing Linux-shaped rerun harness) is unchanged and, at 9 idle passes
running close to 9 days, more overdue than ever.

**`postgresql_embedded`: fixed in this PR.** Added the same
`GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}` to `test-docker`'s "Run
Docker-dependent tests" step that `coverage` and the Windows journey job
already carry, with a comment naming the 2026-09-16 run that hit this and
pointing at the two existing instances of the same fix. This is not the
"raised timeout/added retry" this role bans — it's authenticating a call
that was already meant to be authenticated everywhere this crate compiles,
per the repo's own prior fixes; nothing about the test or the build's
determinism changes, only whether a legitimate GitHub API call gets the
60/hour or 5,000/hour rate limit tier. No before/after rerun-rate table is
attached because there's no rate to measure: this isn't a flaky test, it's
an unauthenticated call that fails whenever ambient rate-limit pressure
crosses a threshold this repo doesn't control the timing of, and the fix
is the same one already proven to work at the other two call sites — the
`coverage` job has run this build authenticated on every push since that
job's own fix landed, with no repeat of the 403.

- **Ledger updated**: `live_upgrade` entry gets a 2026-09-17 dated update
  correcting the observed-vs-confirmed count and recording that today's hit
  is a second (not first) plain-`Test`-job occurrence; the
  `postgresql_embedded` entry (added by this same PR) is updated from "open,
  not applied" to "fixed" with the `test-docker` env-block addition.
- **No action needed** elsewhere — every other failure this pass found is
  squarely branch-owned WIP, including the ALPN test failures newly
  identified by log rather than assumed from the branch name alone.

## 📊 Measurement

| Item | This pass | Status |
|---|---|---|
| `live_upgrade` line-567 ("new build never served") | No occurrence | Unchanged, fixed by #2645 |
| `live_upgrade` `status: 0` / line-686 | 4th observed occurrence (run 35089021085), 2nd on a plain `Test` job, line confirmed via backtrace (not text) | Escalated, still uncampaigned |
| `live_upgrade` line-714 | No occurrence | Unchanged, n=1, undiagnosed |
| `cache_stampede` | No occurrence in the 8 runs inspected at job level (not proven-exhaustive across all 120 — see caveat in Symptom) | Unchanged, undiagnosed |
| `sim_fault_plan` | No occurrence in the 8 runs inspected at job level (same caveat) | Unchanged, undiagnosed |
| `job_tracking_stores_integration` | No occurrence in the 8 runs inspected at job level (same caveat) | Unchanged (n=2, not campaigned) |
| `postgresql_embedded` GitHub API rate limit | New signature (run 35126796648) | **Fixed this PR** — `test-docker` now authenticates, matching `coverage`/Windows journey |
| `manual-macos-contention-check.yml` dispatches | 0 → 0 | 9th consecutive idle pass, ~210.9h |

## 🔬 Reproduce

Two queries were needed to cover the full window without hitting the stale
`page=1`+`status=completed` combination (see Symptom):

```
# Query A — covers the window's far/current edge (2026-09-16T14:43:12Z to
# 2026-09-17T09:59:29Z), all 100 runs land after the cutoff:
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=250427287 (ci.yml's numeric workflow ID),
             event=pull_request, perPage=100, page=1)
# NOTE: adding status=completed to this exact query (same page=1) returned a
# stale page dated 2026-09-03/04 across three repeated attempts this pass —
# omitting `status` avoided it.

# Query B — covers the window's near edge (2026-09-16T00:35:16Z to
# 2026-09-16T14:37:26Z); filter client-side to created_at >
# 2026-09-16T09:40:05Z (exclusive, the prior report's cutoff) to get the 20
# runs that land in this window:
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=250427287, event=pull_request, status=completed,
             perPage=100, page=2)

# A ∪ B, deduplicated by run id (no overlap — A starts after B ends) = 120
# runs: 81 cancelled, 31 success, 7 failure, 1 in-progress.
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
