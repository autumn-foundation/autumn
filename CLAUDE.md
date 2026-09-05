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
`Test (Docker)` job rather than the last step of `Test (ubuntu-latest)`, so it
gets a runner whose disk it is the only claimant of — sweeps every `#[ignore]`d
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
