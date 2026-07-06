# CLAUDE.md - Autumn Workspace Guidelines

## Commands

- **Build**: `cargo build --workspace`
- **Check**: `cargo check --workspace`
- **Lint**: `cargo fmt --all && argo clippy --workspace --all-targets -- -D warnings`
- **Test all**: `cargo test --workspace`
- **Test specific package**: `cargo test -p <pkg>`
- **Test specific target**: `cargo test -p <pkg> --test <target>`

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

#### 2. Isolated Integration Tests (Separate Binaries)

Only create separate test binaries if the test:

- **Has process-wide side effects**: Mutates global state (e.g., process-wide global caches or registry setups) that would interfere with other tests running concurrently in the same process.
- **Changes the working directory**: Calls `std::env::set_current_dir` (which can break relative path checks in other concurrent tests like trybuild). Note: Tests doing this should still use a drop guard to restore the directory.
- **Is executed independently in CI**: Targeted individually via a `--test <name>` filter in GitHub Actions workflows to keep that specific CI runner's build/compilation slice minimal.

To add an isolated integration test:

1. Place the test file directly in the root of `tests/` (e.g., `tests/<test_name>.rs`).
2. Add a `[[test]]` entry to the crate's `Cargo.toml`:
   ```toml
   [[test]]
   name = "<test_name>"
   path = "tests/<test_name>.rs"
   ```
3. Do **not** add it to `tests/integration/mod.rs`.
