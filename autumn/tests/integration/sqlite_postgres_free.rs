//! A `SQLite` build links no Postgres client (issue #2605).
//!
//! `db` is the shared database layer. `postgres` adds the Postgres client.
//! `sqlite` builds on `db` only. These tests read the manifests, ci.yml and
//! the feature-combination script.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Crates that only the Postgres backend needs. ci.yml greps for the same set.
const POSTGRES_CRATES: &[&str] = &[
    "pq-sys",
    "pq-src",
    "tokio-postgres",
    "tokio-postgres-rustls",
    "postgresql_embedded",
];

/// Crates whose `postgres` (or `postgres_backend`) feature is a Postgres
/// backend.
const DIESEL_CRATES: &[&str] = &["diesel", "diesel-async", "diesel_migrations"];

/// Test-only features the `SQLite` CI lane does not build.
const LANE_EXCLUDED: &[&str] = &["test-support", "system-tests", "acme-pebble"];

/// The features `cargo hack --each-feature` always skips, for build cost.
const BASE_HACK_EXCLUDED: &[&str] = &[
    "managed-pg-bundled",
    "managed-pg",
    "system-tests",
    "test-support",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = workspace_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn manifest(relative: &str) -> toml::Table {
    read(relative)
        .parse()
        .unwrap_or_else(|e| panic!("parse {relative}: {e}"))
}

fn strings(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn autumn_features() -> toml::Table {
    manifest("autumn/Cargo.toml")["features"]
        .as_table()
        .expect("[features]")
        .clone()
}

/// Every entry that `feature` turns on, followed through other features.
fn closure(features: &toml::Table, feature: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![feature.to_owned()];
    while let Some(name) = queue.pop() {
        if seen.insert(name.clone()) {
            queue.extend(strings(features.get(&name)));
        }
    }
    seen
}

/// Whether one `[features]` entry turns on a Postgres crate or backend.
///
/// `dep:x`, `x/f` and `x?/f` all name crate `x`. `x?/f` only adds `f` when
/// `x` is on for another reason, but it still turns on a Postgres backend.
fn is_postgres_entry(entry: &str) -> bool {
    if entry == "postgres" {
        return true;
    }
    let entry = entry.strip_prefix("dep:").unwrap_or(entry);
    let (krate, feature) = entry.split_once('/').unwrap_or((entry, ""));
    let krate = krate.trim_end_matches('?');
    POSTGRES_CRATES.contains(&krate)
        || (DIESEL_CRATES.contains(&krate) && matches!(feature, "postgres" | "postgres_backend"))
}

fn postgres_entries(features: &toml::Table, feature: &str) -> Vec<String> {
    closure(features, feature)
        .into_iter()
        .filter(|entry| is_postgres_entry(entry))
        .collect()
}

/// The `SQLITE_ONLY_FEATURES` value in ci.yml.
fn ci_lane(ci: &str) -> Vec<String> {
    let line = ci
        .lines()
        .find_map(|l| l.trim().strip_prefix("SQLITE_ONLY_FEATURES:"))
        .expect("ci.yml must set SQLITE_ONLY_FEATURES");
    line.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .split(',')
        .map(str::to_owned)
        .collect()
}

#[test]
fn entry_classifier_reads_every_spelling() {
    for entry in [
        "postgres",
        "dep:pq-sys",
        "tokio-postgres/runtime",
        "diesel/postgres",
        "diesel?/postgres",
        "diesel/postgres_backend",
        "diesel_migrations/postgres",
    ] {
        assert!(is_postgres_entry(entry), "{entry}");
    }
    for entry in ["db", "dep:diesel", "diesel/sqlite", "dep:rustls"] {
        assert!(!is_postgres_entry(entry), "{entry}");
    }
}

#[test]
fn sqlite_feature_reaches_no_postgres_entry() {
    let features = autumn_features();
    assert!(closure(&features, "sqlite").contains("db"));
    let leaked = postgres_entries(&features, "sqlite");
    assert!(leaked.is_empty(), "`sqlite` reaches {leaked:?}");
}

#[test]
fn default_and_postgres_features_keep_the_postgres_client() {
    let features = autumn_features();
    for feature in ["postgres", "default", "managed-pg"] {
        let reached = closure(&features, feature);
        for entry in [
            "postgres",
            "dep:pq-sys",
            "dep:tokio-postgres",
            "diesel/postgres",
        ] {
            assert!(reached.contains(entry), "`{feature}` must reach `{entry}`");
        }
    }
}

#[test]
fn shared_diesel_entries_do_not_enable_postgres() {
    let workspace = manifest("Cargo.toml");
    let shared = workspace["workspace"]["dependencies"]
        .as_table()
        .expect("[workspace.dependencies]");
    let autumn = manifest("autumn/Cargo.toml");
    let own = autumn["dependencies"].as_table().expect("[dependencies]");
    for name in DIESEL_CRATES {
        for (place, table) in [("workspace", shared), ("autumn", own)] {
            let features = strings(table.get(*name).and_then(|d| d.get("features")));
            assert!(
                !features
                    .iter()
                    .any(|f| matches!(f.as_str(), "postgres" | "postgres_backend")),
                "{place} `{name}` must not enable Postgres; autumn-web's \
                 `postgres` feature does: {features:?}"
            );
        }
    }
}

#[test]
fn ci_builds_and_checks_the_pure_sqlite_graph() {
    let ci = read(".github/workflows/ci.yml");
    for line in [
        r#"cargo clippy -p autumn-web --no-default-features --features "$SQLITE_ONLY_FEATURES" --lib"#,
        r#"cargo tree -p autumn-web --no-default-features --features "$SQLITE_ONLY_FEATURES""#,
        r#"grep -q '^libsqlite3-sys ' "$tree""#,
    ] {
        assert!(ci.contains(line), "ci.yml must run `{line}`");
    }
    let grep = ci
        .lines()
        .find(|l| l.contains("if grep -E '^(") && l.contains(r#""$tree""#))
        .expect("ci.yml must grep the tree for Postgres crates");
    let alternation = grep
        .split_once("'^(")
        .and_then(|(_, rest)| rest.split_once(") '"))
        .map(|(names, _)| names)
        .expect("the CI grep must be `'^(a|b|...) '`");
    let grepped: BTreeSet<&str> = alternation.split('|').collect();
    let expected: BTreeSet<&str> = POSTGRES_CRATES.iter().copied().collect();
    assert_eq!(
        grepped, expected,
        "the CI grep and POSTGRES_CRATES must match"
    );
}

#[test]
fn ci_lane_reaches_no_postgres_and_covers_every_other_feature() {
    let features = autumn_features();
    let lane = ci_lane(&read(".github/workflows/ci.yml"));
    assert!(lane.iter().any(|f| f == "sqlite"), "{lane:?}");

    let mut covered = BTreeSet::new();
    for feature in &lane {
        assert!(
            features.contains_key(feature),
            "SQLITE_ONLY_FEATURES names an unknown feature `{feature}`"
        );
        let leaked = postgres_entries(&features, feature);
        assert!(
            leaked.is_empty(),
            "lane entry `{feature}` reaches {leaked:?}"
        );
        covered.extend(closure(&features, feature));
    }

    // A new feature that does not need Postgres joins the lane, or the
    // exclusion list with a reason.
    let missing: Vec<&String> = features
        .keys()
        .filter(|f| !covered.contains(*f) && !LANE_EXCLUDED.contains(&f.as_str()))
        .filter(|f| postgres_entries(&features, f).is_empty())
        .collect();
    assert!(
        missing.is_empty(),
        "add to SQLITE_ONLY_FEATURES in ci.yml: {missing:?}"
    );
}

#[test]
fn each_feature_sweep_skips_exactly_the_features_with_no_backend() {
    let features = autumn_features();
    let no_backend: BTreeSet<String> = features
        .keys()
        .filter(|f| {
            let reached = closure(&features, f);
            reached.contains("db") && !reached.contains("postgres") && !reached.contains("sqlite")
        })
        .cloned()
        .collect();
    assert!(no_backend.contains("db"), "{no_backend:?}");

    let script = read("scripts/check-feature-combinations.sh");
    let list = script
        .lines()
        .find_map(|l| l.trim().strip_prefix("--exclude-features "))
        .expect("the script must pass --exclude-features");
    let skipped: BTreeSet<String> = list
        .trim_end_matches('\\')
        .trim()
        .split(',')
        .filter(|f| !BASE_HACK_EXCLUDED.contains(f))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        skipped, no_backend,
        "each feature that enables `db` but no backend must be skipped by the \
         sweep (and built with a backend in Phase B), and nothing else"
    );
}

#[test]
fn db_without_a_backend_stops_at_compile_error() {
    let lib = read("autumn/src/lib.rs");
    let gate = r#"#[cfg(all(feature = "db", not(any(feature = "postgres", feature = "sqlite"))))]"#;
    let at = lib.find(gate).expect("lib.rs must gate the backend check");
    assert!(
        lib[at + gate.len()..]
            .trim_start()
            .starts_with("compile_error!("),
        "the backend gate must guard a `compile_error!`"
    );
}
