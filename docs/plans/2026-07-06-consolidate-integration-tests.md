# Consolidate Integration Tests Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Consolidate all integration tests in `autumn` and `autumn-cli` into single integration test binaries, structured under `integration/` subdirectories.

**Architecture:** Move test files into `tests/integration/`, expose them through `integration/mod.rs`, disable autotests, and define a single test target per crate.

**Tech Stack:** Rust / Cargo

---

### Task 1: Update `autumn/Cargo.toml`
**Files:**
- Modify: `autumn/Cargo.toml`
Add `autotests = false` in `[package]`, and declare `[[test]]` named `integration_tests`.

### Task 2: Restructure `autumn` tests
**Files:**
- Create: `autumn/tests/integration/mod.rs`
- Create: `autumn/tests/integration_tests.rs`
- Delete: `autumn/tests/security_tests.rs`
- Move all test files directly under `autumn/tests/` to `autumn/tests/integration/`.
- Move `autumn/tests/security/` directory to `autumn/tests/integration/security/`.

### Task 3: Update `autumn-cli/Cargo.toml`
**Files:**
- Modify: `autumn-cli/Cargo.toml`
Add `autotests = false` in `[package]`, and declare `[[test]]` named `cli_tests`.

### Task 4: Restructure `autumn-cli` tests
**Files:**
- Create: `autumn-cli/tests/integration/mod.rs`
- Create: `autumn-cli/tests/cli_tests.rs`
- Move all test files directly under `autumn-cli/tests/` to `autumn-cli/tests/integration/`.
- Move `autumn-cli/tests/common/mod.rs` to `autumn-cli/tests/integration/common.rs`.
- Update `config.rs`, `experiments.rs`, and `flags.rs` to use `use crate::integration::common;` instead of `mod common;`.
