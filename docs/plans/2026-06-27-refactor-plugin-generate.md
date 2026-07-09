# Refactor Plugin Generate Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Refactor `autumn-cli/src/generate/plugin.rs` to simplify target directory collision check logic and reuse the `super::naming::pascal` helper.

**Architecture:** Replace the custom directory existence check with streamlined short-circuiting logic and remove `to_pascal_case`, replacing it with `super::naming::pascal`.

**Tech Stack:** Rust (autumn-cli)

---

### Task 1: Refactor target directory collision check and Pascal case helper in generate::plugin

**Files:**
- Modify: `autumn-cli/src/generate/plugin.rs`

**Step 1: Run existing tests**
Run: `cargo test -p autumn-cli --bin autumn generate::plugin::tests`
Expected: PASS

**Step 2: Write minimal implementation**
Apply changes to `autumn-cli/src/generate/plugin.rs`:
- Replace directory checks at lines 37-52 with the simplified logic:
```rust
    if target_dir.exists() && !flags.force {
        let is_empty_dir = target_dir.is_dir() && fs::read_dir(&target_dir)?.next().is_none();
        if !is_empty_dir {
            return Err(GenerateError::Config(format!(
                "target directory '{}' already exists and is not empty. Use --force to override.",
                target_dir.display().to_string().replace('\\', "/")
            )));
        }
    }
```
- Remove `to_pascal_case` utility.
- Replace `to_pascal_case(name)` at line 77:
```rust
    let struct_name = format!("{}Plugin", super::naming::pascal(&name.replace('-', "_")));
```

**Step 3: Run test to verify it passes**
Run: `cargo test -p autumn-cli --bin autumn generate::plugin::tests`
Expected: PASS

**Step 4: Run linter and formatter**
Run: `cargo fmt --package autumn-cli`
Run: `cargo clippy -p autumn-cli --bin autumn`

**Step 5: Commit**
Run:
```bash
git add autumn-cli/src/generate/plugin.rs
git commit -m "refactor: simplify directory collision check and reuse pascal case helper"
```
