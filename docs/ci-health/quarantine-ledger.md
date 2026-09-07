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
- **Verdict not yet rendered**: whether this is macOS runner contention or a
  genuine narrow race in the hot-upgrade handoff (`autumn/src/upgrade.rs`)
  that macOS's scheduling merely exposes more reliably.
- **Next step**: the Tier 1 load-faithful rerun campaign (10+ fresh
  `macos-latest` VMs, pinned commit, unfiltered `cargo test --workspace`) —
  now committed as `.github/workflows/manual-macos-contention-check.yml`,
  gated on a human dispatching it (new macOS CI spend needs sign-off).
- **A fix is already in flight** (PR #2510) that reclassifies
  `ECONNRESET`/`ECONNABORTED` (retryable) separately from `ECONNREFUSED`
  (hard zero-tolerance failure) — but per its own description it could not
  be verified against a real macOS run, so it does not yet carry the
  before/after rerun evidence this ledger's intake form requires. Track it
  against the rerun campaign above before treating this entry as resolved.

### `cache_stampede::swr_serves_stale_and_refreshes_in_background`

- **Observed**: 1/17 `macos-latest` executions, organic sample, 2026-09-03.
  A *different* assertion (line 501, publish-visibility poll) than the one
  already hardened for a documented `windows-latest` flake in #1809 — same
  test, two different timing-sensitive assertions on two different
  non-Linux platforms.
- **Status**: one occurrence — suggestive, not yet a repeat signature.
  Covered by the same rerun campaign as `live_upgrade` above.

### `sim_fault_plan::same_seed_replays_a_byte_identical_outcome_100_times`

- **Observed**: 1/17 `macos-latest` executions, organic sample, 2026-09-03.
  Panic: `"job runtime is not initialized; register jobs with
  AppBuilder::jobs()"` — reads as a setup/shared-state defect (the test
  runs under `job::global_job_runtime_test_lock`, a process-global lock),
  not an exhausted wall-clock wait.
- **Status**: one occurrence — suggestive, not yet a repeat signature.
  Covered by the same rerun campaign as `live_upgrade` above.
