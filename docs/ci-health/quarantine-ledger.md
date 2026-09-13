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

### `offsite_backup::offsite_backup_upload_then_restore_round_trips` / `offsite_backup::offsite_backup_uploads_large_artifact_via_multipart` / `sqlite_replication_s3::replicates_to_and_restores_from_a_real_s3_endpoint`

- **Not a flake — a total, deterministic external-dependency outage.**
  Sampling the ~23.3h window since the 2026-09-11 follow-up's cutoff
  (2026-09-11T09:09:24Z–2026-09-12T07:46:03Z) found `Test (Docker)` failing
  on 20 of 24 failed `ci.yml` runs, across completely unrelated branches
  (`claude/*` and `vesper/bugbash-*`, no shared code change). Every one of
  the `autumn-cli` `cli_tests` occurrences checked (run IDs 34668965068,
  34681578354, 34650506433, 34641243320, 34647320725, and others) panics
  identically at `autumn-cli/tests/integration/offsite_backup.rs:213:10`:
  `"start MinIO — is Docker running?: Client(PullImage { descriptor:
  \"minio/minio:RELEASE.2025-02-28T09-55-16Z\", err:
  DockerResponseServerError { status_code: 404, message: \"pull access
  denied for minio/minio, repository does not exist or may require 'docker
  login': denied: requested access to the resource is denied\" } })"`.
  Since `Test (Docker)` feeds the required `test-gate` aggregator
  (`Test suite`), this was failing the required check on essentially every
  open PR in the repo.
- **Mechanism**: unpinned/vanished external service, not a race or shared
  state. Confirmed directly against Docker Hub's own API
  (`https://hub.docker.com/v2/repositories/minio/minio/` →
  `{"message":"object not found"}`) that the `minio/minio` repository no
  longer exists on Docker Hub at all — not just this tag. MinIO Inc.
  stopped publishing free images to Docker Hub in October 2025. The
  `testcontainers-modules` crate (pinned at 0.15.0 in `Cargo.lock`) hard-codes
  `minio/minio` as the image name in its `Image` impl, so every
  `MinIO::default()` call in this repo pulled from the now-dead repository,
  100% of the time — this is not stochastic, so no rerun-rate campaign is
  needed to characterize it beyond the cross-commit evidence already in
  hand (dozens of independent commits, zero passes, identical signature).
  The same tagged image is still mirrored byte-for-byte on Quay
  (`quay.io/minio/minio:RELEASE.2025-02-28T09-55-16Z`, confirmed via
  `quay.io`'s API: identical manifest digest
  `sha256:379b06de0d24339646b6139860b170c39b004818dcec95259ee680997839f7dc`
  to the Docker Hub layer that used to serve this tag).
- **Test-vs-product**: neither — this is test/CI infrastructure depending
  on a third-party image registry outside this repo's control. No product
  code path is implicated.
- **Fix**: redirect every `MinIO::default()` call site to Quay via
  `testcontainers`'s own `ImageExt::with_name("quay.io/minio/minio")`,
  keeping the crate's existing default tag unchanged (same verified
  manifest digest, so container behavior is identical — only the registry
  changes). Applied to all three affected call sites:
  `autumn-cli/tests/integration/offsite_backup.rs` (both tests),
  `autumn/tests/integration/sqlite_replication_s3.rs`, and
  `examples/reddit-clone/tests/avatar_s3_integration.rs` (not part of
  either CI Docker sweep, but the same defect, so fixed for consistency
  rather than left to fail identically whenever someone runs it).
- **Verification**: `cargo check`/`cargo clippy -D warnings` clean on all
  three affected test targets (`autumn-cli --test cli_tests`, `autumn-web
  --test integration_tests --features test-support,offline-sync`,
  `reddit-clone --test avatar_s3_integration`). No Docker daemon is
  available in this sandbox, so the actual container pull could not be
  exercised locally; **CI-native verification is pending on this PR's own
  `Test (Docker)` job**, which exercises the real pull against
  `quay.io/minio/minio` for the first time. Revert check: not applicable in
  the usual sense (nothing in this repo's own logic changed — the defect
  was entirely in an external registry going away), but the `PullImage`
  failure this fix removes is fully reproducible pre-fix (see the run IDs
  above) and specific to the registry, not the tag or image content, so
  restoring `MinIO::default()` without `.with_name(...)` would reproduce
  the identical 404 immediately.
