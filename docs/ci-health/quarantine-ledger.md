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

- **2026-09-10 update — a third Linux/coverage signature hit, then a
  same-day fix landed on `trunk-dev` naming three mechanisms.** Sampling
  the 24.5h since the 2026-09-09 follow-up (86 `pull_request`-triggered
  `ci.yml` runs, 55 cancelled/23 success/8 failure) turned up one more
  organic hit, on the same `Coverage (workspace)`/Linux job shape as the
  prior day's line-567 hit: run 34360601529 (branch
  `claude/friendly-ritchie-rex1a9`, job id 102530590317), 2026-09-09T13:59Z.
  Panic at `examples/hot-upgrade/tests/live_upgrade.rs:552:5`: `"every read
  must be served across the cutover, saw [Observation { status: 0, body:
  "", latency: 295.896µs }, Observation { status: 0, body: "", latency:
  406.843µs }]"`, with the connection-error counters printed immediately
  above it all reading zero: `"connection failures across cutover:
  refused=0 hard_failures_after_retry=0 mid_flight_resets_retried=0"`. A
  third distinct assertion/signature on the same test (not the macOS
  connect-error cluster, not the Linux line-567 "new build never served"
  hit) — fetched via the job's raw log blob URL after `get_job_logs` with
  `return_content=true` truncated the tail before reaching the panic line
  (the test's own per-request tracing spam is large enough that even an
  8000-line tail landed short; the untruncated blob URL was needed).
  Two Linux/`Coverage (workspace)` hits on this test inside roughly 24
  hours (2026-09-09 pre-09:47Z and 2026-09-09T13:59Z), both organic,
  reinforced that this was not a macOS-only mechanism.

  **Same day, `trunk-dev`'s tip (`8fae8af`, PR #2645, merged
  2026-09-10T04:56:32Z) landed a fix titled "Fix live_upgrade test: three
  real timing races, not flakes"**, authored independent of this ledger's
  own tracking. It names three mechanisms, all test-defect (not
  product-defect — the hot-upgrade handoff mechanism itself was not
  changed) and root-caused rather than tolerance-widened:
  1. The seed request could race v1's own startup barrier
     (`StartupBarrierLayer` in `router.rs` can still 503 ordinary traffic
     after `capture_bound_addr` sees the bind-log line but before
     `on_startup` hooks finish) — fixed with a `wait_until_ready` poll
     bounded to the codebase's existing 30s upgrade budget.
  2. The fixed 3.5s post-signal window assumed the cutover itself is fast,
     with no budget behind that number — this is the mechanism behind the
     2026-09-09 Linux line-567 "new build never served" hit. Replaced with
     an adaptive wait (same 30s bound) for `successor_pid`, keeping 3.5s as
     further sustained traffic after cutover rather than the sole signal.
  3. A read can land on the *successor's* own startup barrier (v2 can
     legitimately `accept()` and 503 before `mark_startup_complete`) —
     `with_startup_barrier_retry` now retries it, bounded, mirroring how
     `with_reset_retry` already treats a mid-flight reset as
     expected-but-bounded.

  **Correction (post-review): do not attribute the 2026-09-09T13:59Z hit to
  mechanism 3.** An earlier version of this entry read that hit's
  `Observation { status: 0, body: "" }` pair as "the client-side shape of
  the same successor-not-ready-yet race." Checked against the merged
  source (`examples/hot-upgrade/tests/live_upgrade.rs`) rather than
  asserted from the log alone: `is_startup_barrier_response` requires an
  *exact* match — `observation.status == 503 && observation.body ==
  "Service is still starting up"` — and `with_startup_barrier_retry` only
  retries when that predicate holds; any other outcome, `status: 0`
  included, is returned immediately, unretried (`live_upgrade.rs:394`).
  A `status: 0`/empty-body observation is not an HTTP 503 response either
  way, so it fails that predicate and mechanism 3's retry would not have
  touched it. **Correction (post-review): do not narrow this to
  "connection-level."** An earlier version of this paragraph read
  `status: 0` as meaning no HTTP response was received at all. Checked
  against `get()` itself (`live_upgrade.rs:93-125`): a connect/write/read
  syscall failure returns `Err` and feeds `refused_errors`/`hard_failures`
  directly — a genuinely distinct path from this observation. `status: 0`
  is instead assigned via `.unwrap_or(0)` on the `Ok` path, whenever the
  response text's second whitespace-separated token isn't there or doesn't
  parse as a status code — which an empty read after a bare `accept()`
  would produce, but so would a malformed or truncated *non-empty* reply;
  the raw bytes weren't logged, so which of those actually happened here is
  unknown. Classify this as an unparseable/unknown response, not a
  connection-level failure. Since
  the failing run's own counter line (`refused=0
  hard_failures_after_retry=0 mid_flight_resets_retried=0`, with no
  `startup_barrier_hits` figure — that counter didn't exist yet in the
  pre-fix test) shows none of the *named* failure modes fired either, this
  signature is **not yet explained by any of the three mechanisms above**
  and stays an open, unattributed data point. Whether PR #2645 happens to
  fix it anyway (as a side effect of mechanism 1 or 2, which do run earlier
  in the same request path) is untested — that is exactly the kind of claim
  the CI-native rerun campaign below exists to settle, not something to
  assert from a single log.

  The fix's own verification, per its commit message: `cargo llvm-cov
  --no-report -p hot-upgrade --test live_upgrade` (a targeted, instrumented
  local rerun — **correction (post-review): not the exact build CI's
  `Coverage` job uses**, see below) passed 9+ consecutive runs across two
  local contention levels (4-8 and 16 busy loops on 4 cores), including runs
  that hit the barrier and still passed. That is real evidence and a named
  mechanism per test, satisfying the hard gate's diagnosis requirement — but
  it is a local, self-reported rerun count on a narrower build than CI's,
  not the CI-native same-commit campaign this role's own evidentiary bar
  calls for before treating an entry as closed.

  **Correction (post-review): the local command above is not CI's build.**
  `ci.yml`'s actual "Generate coverage (workspace catch-all)" step runs
  `cargo llvm-cov clean --workspace` followed by `cargo llvm-cov --workspace
  --exclude autumn-web --exclude autumn-cli --all-features --no-report` —
  a full-workspace, all-features build carrying every other crate's
  instrumentation and compile/link load in the same process, not a
  single-package `-p hot-upgrade --test live_upgrade` run with the default
  feature set. The two plausibly differ in exactly the dimension this
  investigation cares about (contention/timing), so record the fix's own
  9+ runs as targeted instrumented reruns that support the diagnosis, not
  as a rerun of the CI build itself.

  **Correction (post-review): closing this entry needs two separate,
  distinct pieces of evidence, not one dispatch — and the macOS half's
  required sample count was also stated wrong.** An earlier version of this
  paragraph said running `manual-macos-contention-check.yml` once (option
  (a)) would close the entry, and separately claimed the macOS cluster's
  historical rate is sub-10% (requiring ≥50 samples). Both need fixing:

  1. **The rate is not sub-10%, so ≥20 is the applicable bar, not ≥50.**
     The macOS cluster's own measured rate, from this entry's "Observed"
     line below, is 3/17 (≈17.6%); folding in the 13/13 clean organic
     samples #2548 banked since #2510 merged gives 3/30 (exactly 10%, not
     *below* 10%). This role's own operating standard escalates to ≥50 only
     for genuinely low-rate (sub-10%) flakes — a rate at or above 10% stays
     on the standard ≥20 bar. A single 20-sample dispatch of
     `manual-macos-contention-check.yml`, at its `samples` input's maximum
     (a `type: choice` capped at `["5", "10", "20"]`), can therefore reach
     the applicable bar for the macOS half in one run, not three.
  2. **That one dispatch still cannot close the whole entry**, because this
     harness is macOS-only and cannot touch either Linux/`Coverage
     (workspace)` signature at all — those need a still-unbuilt second
     harness with its own rerun count, run against CI's actual
     coverage-lane command, before *that* half can close.

  **Not closing this entry yet, on either half.** Zero organic hits in the
  small post-merge window sampled here (one push-triggered run on
  `trunk-dev` at the fix commit itself, success) — reassuring, but n=1, not
  evidence.

- **Observed**: 3/17 eligible `macos-latest` CI executions (14 confirmed, 3
  unresolved — see the 2026-09-04 census for the derivation), 0/16-17 on
  `ubuntu-latest`, organic PR-traffic sample, 2026-09-03/04.
- **New organic hit, 2026-09-09, on a Linux runner at a different assertion
  — tracked as a separate signature, not yet unified with the macOS
  cluster**: run 34317587464 (PR #2645, branch `claude/wizardly-wright-i1jsql`),
  job `Coverage (workspace)`, a plain hosted `ubuntu-latest` runner
  (confirmed via the job's own `labels`). This run got a plain hosted
  runner not because `coverage` is exempt from `heavy_runs_on` — it isn't;
  `coverage`, like `test-docker`, `trybuild`, and `loom`, uses
  `runs-on: ${{ fromJSON(needs.meta.outputs.heavy_runs_on) }}` directly and
  unconditionally (only the `test` job's matrix wraps it in a
  `matrix.os == 'ubuntu-latest' && ... || matrix.os` ternary) — but because
  `runner-routing.yml` hard-codes `heavy_runs_on` to `["ubuntu-latest"]` for
  every `pull_request` event, by construction (self-hosted routing is
  structurally restricted to base-repo-controlled events — push,
  workflow_dispatch, schedule — since a PR's workflow file comes from a
  fork-controlled head). So this run drew a standard runner, not a
  contended self-hosted one, but "standard GitHub-hosted" is not itself a
  measured contention level — the actual load on either this runner or any
  of the 3 macOS runners in the earlier hits is unmeasured in both
  directions. Step "Generate coverage (workspace catch-all)" runs `cargo
  llvm-cov`, which instruments every test binary it wraps; this job's own
  inline comment notes that roughly doubles `target/`'s on-disk *size* —
  no wall-clock runtime measurement was taken here, so treat any execution
  overhead as unquantified, not a confirmed slowdown. Failure is at
  `tests/live_upgrade.rs:567`: `"the new build should have served part of
  the load"` — the v2 binary never appeared in the observed read set —
  which is a **different assertion** than `assert_eq!(connect_errors, 0)`
  at the file's then-`line 268`, the connection-error check the 3 macOS
  hits above were classified against per the 2026-09-04 census. That
  single counter has since been split by the #2510 fix into `refused == 0`
  / `hard == 0` around today's lines 520-528 — cited by name rather than a
  guessed current line number, since the file has been refactored since
  the census ran. Full run: 5 passed, 1 failed in the `hot-upgrade`
  package.
  - **Why this is logged here but not folded into the macOS cluster's
    diagnosis**: same test file, but a different assertion can mean a
    different bug entirely — "the new build never took over" and "requests
    saw connection errors during cutover" are not obviously the same
    failure mode just because they share a test. Do not treat this as
    disproving, or as evidence against, the macOS-specific framing of the
    *existing* 3-hit cluster; treat it as a fourth, separate data point
    that argues the still-undispatched rerun campaign (below) should not
    stay macOS-only, since a hot-upgrade timing sensitivity may exist on
    more than one platform — without yet claiming those sensitivities
    share a mechanism. Only a rerun campaign that reproduces the *same*
    signature on both platforms would justify unifying them.
