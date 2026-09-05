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

| Job | What it runs |
| --- | --- |
| `test` | `cargo test --workspace -- --skip compile_fail:: --skip sim_` — the default lane, plus a second step running `sim_` at `--test-threads=1` |
| `trybuild` | `compile_fail::*` only, split into four shards |
| `test-features` | one job per non-default feature set (markdown, i18n, tls, …) |
| `test-docker` | the Linux `#[ignore]`d Docker/testcontainer sweep |

Two rules matter when editing tests:

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

The CI `test-docker` job (Linux) sweeps every `#[ignore]`d
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