- **Closed**: 2026-09-12, pending this PR's own CI run for the CI-native
  confirmation noted above (🚦 Semaphore). **Confirmed 2026-09-13**: PR
  #2740's own `Test (Docker)` check run (job 103543584136, part of workflow
  run 34688787858) completed `success` at 2026-09-12T11:55:09Z — the real
  pull against `quay.io/minio/minio` that could not be exercised in this
  sandbox has now been exercised by CI itself. This entry's fix is
  CI-natively verified, not just locally clippy-clean. See the new escape
  entry immediately below for a coordination defect this fix's merge
  timing collided with (independent, not a defect in the fix itself).

### Escape: four independent fixes for the same MinIO/Docker-Hub outage collided at merge, needing two reconciliation commits — ~8.4h of spurious Lint failures on 5 branches

- **Correction (post-review, via a Codex review comment on PR #2768): the
  original version of this entry named the wrong commits and the wrong
  count.** It attributed the cleanup to #2749/#2750/#2751/#2752 and framed
  this as a two-PR (#2740/#2743) collision. Checked directly against each
  commit's actual diff rather than its title or PR number: #2750
  (`cfb5d93`), #2751 (`6dd7bb1`), and #2749 (`4eeeeac`) are **empty
  merges** — no file changes at all, because by the time each squash-merge
  landed, `trunk-dev` already carried equivalent content from a different,
  concurrently-merging branch. #2752 (`1c5312e`) touches
  `autumn-cli/src/generate/auth.rs`/`schema_edit.rs`/`CHANGELOG.md` only —
  nothing MinIO-related. Corrected below from the actual diffs (`git show
  --stat`/`-p` on every commit that touched the three affected test
  files), not from any commit's own self-description — even the commit
  that removed the dead helper (#2756) misattributes what added it.
- **Second correction (post-review, via a further Codex review comment on
  PR #2768): the lint-fallout window ends at #2756, not #2729.** The
  first correction pass (above) still closed the window at #2729
  (2026-09-13T02:30:05Z) and called it "the actual final resolution."
  Checked directly against the tree at `a7c7c46` (#2756, 02:09:18Z):
  `start_minio()` already has its two live callers, `MINIO_IMAGE` is
  already wired into the avatar test's call site, and `minio_image()` is
  already removed — zero dead code, full stop. #2729 (21 minutes later)
  refactors that already-clean tree (reintroducing and then re-collapsing
  its own branch's separate `minio_image()` copy, entirely within its own
  single squashed commit — trunk-dev itself never saw that intermediate
  state) and adds the regression test; it is a subsequent architectural
  cleanup and a genuinely good side effect, not part of resolving the
  fallout, which had already ended. The impact window and all timing
  below are corrected to close at #2756.
- **Third correction (post-review, via two further Codex review comments
  on PR #2768): "six independent fixes" conflated independent outage
  diagnoses with reactive cleanup commits, and the branch count was
  overstated.** #2756 (9h05 after #2740) only deletes an unused helper —
  it is not an independent diagnosis of the outage, it is cleanup of dead
  code the collision left behind. #2725's own sub-commit message says so
  explicitly: "fix: use the MINIO_IMAGE const **the merge from trunk-dev
  introduced**" — it is repairing dead code its own branch picked up from
  merging `trunk-dev`, not freshly diagnosing the Docker Hub outage.
  Separated below into two independent-diagnosis fixes reacted to by two
  reconciliation commits, not six of a kind. Separately, the claimed "at
  least 6 distinct, unrelated WIP branches" named only five (counting
  `brave-goldberg-gyr60j` once, since its two hits are one branch) — the
  two `fix/reddit-clone-minio-*`/`fix/minio-quay-registry` branches
  mentioned alongside them are not unrelated victims of the fallout, they
  are other sessions' own independent attempts at fixing the outage
  itself, a different category of evidence. Reduced to the 5 branches
  actually confirmed by job-log inspection.
- **Not a flake, not a product bug — a coordination gap.** The same
  universally-visible `ci.yml` failure (every `Test (Docker)` run 404ing
  on the dead `minio/minio` Docker Hub repository, regardless of a PR's
  own diff) was independently diagnosed and fixed inside **four separate
  PRs** within a ~6.3 hour window, none aware of the others — consistent
  with this role's own and every other session's "red CI is work now"
  posture: whoever hit the failure on their own PR fixed it in place
  rather than waiting. Two further commits then had to reconcile the
  dead code this collision left behind. Chronology, verified against each
  commit's actual file-level diff and (where quoted) its own commit
  message, not any commit's self-description of *other* commits — even
  the commit that removed the dead helper (#2756) misattributes what
  added it:

  **Independent outage diagnoses (4):**
  - **#2740** (`abacf9e`, this ledger's own fix, merged
    2026-09-12T17:03:51Z): inlined
    `.with_name("quay.io/minio/minio")` at all four call sites across
    `offsite_backup.rs` (both tests), `sqlite_replication_s3.rs`, and
    `avatar_s3_integration.rs`. **No helper function or constant** — the
    original entry's claim that this PR added `minio_image()` is wrong.
  - **#2743** (`d2693c1`, independent, 17:46:17Z, 43 min later):
    added `const MINIO_IMAGE` to `avatar_s3_integration.rs` without wiring
    it into the call site (which already carried #2740's identical inline
    literal by merge time) — the first dead-code seed.
  - **#2722** (`e9f90a7`, an unrelated replay-guard test PR, sub-commit
    "fix: point MinIO testcontainers at quay.io (Docker Hub repo
    pulled)" — its own message independently re-derives the outage from
    its own PR's `Test (Docker)` failure, not from a merge conflict,
    23:21:15Z): introduced a **new** `start_minio()` helper in
    `offsite_backup.rs`, replacing both tests' inline literals from
    #2740.
  - **#2720** (`0917af7`, an unrelated `SeqKey` append-ordering PR,
    sub-commit "fix: MinIO Docker tests point at quay.io, not the dead
    Docker Hub repo" — likewise its own independent re-derivation,
    23:22:06Z, one minute after #2722): independently introduced a
    **second, competing** `minio_image()` helper in the same file,
    duplicating `start_minio()`'s purpose with a different tag-pinning
    strategy, and left it uncalled (dead code) — the second signature.

  **Reconciliation commits, reacting to the above collision rather than
  independently diagnosing the outage (2):**
  - **#2725** (`e2cd122`, an unrelated `autumn upgrade` codemod PR,
    sub-commit explicitly titled "fix: use the MINIO_IMAGE const **the
    merge from trunk-dev introduced**", 23:19:07Z): wired
    `avatar_s3_integration.rs`'s call site to #2743's constant, closing
    that file's dead-code gap — its own message names this as repairing
    merge-introduced dead code, not a fresh diagnosis.
  - **#2756** (`a7c7c46`, 2026-09-13T02:09:18Z, 9h05 after #2740): removed
    the uncalled `minio_image()` from #2720, keeping `start_minio()`. Its
    own diff touches nothing but that deletion. **This is where the
    lint-fallout window actually ends** — confirmed against the tree at
    this commit: `start_minio()` has two live callers, `MINIO_IMAGE` is
    wired into the avatar test, no unused helper remains. Zero dead code.

  **Later, unrelated to either the collision or its cleanup:**
  - **#2729** (`6e71bfb`, a large wire-contracts feature PR whose
    long-lived branch had merged `trunk-dev` in three times over the same
    window and picked up a MinIO fix each time, 2026-09-13T02:30:05Z, 21
    minutes *after* #2756 already closed the fallout): a subsequent
    refactor of the already-clean tree, not part of resolving the escape.
    It reintroduces and then re-collapses its own branch's separate
    `minio_image()` copy entirely within its own single squashed commit
    (`trunk-dev` itself never saw that intermediate duplicate state), and
    lands one clean genuine improvement as a side effect: a new
    regression test, `minio_image_pulls_from_the_public_registry` — a
    `#[test]` (not `#[ignore]`d, so it runs in the ordinary lane without
    Docker) asserting the descriptor's registry and that a tag is pinned,
    specifically so a future regression "surfaces as a red job naming a
    registry rather than the file that forgot" (its own doc comment).

  Confirmed directly against `autumn-cli/tests/integration/offsite_backup.rs`
  at `trunk-dev`'s current tip (`6e71bfb`, post-#2729's later refactor):
  `start_minio()` calls `minio_image()`, both tests call `start_minio()`,
  and `minio_image_pulls_from_the_public_registry` passes — one source of
  truth, no dead code, regression-guarded. This describes the current
  state, not the fallout's resolution point (#2756, above).
- **Impact, measured**: sampling `ci.yml` `pull_request` runs from
  2026-09-12T17:46:17Z (the #2743 merge, first dead-code seed) to
  2026-09-13T02:09:18Z (the #2756 merge, where the tree is first
  confirmed clean) — roughly 8.4 hours — found the same `-D dead-code`
  `Lint` failure on 5 distinct, unrelated WIP branches (confirmed by job
  log inspection, not inferred from branch name): `claude/friendly-ritchie-uw76a2`,
  `claude/tender-galileo-6f3dr7`, `claude/epic-clarke-8nbaes`,
  `claude/brave-goldberg-gyr60j` (hit twice, both signatures below),
  `claude/busy-cerf-9zos9k`. (Separately, `fix/reddit-clone-minio-*` and
  `fix/minio-quay-registry` branch names were visible in the same window —
  those are other sessions' own outage-fix attempts, evidence of the
  coordination gap's scale, not additional dead-code victims, so they are
  not counted in this blast-radius figure.) Two distinct signatures, both
  dead-code, both in MinIO-related test files: `` error: function
  `minio_image` is never used `` (`autumn-cli/tests/integration/offsite_backup.rs:216`,
  from #2720's copy) and `` error: constant `MINIO_IMAGE` is never used ``
  (`examples/reddit-clone/tests/avatar_s3_integration.rs:22`, from #2743's
  unwired constant). Every hit failed only `Lint`/`Clippy` (and the `Test
  suite` aggregator that depends on it) — no runtime test behavior was
  affected, consistent with this being purely a merge-time dead-code
  artifact, not a functional regression.
- **Mechanism classification**: neither a test defect nor a product
  defect — a **process/coordination gap**, a four-way one for the
  original diagnoses plus two reactive cleanups, not a two-way one.
  Nothing in `ci.yml` or the repo's own tooling flags "another open PR
  already fixes this exact failure" before merge, and nothing flags
  "this branch's own dead code came from a trunk-dev merge, not from its
  own diff" either — so a failure visible to literally every open PR at
  once (Docker Hub removing a dependency image) drew independent,
  uncoordinated fixes from whichever PR happened to notice it first,
  including inside PRs whose own subject matter (a replay-guard test, a
  `SeqKey` ordering fix) had nothing to do with MinIO, and each collision
  between those fixes then needed its own separate reconciliation commit.
  No action needed here beyond recording it accurately: the fallout is
  already fully resolved on `trunk-dev`, with a regression test added
  later, and the affected branches only need an ordinary rebase to pick
  up the clean state. This is Tier 1 escape-analysis evidence per this
  role's own evidentiary tiers, not a new quarantine candidate.
- **Closed**: 2026-09-13 (🚦 Semaphore), recorded after the fact — the
  fallout resolved itself via the commits above before this pass began;
  corrected twice, same day (2026-09-13), after Codex review comments on
  the ledger's own PR (#2768) caught first the misattributed commits,
  then the wrong window end-point and the conflation of independent
  diagnoses with reactive cleanup commits.

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
- **Verdict not yet rendered** *(superseded — see the 2026-09-10 update at
  the top of this entry)*: whether the line-567 signature is a
  runner-class/contention timing dependence in the test's load-window
  design, a genuine narrow race in the hot-upgrade handoff
  (`autumn/src/upgrade.rs`) that slow execution merely exposes more
  reliably, or an unrelated failure mode from the 3 macOS connection-error
  hits entirely. All three remain open. **Superseded 2026-09-10**: PR
  #2645's mechanism 2 names this exact signature in its own commit message
  ("no read observed a v2 response inside the fixed window") and fixes it
  as a runner-class/contention timing dependence in the test's own
  load-window design — the first of these three options, confirmed
  test-defect rather than a product handoff race, not merely "slow
  execution exposing" one. Diagnosed-and-fixed still isn't the same as
  closed-per-this-role's-bar (see the 2026-09-10 update's own closure
  paragraph) — this note marks the verdict as rendered, not the entry as
  closed.
- **2026-09-11 update — harness still undispatched (4th consecutive daily
  pass); zero new organic hits inside the sampled window, but one landed
  live afterward on this ledger's own tracking PR.** **Correction
  (post-review, via a thirteenth Codex review comment on PR #2711): the
  original headline here said "zero new organic hits" unscoped, which
  went stale the moment the live hit below was logged in this same
  entry.** Scoped now: zero hits in the sampled 109-run window; one hit
  (run 34591670807, a `status: 0` repeat) outside it — see below.
  Sampled the ~23.3h since the 2026-09-10 follow-up's actual recorded
  cutoff (`2026-09-10T09:48:19Z`–`2026-09-11T09:09:24Z`; **correction,
  post-review, via a sixth Codex review comment on PR #2711**: an earlier
  version of this line understated the window as starting at
  `10:30:30Z`, silently skipping the 42-minute gap between the two
  reports — that gap was separately queried and holds 9 more
  `pull_request`-triggered `ci.yml` runs, all cancelled, no failures, so
  the combined population is 109 runs: 70 cancelled/25 success/14
  failure, not 100/61/25/14). **Second correction (post-review, via an
  eleventh Codex review comment on PR #2711): the 100-run figure itself
  was only ever a page-1 result at the `perPage=100` ceiling, not verified
  complete.** This repo's `total_count` for the underlying query grew
  visibly during the pass (~7369 → ~8191), evidence of continuous
  concurrent writes that can shift page boundaries between fetches.
  Checked page 2 of the identical query: it overlaps this window's near
  edge (`2026-09-09T10:07:59Z`–`2026-09-10T10:46:39Z`, i.e. past the
  page-1 minimum), and every run in the overlap back to the actual cutoff
  is accounted for — 16 runs, all cancelled/success except one failure
  (34467823999) already identified above. No additional failures surfaced
  there, but the window's far edge (near `2026-09-11T09:09:24Z`) was never
  independently re-checked against a later page, and sampling by
  wall-clock time plus a fixed page count is not reproducible against a
  table this actively written to. Full reasoning and the reproduce
  command are in the 2026-09-11 report; a future pass sampling this repo
  should anchor to a stable run ID or commit rather than wall-clock time.
  **Live organic hit during this same PR's own CI, 2026-09-11T11:51:57Z —
  a second occurrence of the previously-unattributed `status: 0`
  signature, now also on a plain `Test (ubuntu-latest)` job with no
  coverage instrumentation.** PR #2711 (this ledger's own PR) is a
  docs-only change with no code diff, so this is pure organic CI noise
  from `trunk-dev`'s current `live_upgrade.rs`, not anything this PR
  touched. Run 34591670807, job `Test (ubuntu-latest)`
  (`check_run_id` 103245977784), branch `claude/sleepy-brown-uykw44`
  at the same base commit as `trunk-dev`'s tip (which already carries the
  #2645 fix — confirmed `8fae8af` is an ancestor). Panic at
  `examples/hot-upgrade/tests/live_upgrade.rs:686:5`: `"every read must be
  served across the cutover, saw [Observation { status: 0, body: "",
  latency: 554.629µs }]"` — a single `status: 0`/empty-body observation,
  same shape as the 2026-09-09T13:59Z hit this ledger already logged as
  not matching any of PR #2645's three named predicates. `refused`/`hard`/
  retry-bound assertions above this line did not panic, so those counters
  were clean, consistent with the earlier hit. This is now two occurrences
  of this exact signature, and — significantly — this one is on the plain
  `Test (ubuntu-latest)` job, **not** `Coverage (workspace)`, which weakens
  the working assumption (never more than a hypothesis) that this
  signature needs `cargo llvm-cov` instrumentation to manifest: it doesn't.
  Still undiagnosed and still not campaigned (n=2, not a formal rerun
  protocol), but this raises its priority for the still-unbuilt
  Linux-shaped rerun harness the 2026-09-09/10 reports already flagged as
  needed — it is not coverage-specific after all, so a plain `cargo test`
  rerun harness (Linux, no `llvm-cov`) could reproduce it, which is a
  cheaper harness to build than previously assumed. (This hit is outside
  the sampled 109-run window above — it landed live, after the window
  closed — so it is not one of the 14 counted failures there.)

  Of the 14 failures counted in the sampled window itself, none match
  `live_upgrade`, `cache_stampede`, or `sim_fault_plan` —
  see the new `job_tracking_stores_integration` entry below for the one
  finding this pass did turn up, on a different test entirely.
  `manual-macos-contention-check.yml`: still `total_count: 0` against
  `workflow_dispatch` runs, checked 2026-09-11T~09:5xZ — unchanged for a
  4th straight day since it became dispatchable 2026-09-08T15:07:44Z.
  Zero organic hits this pass is reassuring but a ~23h window is not a
  substitute for the rerun campaign below; the recommendation to dispatch
  it (macOS half only, `samples: "20"`) stands unchanged from the
  2026-09-10 pass.
- **2026-09-13 update — 5th consecutive pass, harness still undispatched;
  zero new organic hits on any of the three tracked signatures in the
  sampled window.** Sampled `ci.yml` `pull_request` runs from roughly
  2026-09-12T13:58Z to 2026-09-13T09:02Z (~19 hours, ~130+ runs spanning
  both pages of the query). Every failure in that window attributed to
  one of: the pre-existing MinIO/Docker-Hub outage (pre-#2740, before
  17:03:52Z), the #2740/#2743 dead-code escape documented above
  (17:46:17Z-02:09:18Z), or a WIP branch's own in-progress bug (a
  `dependabot` toolchain bump breaking `semver_script_checks_...`, a
  `capture_min_length` feature branch, repeated `Clippy` churn on single
  branches iterating on lint fixes). None matched `live_upgrade`,
  `cache_stampede`, `sim_fault_plan`, or (see below)
  `job_tracking_stores_integration`. `manual-macos-contention-check.yml`:
  still `total_count: 0` against `workflow_dispatch` runs, checked
  2026-09-13T~09:1xZ — unchanged for a 5th straight day since it became
  dispatchable 2026-09-08T15:07:44Z (now ~90 hours idle).
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

### `job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`

- **New, 2026-09-11.** First occurrence found in the 2026-09-11 follow-up
  pass. Run 34517281816 (branch `vesper/bugbash-2634-spez-normalize-fallback`,
  not a change to the job-tracking code itself), job `Test (Docker)`
  (the bare `--ignored` sweep over the `autumn` consolidated
  `integration_tests` binary), 2026-09-10T19:24–20:01Z. `test result:
  FAILED. 369 passed; 1 failed` — a single failure among the whole Docker
  sweep. Panic at
  `autumn/tests/integration/job_tracking_stores_integration.rs:264:5`:
  `"record should be past its configured TTL"`.
- **Mechanism**: the test (lines 216-264) configures `ttl_secs: 1`, calls
  `job::enqueue_tracked` (which stamps `expires_at = self.clock.now() +
  1s` using the *application's* `SystemClock`,
  `PgJobTrackingStore::expires_at` in
  `autumn/src/job_tracking.rs:1874-1878`), reads the row back once, then
  `tokio::time::sleep(Duration::from_millis(1_200))` before asserting
  `expires_at <= NOW()`, evaluated by Postgres
  (`autumn/tests/integration/job_tracking_stores_integration.rs:256-258`).
  `tokio::time::sleep` is `Instant`-backed and cannot fire early, so at
  least 1200ms of real host time elapses before the check — comfortably
  over the 1000ms TTL if `expires_at` is never rewritten after the initial
  enqueue.

  **Originally read (time dependence — dual clock source) as requiring
  Postgres's wall clock to lag the app host's by more than the ~200ms
  margin, attributed to contention on a heavily loaded runner.**
  **Correction (post-review, via a second Codex review comment on PR
  #2711): drop contention-induced clock skew as a candidate.** The Rust
  test process and its `testcontainers`-managed Postgres container run on
  the same GH Actions runner and, absent an explicit Linux time
  namespace (not configured here), read the same underlying
  `CLOCK_REALTIME` — they are not two independently-advancing clocks in
  the sense that framing implied. CPU scheduling contention can delay
  *when* a descheduled process gets to observe or write the clock, but
  that only ever adds real elapsed time before the observation happens; it
  cannot make the value read back *lag behind* true elapsed time, since
  both sides are reading the same clock. A genuine clock skew here would
  need a discrete step (e.g. an NTP correction moving the clock backward
  between the write and the check) rather than ordinary contention — a
  categorically different and far less likely mechanism, not the
  contention-driven one originally proposed. Demoted accordingly; not
  ruled out as a class (a clock step is possible in principle), but no
  longer treated as comparably likely to the mechanism below.

  **Correction (post-review, via a first Codex review comment on PR
  #2711): the "only way" framing was wrong regardless — a second,
  actually well-supported mechanism requires no clock disagreement at
  all.** `run_job_handler_inner` (`autumn/src/job.rs:2266-2286`) calls
  `store.mark_running(key)` immediately once the enqueued no-op job is
  picked up by the running job runtime this test starts, and on
  completion calls `ctx.settle_success()` (`autumn/src/job.rs:2346`); both
  route through `PgJobTrackingStore::update`
  (`autumn/src/job_tracking.rs:1927-1936`), which unconditionally
  rewrites `expires_at` to *that write's own* `now + ttl`, all on the same
  clock. If either write lands roughly 200-1000ms after the test's
  initial read — well within reach of ordinary worker dispatch latency,
  no contention or clock disagreement of any kind required —
  `expires_at` is pushed past the 1.2s check point legitimately. This is
  the same worker/update race the reviewer notes the Redis sibling test
  (lines 113-117 immediately above) also permits in principle, though no
  organic hit has been observed there — that sibling test is exposed to
  the same worker/update race but never crosses a second clock source, so
  it cannot help isolate the (now-demoted) clock-skew hypothesis, and its
  clean history so far says nothing about the worker-refresh one either
  way. **This worker-refresh mechanism is now the primary candidate**;
  neither it nor a discrete clock step is confirmed.
- **Test-vs-product**: not yet rendered, under either candidate mechanism.
  **Correction (post-review, via a fourth Codex review comment on PR
  #2711): "production never compares against Postgres's own `NOW()`" was
  flatly wrong — a separate production code path does exactly that,
  deliberately.** `pg_cleanup_expired_tracking_rows`
  (`autumn/src/job.rs:9333-9358`), run periodically off a
  `tracking_cleanup_interval.tick()`, executes `DELETE FROM
  autumn_job_tracking WHERE expires_at <= NOW()` — the same cross-process
  shape (an app-clock-stamped `expires_at` against Postgres's own `NOW()`)
  this test's assertion uses, and the codebase's own test comments
  (`autumn/src/job.rs:16704-16707`) already document the choice
  explicitly. So this test doesn't invent a comparison production never
  makes; it re-derives one production already makes elsewhere.
  **Correction (post-review, via a seventh Codex review comment on PR
  #2711): the sweep's cadence does not make ordinary clock disagreement
  immaterial to it, and the reasoning above was wrong to imply that.**
  Cadence controls how often the sweep gets a chance to observe a
  disagreement, not the disagreement's *size* at any one observation —
  a sweep that runs once every five minutes with the DB clock leading the
  app clock by, say, 50ms can delete a row `PgJobTrackingStore` still
  considers live just as readily as one that runs every second; running
  less often does not shrink the skew.
  **Correction (post-review, via an eighth Codex review comment on PR
  #2711): TTL length is not a bound on this risk either, and the previous
  fix's replacement reasoning repeated the same class of error.** A
  longer TTL moves the absolute expiry point further into the future; it
  does not widen any margin around that point, and a fixed clock
  disagreement (e.g. Postgres leading the stamping host by 50ms) shaves
  the same 50ms off the effective TTL whether it is 1 second or 24 hours.
  `JobTrackingConfig::ttl_secs` (`autumn/src/config.rs:3943-3966`) also has
  no enforced minimum — it is operator-configurable with a 24-hour
  default and nothing stopping a much smaller value — so "production TTLs
  are presumably chosen with margin" was an assumption, not a bound.
  Withdrawn along with the cadence reasoning it echoed: nothing in this
  entry actually bounds the early-deletion/late-retention risk from a
  real clock disagreement; it is retained as open, not quantified away.

  **Correction (post-review, via a ninth Codex review comment on PR
  #2711): the read path is not reliably same-clock either — that was true
  only for this specific test's single-process shape, not for production
  generally.** `docs/guide/jobs.md`'s "Web and worker process roles"
  section documents `web` and `worker` as separate process roles
  (typically separate replicas/hosts) that share one durable Postgres
  backend: a `web` replica's `job::enqueue_tracked` can stamp `expires_at`
  from its own `SystemClock`, while a different `worker` replica's
  `mark_running`/`settle_success` later calls
  `PgJobTrackingStore::update` (`autumn/src/job_tracking.rs:1896-1902`)
  using *that host's* `self.clock.now()` — genuinely two independent
  clocks in that supported topology, the same shape as the cleanup sweep,
  not a same-clock comparison at all. Only this test's own `combined`
  (single-process) shape makes it same-clock; a discrete clock step is
  not the only way the read path can disagree with an `expires_at` stamped
  elsewhere — ordinary inter-host skew across `web`/`worker` replicas can
  too, with no step required. `autumn/src/time.rs:105-108`'s point about
  wall-clock comparisons lacking a monotonic guarantee still applies and
  still matters for the single-host clock-step case, but it is no longer
  the only source of read-path risk. A backward host clock step between a
  write and a later read would extend a tracked job's effective TTL in
  production via this path too, not just in this test — a real,
  product-relevant characteristic of using wall-clock timestamps for TTL
  comparisons, not dismissible as a test artifact, and now understood to
  be one of at least two ways (clock step, or ordinary web/worker skew)
  this path's assumption can fail. None of this means the observed
  failure *was* a clock-related race of any kind — the worker-refresh
  mechanism above remains the better-supported explanation for this
  specific incident, since it fires within a single test process and
  needs no cross-host clock disagreement at all — only that the
  scenario's test-vs-product classification was wrong as originally
  written, repeatedly: once for
  treating the cross-process comparison itself as production-absent, and
  once for treating even a clock step as test-only. Refreshing
  `expires_at` on `mark_running`/`settle_success` (the
  worker-refresh hypothesis) is deliberate, sensible production behavior
  in its own right — a job still being worked on should not expire out
  from under it — so if that mechanism is the one actually firing here,
  the defect is squarely in the test's assumption that a fixed 1200ms
  sleep leaves no room for the tracked job's own worker to touch the
  record, not in the store: a test defect there. Both remain hypotheses
  from reading the source, not yet confirmed by a rerun campaign or an
  isolating experiment (e.g. asserting on `updated_at` to see which write,
  if either, actually fired), so treat the verdict as provisional per this
  role's own bar.
- **Status**: n=1, not campaigned. Logged here for recognition per this
  role's standard for a first hit; escalate to a rerun campaign only if a
  repeat signature appears. Not quarantined — the Docker sweep is
  unmodified and this test keeps running on every sweep.
- **2026-09-13 update**: no repeat in the ~19h window sampled this pass
  (see the `live_upgrade` entry's dated update above for the window and
  method). Still n=1, still not campaigned.
