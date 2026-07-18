# Catch Typo'd autumn.toml Keys Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Detect misspelled keys in `autumn.toml` (and profile files), naming the full dotted path and suggesting the closest match using Levenshtein distance, with an opt-in strict fail-fast gate for CI/startup.

**Architecture:** 
1. Implement a generic Serde `Deserializer` in `autumn-web` that walks `AutumnConfig` at compile/runtime to extract all valid dotted paths (avoiding duplication and manually maintained lists).
2. Share the existing `levenshtein()` function in `autumn-web::config` by making it public.
3. Add a recursive validator in `autumn-cli` (or shared in `autumn-web`) that checks a TOML table against the extracted schema.
4. Integrate this validator into `autumn doctor` and `autumn check --config`, exiting non-zero when warnings/typos are found in strict/check mode.

**Tech Stack:** Rust, Serde, TOML, Clap

---

### Task 1: Expose levenshtein and implement SchemaExtractor

**Files:**
- Modify: [autumn/src/config.rs](file:///c:/Users/markm/autumn/autumn/src/config.rs)

**Step 1: Write the failing test**
We will add unit tests for `SchemaExtractor` in `autumn/src/config.rs` asserting it extracts fields from a test struct and from `AutumnConfig`.

**Step 2: Run test to verify it fails**
Run: `cargo test -p autumn-web --lib config::tests::test_schema_extractor`

**Step 3: Write minimal implementation**
- Make `fn levenshtein` public: `pub fn levenshtein(a: &str, b: &str) -> usize`.
- Implement `SchemaDeserializer` and helper types implementing `serde::Deserializer` to traverse structs and collect their field names recursively.
- Implement `AutumnConfig::get_schema_keys() -> HashMap<String, HashSet<String>>` which runs `AutumnConfig::deserialize(SchemaDeserializer::new())`.

**Step 4: Run test to verify it passes**
Expected: PASS

**Step 5: Commit**
`git commit -m "feat: implement SchemaExtractor and expose levenshtein"`

---

### Task 2: Implement Recursive TOML Validator

**Files:**
- Modify: [autumn-cli/src/doctor.rs](file:///c:/Users/markm/autumn/autumn-cli/src/doctor.rs)
- Modify: [autumn-cli/Cargo.toml](file:///c:/Users/markm/autumn/autumn-cli/Cargo.toml) (enable more features for schema extraction)

**Step 1: Write the failing test**
Write a test in `doctor.rs` checking `validate_toml_content` with:
- A valid table.
- A table with a typo (e.g. `[database] primry_url = "..."`) returning the correct dotted path and suggestion.
- An unknown key with no close match returning no suggestion.

**Step 2: Run test to verify it fails**
Run: `cargo test -p autumn-cli --lib doctor::tests::test_validate_toml`

**Step 3: Write minimal implementation**
- Implement `validate_toml_content(content: &str, schema: &HashMap<String, HashSet<String>>) -> Vec<(String, Option<String>)>` which recursively traverses the TOML table, handles `[profile.<name>]` paths, and finds closest matches via `levenshtein`.
- Update `check_toml_content` to use this recursive validation and report all findings.

**Step 4: Run test to verify it passes**
Expected: PASS

**Step 5: Commit**
`git commit -m "feat: implement recursive TOML validation with suggestions"`

---

### Task 3: Support Merged Profiles and CLI Gates

**Files:**
- Modify: [autumn-cli/src/doctor.rs](file:///c:/Users/markm/autumn/autumn-cli/src/doctor.rs)
- Modify: [autumn-cli/src/main.rs](file:///c:/Users/markm/autumn/autumn-cli/src/main.rs)
- Modify: [autumn-cli/src/check.rs](file:///c:/Users/markm/autumn/autumn-cli/src/check.rs)

**Step 1: Write the failing test**
Write tests for `autumn check --config` exit status and merged profile validation.

**Step 2: Run test to verify it fails**
Run: `cargo test -p autumn-cli`

**Step 3: Write minimal implementation**
- In `doctor.rs`, find all `autumn-*.toml` files in the current directory and all inline profiles, merge and validate them.
- In `main.rs`, add `--config` flag to `Check` subcommand.
- In `check.rs`, implement `run_config_check` which runs the validation and exits non-zero on any warning/typos.

**Step 4: Run test to verify it passes**
Expected: PASS

**Step 5: Commit**
`git commit -m "feat: add check --config and validate profile overlays"`