- **Verdict not yet rendered**: whether the line-567 signature is a
  runner-class/contention timing dependence in the test's load-window
  design, a genuine narrow race in the hot-upgrade handoff
  (`autumn/src/upgrade.rs`) that slow execution merely exposes more
  reliably, or an unrelated failure mode from the 3 macOS connection-error
  hits entirely. All three remain open.
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
  ≥20 (≥50 for the sub-10% end) sample size this role's own evidentiary bar
  calls for before treating an entry as closed — and per the corrected math
  in the 2026-09-10 update above, 3/30 (10%, not below it) puts this
  specific cluster on the ≥20 side of that split, not ≥50. (That specific
  numeric threshold is Semaphore's own operating standard, not a field
  defined in
  this ledger's intake form above — the intake form's own requirement is
  just a same-commit rerun-rate baseline, `<k>/<n>`, with no minimum `n`
  written into it.)
- **A fix has already landed** (PR #2510, merged 2026-09-05T20:25:01Z) that
  reclassifies `ECONNRESET`/`ECONNABORTED` (retryable) separately from
  `ECONNREFUSED` (hard zero-tolerance failure) — this is the change behind
  today's `refused == 0` / `hard == 0` split at lines 520-528 referenced
  above. But per its own description it could not be verified against a
  real macOS run at merge time, so it still does not carry the before/after
  rerun evidence this role's evidentiary bar calls for, and it would not
  address the new line-567 signature above regardless (different assertion
  entirely). Track it against the rerun campaign above before treating this
  entry as resolved — "merged" is not the same as "verified."

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
  tightly as the 2026-09-04 census's), so still below the ≥20/≥50 sample
  size this role's own evidentiary bar (not the intake form) calls for
  before a fix PR, but it should be weighted at least as high as
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
