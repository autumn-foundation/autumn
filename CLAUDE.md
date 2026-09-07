# CLAUDE.md - Autumn Workspace Guidelines

## Versioning

**Never bump the workspace version** (`version` under `[workspace.package]` in
the root `Cargo.toml`, or any of the `autumn-web = { version = "..." }` /
`autumn-macros = { version = "..." }` pins that track it) unless the user
explicitly asks for a release/version bump. Land feature work as new bullets
under the existing `## [Unreleased]` section in `CHANGELOG.md` — do not create
a new dated/numbered `## [x.y.0]` section yourself. Cutting a release (bumping
the version, dating the changelog section, updating install instructions) is a
separate, deliberate step the user asks for by name.

## Commands

- **Build**: `cargo build --workspace`
- **Check**: `cargo check --workspace`
- **Lint**: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
- **Test all**: `cargo test --workspace`
- **Test specific package**: `cargo test -p <pkg>`
- **Test specific target**: `cargo test -p <pkg> --test <target>`
- **Pre-push gate**: `./scripts/pre-push-check.sh` — compile-only (`--no-run`)
  mirror of CI's `lint` + `test` jobs. Run it before pushing: a narrow `cargo
  test -p <pkg>` never links the autumn-web consolidated `integration_tests`
  binary, so it misses cross-package compile breaks the CI `cargo test
  --workspace` gate catches. See CONTRIBUTING.md "Before you push".

---

## CI test sharding

`.github/workflows/ci.yml` runs the suite as four sibling job families rather
than one job per OS. Which one a test lands in follows from how it is written,
so it is usually automatic:

| Job | OS | What it runs |
| --- | --- | --- |
| `test` | all 3 | `cargo test --workspace -- --skip compile_fail:: --skip sim_` — the default lane, plus a second step running `sim_` at `--test-threads=1` |
| `trybuild` | Linux | `compile_fail::*` only, split into four shards |
| `test-features` | Linux | one job per non-default feature set (markdown, i18n, tls, …) |
| `test-docker` | Linux | the `#[ignore]`d Docker/testcontainer sweep |

Only `test` runs on all three OSes. `trybuild` and `test-features` are Linux
only: a trybuild golden is pinned to the rustc *version*, not the OS (the same
`.stderr` file served all three legs), and a feature lane asks whether a
feature's own suite passes rather than whether a platform works. Cross-platform
behaviour is covered by `test` on all three, plus `Windows Tier 1 journey`.

`coverage` is sharded too, into four lanes split by feature set — the thing
that forces an instrumented rebuild. Each lane reports its own lcov and uploads
under its own Codecov flag; Codecov merges them per commit. Adding a lane means
adding a `flags:` value, not just a matrix entry.

Rules that matter when editing tests:

- **`compile_fail.rs` is partitioned by test-function name.** A new `#[test]`
  in that module runs in the `rest` shard automatically. A **renamed** one does
  not: `fail`, `pass_a` and `pass_b` name their functions explicitly and assert
  a non-zero pass count, so a rename fails that shard loudly rather than
  silently skipping it. Rename the shard filter in `ci.yml` alongside.
- **Branch protection should require `Test suite`** (the `test-gate` job), not
  the individual shards — it is the one check name that survives adding or
  removing a shard.
- **A new `sim_*` module is single-threaded automatically.** The `test` job
  skips `sim_` and a second step re-runs exactly that set with
  `--test-threads=1`. These are determinism tests — each builds a
  `start_paused(true)` current-thread runtime and asserts a seed replays
  byte-identically — and they fail under CPU oversubscription. Sharding made
  that *worse*: pulling trybuild out of the binary freed the libtest thread
  pool, so the remaining ~1880 tests went from trickling through trybuild's
  gaps (863s) to full parallelism (61s), and on the 4-vCPU `ubuntu-latest`
  runner that flipped them. Name a new simulation module `sim_*` and it lands
  in the quiet lane; name it something else and it will not.
