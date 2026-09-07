//! Every `sqlite*` `[[test]]` target must run in CI (issue #1924).
//!
//! ci.yml's "Run the sqlite integration suite" step is an explicit `--test`
//! allowlist, not a sweep — the `sqlite` feature is a backend flip that cannot
//! be enabled alongside the Postgres lane, so there is no bare `--ignored` run
//! to catch a target nobody listed. A new `autumn/tests/sqlite_*.rs` target left
//! out of that list compiles locally, passes locally, and never runs in CI.
//!
//! This test closes that gap by comparing the manifest against the workflow.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// `[[test]] name = "sqlite…"` entries declared in `autumn/Cargo.toml`.
fn declared_sqlite_test_targets(manifest: &str) -> Vec<String> {
    let value: toml::Value = toml::from_str(manifest).expect("autumn/Cargo.toml parses");
    value
        .get("test")
        .and_then(toml::Value::as_array)
        .map(|targets| {
            targets
                .iter()
                .filter_map(|t| t.get("name").and_then(toml::Value::as_str))
                .filter(|name| name.starts_with("sqlite"))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn every_sqlite_test_target_is_named_in_ci() {
    let root = workspace_root();
    let manifest =
        std::fs::read_to_string(root.join("autumn/Cargo.toml")).expect("read autumn/Cargo.toml");
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");

    let declared = declared_sqlite_test_targets(&manifest);
    assert!(
        !declared.is_empty(),
        "expected some sqlite [[test]] targets — did the manifest shape change?"
    );

    // Anchored on the token boundary: a bare `contains` would let a future
    // `sqlite_crud` pass on the listed `--test sqlite_crud_basic` and never run.
    let missing: Vec<&String> = declared
        .iter()
        .filter(|name| {
            !ci.match_indices(&format!("--test {name}")).any(|(at, m)| {
                ci[at + m.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_')
            })
        })
        .collect();
    assert!(
        missing.is_empty(),
        "these sqlite test targets never run in CI — add `--test <name>` to ci.yml's \
         \"Run the sqlite integration suite\" step (or another named sqlite step): {missing:?}"
    );
}

/// The anchoring above, pinned: a listed longer name must not satisfy a
/// shorter, unlisted one.
#[test]
fn a_prefix_of_a_listed_target_is_still_reported_missing() {
    let ci = "            --test sqlite_crud_basic \\\n";
    let anchored = |name: &str| {
        ci.match_indices(&format!("--test {name}")).any(|(at, m)| {
            ci[at + m.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        })
    };
    assert!(anchored("sqlite_crud_basic"));
    assert!(!anchored("sqlite_crud"));
}

#[test]
fn declared_sqlite_test_targets_reads_the_manifest() {
    let manifest = r#"
[[test]]
name = "sqlite_thing"
path = "tests/sqlite_thing.rs"

[[test]]
name = "other"
path = "tests/other.rs"
"#;
    assert_eq!(declared_sqlite_test_targets(manifest), vec!["sqlite_thing"]);
}
