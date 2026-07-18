# autumn generate plugin subcommand implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement `autumn generate plugin <NAME>` to scaffold a conformant plugin crate containing `Cargo.toml`, `src/lib.rs`, `README.md`, and `tests/conformance.rs` that builds and runs `cargo test` immediately with zero configuration.

**Architecture:** Extend the `autumn-cli` generate subcommands with a `Plugin` variant. Use the CLI's `Plan` emission engine to create the files in a target directory `autumn-<snake_name>-plugin` or a custom `--path` if specified. Pin `autumn-web` version dynamically to the CLI's major/minor version.

**Tech Stack:** Rust, Clap (CLI parsing), Cargo (package management).

---

### Task 1: Create the plugin generator skeleton and tests

**Files:**
- Create: `autumn-cli/src/generate/plugin.rs`
- Modify: `autumn-cli/src/generate/mod.rs`

**Step 1: Write the failing test**
In `autumn-cli/src/generate/plugin.rs`, write a unit test `plan_creates_plugin_files` verifying that `plan_plugin` creates `Cargo.toml`, `src/lib.rs`, `README.md`, and `tests/conformance.rs` at the correct target path.

**Step 2: Run test to verify it fails**
Run: `cargo test -p autumn-cli --lib generate::plugin::tests`
Expected: Fails because the module doesn't exist or isn't declared.

**Step 3: Write minimal implementation**
Declare `pub mod plugin;` in `autumn-cli/src/generate/mod.rs` and write a stub `plan_plugin` function in `autumn-cli/src/generate/plugin.rs` returning a plan with those files.

**Step 4: Run test to verify it passes**
Run: `cargo test -p autumn-cli --lib generate::plugin::tests`
Expected: PASS

**Step 5: Commit**

---

### Task 2: Implement full file generation with dynamic version pinning

**Files:**
- Modify: `autumn-cli/src/generate/plugin.rs`

**Step 1: Write the failing test**
Add a test `plan_includes_correct_contents_and_conformance_run` checking that:
- `Cargo.toml` contains `autumn-web` pinned to the correct major/minor (e.g. `0.5` if `env!("CARGO_PKG_VERSION")` is `0.5.0`).
- `src/lib.rs` implements `Plugin` with `name()` and a commented example route contribution.
- `tests/conformance.rs` contains a test invoking `plugin_conformance::run_conformance`.
- Non-empty directory collision check is enforced and fails cleanly with an error.

**Step 2: Run test to verify it fails**
Run: `cargo test -p autumn-cli --lib generate::plugin::tests`
Expected: Fails because templates/contents are missing and directory collisions aren't checked.

**Step 3: Write minimal implementation**
- Extract the major/minor version from `env!("CARGO_PKG_VERSION")`.
- Implement `plan_plugin` to generate complete templates for `Cargo.toml`, `src/lib.rs`, `README.md`, and `tests/conformance.rs`.
- Add a directory collision check at the start of `plan_plugin` and return `GenerateError::Config` or a custom error when target directory exists and is not empty.

**Step 4: Run test to verify it passes**
Run: `cargo test -p autumn-cli --lib generate::plugin::tests`
Expected: PASS

**Step 5: Commit**

---

### Task 3: Wire into the main CLI interface

**Files:**
- Modify: `autumn-cli/src/main.rs`

**Step 1: Write the failing test**
We will add `Plugin` to `GenerateCommands` Clap CLI definition in `autumn-cli/src/main.rs`.

**Step 2: Run test to verify it fails**
Run: `cargo check -p autumn-cli`
Expected: Fails because `GenerateCommands::Plugin` is not handled in the match statements in `autumn-cli/src/main.rs`.

**Step 3: Write minimal implementation**
- Add `GenerateCommands::Plugin` variant to `GenerateCommands` in `autumn-cli/src/main.rs` with fields `name`, `path`, `dry_run`, `force`.
- Document it with a doc-comment detailing the emitted file plan.
- Implement the match arm in `run_generate_command` to execute the plugin generator.

**Step 4: Run test to verify it passes**
Run: `cargo test -p autumn-cli`
Expected: PASS

**Step 5: Commit**

---

### Task 4: End-to-end Verification

**Step 1: Scaffold a new plugin**
Run: `cargo run --bin autumn -- generate plugin Foo --path temp_foo_plugin`
Verify:
- Files are created under `temp_foo_plugin`.
- `Cargo.toml` contains `autumn-web` version matching the workspace version.
- Conformance test exists in `tests/conformance.rs`.

**Step 2: Verify `cargo test` runs and passes on the generated plugin**
We'll run `cargo test` in the generated plugin's directory. Since it relies on the workspace's `patch.crates-io`, we will temporarily add it as a workspace member or run it with path override to confirm it builds and tests pass out of the box.

**Step 3: Cleanup**
Remove the temporary folder `temp_foo_plugin`.
