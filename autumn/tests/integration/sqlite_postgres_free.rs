//! A SQLite build links no Postgres client (issue #2605).
//!
//! `db` is the shared database layer. `postgres` adds the Postgres client.
//! `sqlite` builds on `db` only. These tests read the manifests and ci.yml.

use std::path::{Path, PathBuf};

/// The Postgres-only entries. `sqlite` must reach none of them.
const POSTGRES_ONLY: &[&str] = &[
    "postgres",
    "dep:pq-sys",
    "dep:pq-src",
    "dep:tokio-postgres",
    "dep:tokio-postgres-rustls",
    "dep:webpki-roots",
    "diesel/postgres",
    "diesel-async/postgres",
    "managed-pg",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn manifest(path: &Path) -> toml::Table {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.parse()
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
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

/// Every entry that `feature` turns on, followed through other features.
fn closure(features: &toml::Table, feature: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut queue = vec![feature.to_owned()];
    while let Some(name) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        queue.extend(strings(features.get(&name)));
        seen.push(name);
    }
    seen
}

fn autumn_features() -> toml::Table {
    let autumn = manifest(&workspace_root().join("autumn/Cargo.toml"));
    autumn["features"].as_table().expect("[features]").clone()
}

#[test]
fn sqlite_feature_reaches_no_postgres_entry() {
    let reached = closure(&autumn_features(), "sqlite");
    assert!(reached.contains(&"db".to_owned()), "{reached:?}");
    let leaked: Vec<&String> = reached
        .iter()
        .filter(|entry| POSTGRES_ONLY.contains(&entry.as_str()))
        .collect();
    assert!(
        leaked.is_empty(),
        "`sqlite` must not reach a Postgres entry, found {leaked:?}"
    );
}

#[test]
fn default_and_postgres_features_keep_the_postgres_client() {
    let features = autumn_features();
    let postgres = closure(&features, "postgres");
    let default = closure(&features, "default");
    for entry in POSTGRES_ONLY.iter().filter(|e| **e != "managed-pg") {
        assert!(
            postgres.iter().any(|f| f == entry),
            "`postgres` must reach `{entry}`"
        );
        assert!(
            default.iter().any(|f| f == entry),
            "`default` must reach `{entry}`"
        );
    }
    assert!(
        closure(&features, "managed-pg").contains(&"postgres".to_owned()),
        "`managed-pg` must enable `postgres`"
    );
}

#[test]
fn shared_diesel_entries_do_not_enable_postgres() {
    let root = workspace_root();
    let workspace = manifest(&root.join("Cargo.toml"));
    let shared = workspace["workspace"]["dependencies"]
        .as_table()
        .expect("[workspace.dependencies]");
    let autumn = manifest(&root.join("autumn/Cargo.toml"));
    let own = autumn["dependencies"].as_table().expect("[dependencies]");
    for name in ["diesel", "diesel-async"] {
        for (place, table) in [("workspace", shared), ("autumn", own)] {
            let features = strings(table.get(name).and_then(|d| d.get("features")));
            assert!(
                !features.iter().any(|f| f == "postgres"),
                "{place} `{name}` must not enable `postgres`; autumn-web's \
                 `postgres` feature does: {features:?}"
            );
        }
    }
}

#[test]
fn ci_builds_and_checks_the_pure_sqlite_graph() {
    let ci = std::fs::read_to_string(workspace_root().join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");
    for command in [
        "cargo clippy -p autumn-web --no-default-features --features sqlite --lib",
        "cargo tree -p autumn-web --no-default-features --features sqlite",
    ] {
        assert!(ci.contains(command), "ci.yml must run `{command}`");
    }
}
