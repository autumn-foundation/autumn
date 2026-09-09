# CI quarantine ledger

Formalizes what the 2026-09-04 CI health census
(`docs/reports/2026-09-04-semaphore-ci-health-census.md`) found this repo
lacked: "no formal ledger exists in this repo (no intake-form/owner/date
convention)." Its one prior example — `cancelled_release_does_not_leak_lock`,
skipped out of `ci.yml`'s Docker sweep with a diagnosis comment but no owner
or diagnose-by date — has since been de-flaked and removed from the skip list
(#2479), so this ledger opens with **zero open entries**, not a backlog.

**No test enters quarantine without an entry here.** A `#[ignore]` or
`--skip` added to work around instability, with nothing recorded below, is
not quarantine — it is a graveyard with a skip annotation, and the next
person to find it has no way to tell a diagnosed, owned wait from an
abandoned one.

## Intake form

Copy this block into a new entry under "Open entries" when quarantining a
test. Every field is required — an entry missing one is not a valid
quarantine, per the rule above.

```
### <test path>::<test name>

- **Quarantined**: <date> in <PR #>
- **Owner**: <github handle> — the person who diagnosed it and is on the
  hook for closing this entry, not necessarily the original test author.
- **Diagnose-by**: <date> — a real date, not "TBD". Missing it means revisit
  this entry, not extend it silently.
- **Rerun-rate baseline**: <k>/<n> from <harness/command>, run <date>.
  Same-commit rerun statistics only — "it's flaky" is not a baseline.
- **Failure signature(s)**: <the actual error/panic text, or a link to one>.
- **Mechanism (if known)**: <root-cause category — shared state, missing
  await/async race, time/timezone dependence, order dependence, unpinned
  external service, resource contention, or product bug — plus the specific
  defect>, or "undiagnosed" if the ledger entry exists only to stop the
  bleeding while triage continues.
- **Linked issue/PR**: <link> — a product bug found via flake triage gets
  filed and linked here, per Semaphore's law 2 ("every flake is a bug — in
  the test or the product — until diagnosed you do not know which").
- **Skip mechanism**: <where in CI this is actually excluded — e.g. `ci.yml`
  `--skip` list, `#[ignore]`, a non-default feature gate> and why that
  mechanism was chosen over the others.
```

## Open entries

_None as of 2026-09-05._

## Closed entries

### `distributed_lock::cancelled_release_does_not_leak_lock`

- **Quarantined**: pre-existing before this ledger; exact date/PR not
  recoverable from `ci.yml`'s history alone — the original `--skip` carried
  a diagnosis comment ("flaky wall-clock zero-duration-timeout race; needs
  deterministic/paused time to de-flake") but no owner or date, which is
  exactly the gap this ledger exists to close going forward.
- **Rerun-rate baseline**: 1/30, same-commit rerun protocol (testcontainers
  Postgres), 2026-09-04.
- **Failure signature**: panic "the release should have been cancelled by
  the zero-duration timeout".
- **Mechanism**: `tokio::time::timeout(Duration::ZERO, ...)` assumed an
  already-elapsed timer always wins the poll race against the real
  `pg_advisory_unlock` round-trip; `Timeout::poll` polls the wrapped future
  before checking its timer, so a same-poll resolution never got cancelled.
  Test defect, not a product defect — the underlying `LockGuard`/
  `AcquireConn` cancel-safety this test exists to prove holds regardless
  (confirmed by the revert check: mutating `AcquireConn::drop` to recycle
  instead of force-close did not turn the test red).
- **Resolution**: rewritten to poll `release()` by hand exactly once and
  assert `Poll::Pending` — no timing dependency. 0/50 reruns after the fix,
  revert check passed. Un-quarantined and restored to the Docker sweep.
- **Closed**: 2026-09-04, #2479 (🚦 Semaphore).

## Under active investigation, not yet quarantined

These are tracked here because they are the subject of an open rerun
campaign, not because a skip has been applied — per Semaphore's own rule
that a raised timeout, added sleep, or added retry is not a valid response
to an unconfirmed flake. Do **not** add a `--skip`/`#[ignore]` for these
without also filling in the intake form above.

### `hot-upgrade::live_upgrade::upgrades_in_place_under_load_without_dropping_a_connection_or_the_state`

- **Observed**: 3/17 eligible `macos-latest` CI executions (14 confirmed, 3
  unresolved — see the 2026-09-04 census for the derivation), 0/16-17 on
  `ubuntu-latest`, organic PR-traffic sample, 2026-09-03/04.
- **New organic hit, 2026-09-09, breaks the macOS-only working hypothesis**:
  a 4th hit, but for the first time on a **Linux** runner and at a
  **different assertion**. Run 34317587464 (PR #2645, branch
  `claude/wizardly-wright-i1jsql`), job `Coverage (workspace)` (a plain
  hosted `ubuntu-latest` runner, confirmed via the job's own `labels` —
  `heavy_runs_on` only overrides the `test` job's `ubuntu-latest` matrix
  leg, and this run drew a standard hosted runner, not a contended
  self-hosted one), step "Generate coverage (workspace catch-all)" — this
  is `cargo llvm-cov`, which instruments every test binary it wraps. This
  job's own inline comment notes that roughly doubles `target/`'s on-disk
  *size*; no wall-clock runtime measurement was taken here, so treat the
  execution-time overhead as real but unquantified, not as a confirmed 2x.
  Failure is at `tests/live_upgrade.rs:567`: `"the new build should have
  served part of the load"` — the v2 binary never appeared in the observed
  read set — not the connection-error assertion (line ~551) the 3 macOS
  hits above were classified against. Full run: 5 passed, 1 failed in the
  `hot-upgrade` package.
  - **Why this matters**: the working hypothesis driving the (still
    undispatched, see below) macOS-only rerun campaign was "macOS runner
    contention" specifically. A hit on a plain Linux hosted runner, on a
    different assertion, under a job running instrumented (and therefore
    at least somewhat slower, though not measured here) binaries, points
    instead at a candidate more general mechanism: *some* sufficiently
    slow/contended execution can shrink the effective window the test's
    fixed-duration load generator has to observe the new build actually
    taking over traffic, before the test's own timeout fires and it
    asserts on what it saw. That is a hypothesis about the *test's* design
    (a wall-clock load window racing a real cutover, at the mercy of
    whatever the host's actual perf happens to be for that run), not
    proof either way, and "slow execution" itself is not yet quantified —
    still not confirmed, but the "macOS-specific" framing this entry has
    carried since 2026-09-04 no longer fits the evidence.
- **Verdict not yet rendered**: whether this is runner-class/contention
  timing dependence in the test's load-window design (now: on *any* slow or
  loaded runner, not only macOS) or a genuine narrow race in the hot-upgrade
  handoff (`autumn/src/upgrade.rs`) that slow execution merely exposes more
  reliably.
- **Next step**: the Tier 1 load-faithful rerun campaign (10+ fresh
  `macos-latest` VMs, pinned commit, unfiltered `cargo test --workspace`) —
  committed as `.github/workflows/manual-macos-contention-check.yml`, gated
  on a human dispatching it (new macOS CI spend needs sign-off). As shipped
  in #2527 the workflow failed to parse (`jobs.test.if` referenced the
  `matrix` context, which isn't available there — GitHub rejected every
  dispatch attempt with zero jobs run, caught by #2548 but not fixed before
  #2527 merged); fixed in
  `docs/reports/2026-09-08-semaphore-macos-contention-harness-fix.md` (#2627,
  merged 2026-09-08T15:07:44Z) and verified `actionlint`-clean. **About 19
  hours later, it still has zero `workflow_dispatch` runs** (`total_count: 0`
  against the workflow's own run history, checked 2026-09-09T10:20Z) —
  nobody has dispatched it yet. (The workflow *file* has existed since
  2026-09-05, so it is four days old, but it only became dispatchable —
  i.e. actually able to run — when #2627 fixed its parse error; don't
  conflate the file's total age with how long the working version has been
  available.) That gap is now more urgent given the new Linux hit above
  widens what the
  campaign needs to test (not macOS-only; ideally a Linux `Coverage
  (workspace)`-shaped rerun too, not just `cargo test --workspace` on a
  plain runner). #2548 separately banked 13/13 clean organic macOS samples
  on the tracked corpus since #2510 merged — reassuring, still short of the
  ≥20 (≥50 for the sub-10% end) this ledger's intake form requires to close
  an entry.
- **A fix is already in flight** (PR #2510) that reclassifies
  `ECONNRESET`/`ECONNABORTED` (retryable) separately from `ECONNREFUSED`
  (hard zero-tolerance failure) — but per its own description it could not
  be verified against a real macOS run, so it does not yet carry the
  before/after rerun evidence this ledger's intake form requires, and it
  would not address the new line-567 signature above regardless (different
  assertion entirely). Track it against the rerun campaign above before
  treating this entry as resolved.

### `cache_stampede::swr_serves_stale_and_refreshes_in_background`

- **Observed**: 1/17 `macos-latest` executions, organic sample, 2026-09-03.
  A *different* assertion (line 501, publish-visibility poll) than the one
  already hardened for a documented `windows-latest` flake in #1809 — same
  test, two different timing-sensitive assertions on two different
  non-Linux platforms.
- **Second organic hit, 2026-09-09 — now a repeat signature, not a
  one-off**: run 34297324354 (branch `dependabot/cargo/validator-0.21.0`),
  job `Test (macos-latest)`, same test, same assertion, same line:
  `autumn/tests/integration/cache_stampede.rs:501:6`, `"background refresh
  must publish the new value after it finishes computing"`. Six days apart,
  same exact panic site, both on `macos-latest` — this is no longer
  "suggestive," it is a confirmed-repeat failure signature. Still not a
  formal rerun-rate (organic sample only, denominator not tracked as
  tightly as the 2026-09-04 census's), so still below this ledger's ≥20/≥50
  bar for a fix PR, but it should be weighted at least as high as
  `live_upgrade` for the next rerun campaign, not treated as the minor
  entry it was when it had n=1.
- **Status**: under active investigation, same rerun campaign as
  `live_upgrade` above (still undispatched).

### `sim_fault_plan::same_seed_replays_a_byte_identical_outcome_100_times`

- **Observed**: 1/17 `macos-latest` executions, organic sample, 2026-09-03.
  Panic: `"job runtime is not initialized; register jobs with
  AppBuilder::jobs()"` — reads as a setup/shared-state defect (the test
  runs under `job::global_job_runtime_test_lock`, a process-global lock),
  not an exhausted wall-clock wait.
- **Status**: one occurrence — suggestive, not yet a repeat signature.
  Covered by the same rerun campaign as `live_upgrade` above.