- **A sim wall-clock budget must start after `sim.build`.** The `sim_` lane is
  a short, freshly-started process, so whichever test mounts the first app in
  it pays the one-time warm-up for every lazy static behind that app —
  measured at 2.25s on `macos-latest`. A `let wall_start = Instant::now()`
  placed above `sim.build(...)` charges that warm-up to the virtual-time
  budget and fails on the slow runners only. Start the clock immediately
  before the `advance`/`run_to_idle` under test, as
  `sim_strict_wall_clock`, `sim_advance_to` and `sim_clock_drain` all do.
- **Process-global state needs its own binary, and sharding enforces that.**
  The `test` lane now runs its ~1880 tests at full parallelism instead of
  trickling through trybuild's gaps, so a test that mutates a process-wide
  singleton and depends on it across several `await`s no longer survives on
  luck. `TestApp::build` clears the global cache unconditionally, so
  `capsule_cache_effect` — which installs one and reads it back over two
  requests — had to move to its own `[[test]]` binary, joining
  `cache_global_integration` and `cached_global_backend`. If a test only passes
  because nothing else happened to run at that moment, it belongs in the
  isolated list below, not the consolidated one.

The split is not cosmetic: on the 2026-08-26 trunk run, `compile_fail::` alone
was 37 of the 47 minutes the consolidated `integration_tests` binary spent
running on Windows, and it was the tail everything else waited on. Its cases
each shell out a nested `cargo` build and trybuild serialises them behind a
project-dir lock, so the only thing that makes it faster is more runners.

---

## Integration Test Layout Guidelines

To minimize Cargo compilation and linking overhead (avoiding 100+ separate binaries), the workspace uses a consolidated test binary structure for both the `autumn` and `autumn-cli` packages.

### Consolidated Test Targets

- **autumn**: `tests/integration_tests.rs` (compiles all consolidated modules in `tests/integration/`)
- **autumn-cli**: `tests/cli_tests.rs` (compiles all consolidated modules in `tests/integration/`)

---

### Adding a New Integration Test

#### 1. Standard Integration Tests (Consolidated)

The default approach for new tests is to add them to the consolidated binary so they compile in a single link step.

1. Place the test file under `tests/integration/<test_name>.rs`.
2. Add the module declaration to `tests/integration/mod.rs`:
   ```rust
   #[cfg(feature = "db")] // Add any required feature gates
   mod <test_name>;
   ```

##### Docker / testcontainer DB tests run automatically in CI

The CI "Run Docker-dependent tests" step — since #1747 a step of the Linux-only
`Test (Docker)` job (`test-docker`) rather than the last step of
`Test (ubuntu-latest)`, so it gets a runner whose disk it is the only claimant
of — sweeps every `#[ignore]`d
test that compiles into the `autumn` consolidated `integration_tests` binary with
`--features "test-support,offline-sync"` (a bare `--ignored` run), so a new
house-pattern testcontainer DB test — `#[ignore = "requires Docker (testcontainers)"]` in a `db`-gated (or ungated) module — executes in CI with
**no workflow edit**. Do not add a per-test allowlist line.

This sweep compiles the consolidated binary with `--features
"test-support,offline-sync,ws,mail,redis,i18n"` (db + maud are already defaults), so
a new Postgres/DB testcontainer test — and now also the previously-unreachable
`ws`/`mail`/`redis` testcontainer Docker tests — runs automatically. As of
#1945 the feature set folds in the `ws` `live_broadcast` OOB-fragment suite, the
`redis` suites (`process_role_worker_gating`, `queue_dedicated_capacity`,
`rate_limit_redis_integration`), and the `mail` newsletter-unsubscribe test:
each is testcontainer-managed (Postgres/Redis in-process), so no CI `services:`
block is required. As of #1384 the set also folds in `i18n`, so the
`#[translatable]` per-locale column round-trip suite (`translatable_model`) is
swept too.

Only **`system-tests`-gated** (browser/Chromium) Docker tests remain excluded —
they need a Chromium binary this runner does not provide. Consequently, a new
`#[ignore]`d test that must not be swept in (a browser/Chromium test, a
container this sweep doesn't provision, or a release-mode timing microbenchmark)
must either sit behind a non-default feature **not** in this set (browser tests
already live behind `system-tests`) so it never compiles into this run, or be
added to the step's `--skip` list. The one unconditionally-compiled exception
(the access_log p99 timing bench) is named in the step's `--skip` list.

##### `autumn-cli`'s `cli_tests` binary gets the same bare Docker sweep

As of #1945, ci.yml's "Run Docker-dependent tests" step also runs a bare
`--ignored` sweep over **`autumn-cli`**'s consolidated `cli_tests` binary, so a
new house-pattern `#[ignore = "requires Docker (testcontainers)"]` test added
to *any* module under `autumn-cli/tests/integration/` — new or existing —
executes in CI with no workflow edit, the same guarantee the `autumn` sweep
above gives. Before this, `cli_tests`'s Docker-gated tests were NOT
auto-swept; only two filtered invocations ran anything (`offsite`,
`db_scrub`), leaving 46 tests across 8 modules dark. The sweep also
`--skip`s the pre-existing `generate_json_postgres.rs` Docker test, which
already ran in `generator-conformance.yml`, so it doesn't run twice.

That sweep's `--skip` list names, **by exact test name** (never
`--skip <module>::`), every `#[ignore]`d test that is NOT a Docker test: it
instead scaffolds and cargo-check/build/runs a fresh generated project
(`#[ignore = "slow: ..."]`), which is too slow for the fast Docker step and
belongs in `generator-conformance.yml`'s own matrix'd job instead, named
explicitly there (same convention as every other generator-shaped gate in
that file). Skipping by exact name, not whole module, matters: a
module-prefix skip would silently swallow any *Docker* test later added to
that same file, defeating the very guarantee this sweep exists to give.
Adding a new cold-start-compile test — to a new module, or an existing
skipped one — needs BOTH a `--skip <exact test name>` line added to ci.yml's
sweep AND its own named step in `generator-conformance.yml`, or it runs in
the (wrong, slow, but not silently dark) Docker step, or never runs at all,
respectively. `autumn-cli/tests/integration/repo_hygiene.rs`'s
`cli_tests_cold_start_ignored_tests_are_ci_named` test enforces the
generator-conformance.yml half for the tests #1945 added; extend its list
when adding another.

#### 2. Isolated Integration Tests (Separate Binaries)

Only create separate test binaries if the test:

- **Has process-wide side effects**: Mutates global state (e.g., process-wide global caches or registry setups) that would interfere with other tests running concurrently in the same process.
- **Changes the working directory**: Calls `std::env::set_current_dir` (which can break relative path checks in other concurrent tests like trybuild). Note: Tests doing this should still use a drop guard to restore the directory.
- **Is executed independently in CI**: Targeted individually via a `--test <name>` filter in GitHub Actions workflows to keep that specific CI runner's build/compilation slice minimal.
- **Needs a non-host toolchain target**: Requires a target the default runner does not install (e.g. `wasm32-wasip1` for the edge-capsule conformance suite, #1790). Such a test must be `#[ignore]`d **and** live in its own `[[test]]` target outside `tests/integration/`, so the Docker sweep — which runs a bare `--ignored` over the consolidated binary — can never pick it up and fail on a missing target. It runs from its own CI job (`edge-conformance`), which installs the target explicitly.

To add an isolated integration test:

1. Place the test file directly in the root of `tests/` (e.g., `tests/<test_name>.rs`).
2. Add a `[[test]]` entry to the crate's `Cargo.toml`:
   ```toml
   [[test]]
   name = "<test_name>"
   path = "tests/<test_name>.rs"
   ```
3. Do **not** add it to `tests/integration/mod.rs`.
