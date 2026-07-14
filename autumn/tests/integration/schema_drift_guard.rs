//! CI guard: schema-key snapshot integrity.
//!
//! Ensures that any config key removed from the compiled schema is either:
//!   (a) still present in the schema (no drift), or
//!   (b) registered in `DEPRECATED_CONFIG_KEYS` (proper deprecation ramp).
//!
//! # Coverage scope (important)
//!
//! `schema_leaf_paths()` is derived by the `SchemaDeserializer`, which only
//! descends into structs defined in `config.rs` itself. External-module config
//! types (e.g. `SecurityConfig`, `AuthConfig`, `SessionConfig`) appear in the
//! snapshot only as single-segment *root* leaves (`security`, `auth`, …) — their
//! nested fields are NOT in the snapshot. This is deliberate: `get_schema_keys`
//! is shared with the strict unknown-key validator, so widening it would change
//! runtime validation behavior.
//!
//! Consequences for the registered deprecated keys, which live in external
//! modules (`security.rate_limit.*`):
//!   * Removal of the whole `security` section IS caught here (the root leaf
//!     disappears and the registry-root check below fires).
//!   * Removal of an individual external *leaf* (e.g. the `trusted_proxies`
//!     field) is NOT visible to this snapshot. That case is instead guarded by
//!     the honored-value integration tests in `tests/config_deprecation.rs`,
//!     which access the fields directly (deletion breaks compilation) and assert
//!     each registered key still loads and still emits its WARN.
//!
//! # Regenerating the snapshot
//!
//! Run with `UPDATE_SCHEMA_SNAPSHOT=1` to write the current schema to the
//! snapshot file:
//!
//! ```
//! UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p autumn-web schema_keys_snapshot_guard
//! ```
//!
//! The snapshot is generated with every optional root section compiled in
//! (`cargo test --all-features`), so it always contains `i18n`/`mail`/
//! `storage`/`http`/`reporting` alongside the always-on sections. A handful of
//! `AutumnConfig` root fields are gated behind an optional cargo feature (see
//! [`FEATURE_GATED_ROOTS`]); when this test binary is compiled without one of
//! those features (e.g. `cargo test -p autumn-web --features maud`, which
//! carries only the crate's own default features), that root section is
//! legitimately absent from `schema_leaf_paths()` — not a real removal — so
//! the guard below excludes it rather than failing.

use autumn_web::config::{AutumnConfig, deprecated_config_keys};
use std::collections::BTreeSet;

/// Pairs of (is this optional feature enabled in this build, its
/// `AutumnConfig` root section) for every field in `config.rs` gated behind
/// `#[cfg(feature = "...")]`. Keep in sync with `config.rs`'s `AutumnConfig`
/// struct — [`feature_gated_roots_mapping_matches_config_when_enabled`] below
/// self-checks this list whenever a listed feature happens to be enabled.
const fn feature_gated_roots() -> [(bool, &'static str); 6] {
    [
        (cfg!(feature = "i18n"), "i18n"),
        (cfg!(feature = "mail"), "mail"),
        (cfg!(feature = "storage"), "storage"),
        // NOTE: `[backup]` (issue #1619) is intentionally NOT here — it is a
        // non-feature-gated root present in every build, so it always appears in
        // the schema and needs no gating exclusion.
        (cfg!(feature = "http-client"), "http"),
        (cfg!(feature = "reporting"), "reporting"),
        (cfg!(feature = "maud"), "stories"),
    ]
}

const SNAPSHOT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/schema_keys.snapshot",
);

fn load_snapshot() -> BTreeSet<String> {
    let content = std::fs::read_to_string(SNAPSHOT_PATH)
        .expect("schema_keys.snapshot missing; run with UPDATE_SCHEMA_SNAPSHOT=1 to generate");
    content
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn write_snapshot(leaves: &BTreeSet<String>) {
    let content: String = leaves.iter().fold(String::new(), |mut s, l| {
        s.push_str(l);
        s.push('\n');
        s
    });
    std::fs::write(SNAPSHOT_PATH, content).expect("failed to write schema_keys.snapshot");
}

/// The set of recognized root sections (first dotted segment of each schema leaf).
fn schema_root_sections() -> BTreeSet<String> {
    AutumnConfig::schema_leaf_paths()
        .iter()
        .filter_map(|leaf| leaf.split('.').next().map(str::to_owned))
        .collect()
}

// ── CI guard: snapshot integrity ──────────────────────────────────────────────

#[test]
fn schema_keys_snapshot_guard() {
    let current = AutumnConfig::schema_leaf_paths();

    if std::env::var("UPDATE_SCHEMA_SNAPSHOT").is_ok() {
        write_snapshot(&current);
        println!("schema_keys.snapshot updated with {} keys", current.len());
        return;
    }

    let snapshot = load_snapshot();
    let registry: BTreeSet<&str> = deprecated_config_keys().iter().map(|d| d.path).collect();
    let disabled_roots: BTreeSet<&str> = feature_gated_roots()
        .into_iter()
        .filter(|(enabled, _)| !enabled)
        .map(|(_, root)| root)
        .collect();

    // Keys in snapshot but absent from current schema without a registry
    // entry, excluding root sections whose gating feature isn't compiled
    // into this test binary (see module docs).
    let removed_without_deprecation: Vec<&str> = snapshot
        .iter()
        .filter(|k| !current.contains(k.as_str()))
        .filter(|k| !registry.contains(k.as_str()))
        .filter(|k| !disabled_roots.contains(k.split('.').next().unwrap_or(k.as_str())))
        .map(String::as_str)
        .collect();

    assert!(
        removed_without_deprecation.is_empty(),
        "Schema keys were removed without a corresponding DEPRECATED_CONFIG_KEYS entry.\n\
         Register a deprecation for each key below, or regenerate the snapshot if the \
         removal is intentional:\n{removed_without_deprecation:?}\n\n\
         Regenerate: UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p autumn-web schema_keys_snapshot_guard",
    );

    // Keys in current schema but absent from snapshot → prompt to regenerate.
    let added_without_snapshot: Vec<&str> = current
        .iter()
        .filter(|k| !snapshot.contains(k.as_str()))
        .map(String::as_str)
        .collect();

    assert!(
        added_without_snapshot.is_empty(),
        "New schema keys are not in the snapshot; regenerate it:\n{added_without_snapshot:?}\n\n\
         Regenerate: UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p autumn-web schema_keys_snapshot_guard",
    );
}

/// Live registry/schema linkage: every deprecated key's root section must still
/// be a recognized config section. This catches a registry entry pointing at a
/// section that was renamed or removed wholesale — which the snapshot's
/// leaf-level diff cannot see for external-module keys. (Leaf-level honoring is
/// covered by `tests/config_deprecation.rs`; see the module docs above.)
#[test]
fn every_registered_deprecated_key_has_a_known_root_section() {
    let roots = schema_root_sections();
    let orphaned: Vec<&str> = deprecated_config_keys()
        .iter()
        .filter(|d| {
            d.path
                .split('.')
                .next()
                .is_none_or(|root| !roots.contains(root))
        })
        .map(|d| d.path)
        .collect();

    assert!(
        orphaned.is_empty(),
        "DEPRECATED_CONFIG_KEYS entries reference unknown root sections \
         (the section was renamed or removed): {orphaned:?}\nKnown roots: {roots:?}",
    );
}

/// Self-check for [`feature_gated_roots`]: whenever a listed feature happens
/// to be enabled in this build (e.g. via workspace feature unification with
/// `cargo test --workspace`, or `--all-features`), its mapped root section
/// must actually appear in the compiled schema. Catches the mapping going
/// stale (a feature renamed, or a root field renamed/removed) instead of
/// silently letting a real removal slip past the guard above as "expected".
#[test]
fn feature_gated_roots_mapping_matches_config_when_enabled() {
    let current = AutumnConfig::schema_leaf_paths();
    let stale: Vec<&str> = feature_gated_roots()
        .into_iter()
        .filter(|(enabled, _)| *enabled)
        .map(|(_, root)| root)
        .filter(|root| !current.iter().any(|k| k.split('.').next() == Some(*root)))
        .collect();

    assert!(
        stale.is_empty(),
        "feature_gated_roots() in this file is stale: these features are enabled in this \
         build but their mapped root section is missing from the compiled schema: {stale:?}\n\
         Update the mapping to match config.rs's current #[cfg(feature = \"...\")] fields.",
    );
}

// ── unit: schema_leaf_paths content ───────────────────────────────────────────

#[test]
fn schema_leaf_paths_contains_known_paths() {
    let leaves = AutumnConfig::schema_leaf_paths();
    // A config.rs-internal nested leaf is fully covered (deep recursion works).
    assert!(
        leaves.contains("server.port"),
        "server.port must be a schema leaf"
    );
    // External-module types appear only as root leaves (see module docs); the
    // deep `security.rate_limit.*` keys are intentionally NOT here — they are
    // honored-checked in tests/config_deprecation.rs.
    assert!(
        leaves.contains("security"),
        "security must appear as a root-level schema leaf"
    );
    assert!(
        !leaves.contains("security.rate_limit.trusted_proxies"),
        "external-module leaves are not in the schema snapshot by design; \
         if this changed, revisit the guard's coverage assumptions"
    );
}

/// Regression guard for the `[deploy]` strict-validation fix.
///
/// `DeployConfig` is a config.rs-internal struct with a fixed, known schema, so
/// the strict unknown-key validator MUST descend into `[deploy]` and reject a
/// typo like `[deploy] app_dr = "…"`. This only holds because `deploy` is
/// declared before `database` in `AutumnConfig` — `DatabaseConfig`'s
/// `deserialize_with` duration field aborts the `SchemaDeserializer` traversal
/// for every field after it (see the doc comment on `AutumnConfig::deploy`). If
/// someone moves `deploy` below `database`, its child keys silently vanish from
/// the schema and this test fails.
#[test]
fn deploy_child_keys_are_strictly_validated() {
    let leaves = AutumnConfig::schema_leaf_paths();
    for key in [
        "deploy.host",
        "deploy.user",
        "deploy.ssh_port",
        "deploy.app_name",
        "deploy.app_dir",
        "deploy.service_name",
        "deploy.readiness_timeout_secs",
        "deploy.keep_releases",
    ] {
        assert!(
            leaves.contains(key),
            "{key} must be a schema leaf so strict validation descends into [deploy]; \
             if this fails, `deploy` was likely moved below `database` in AutumnConfig"
        );
    }

    // End-to-end: the strict validator flags an unknown child key under [deploy].
    let schema = AutumnConfig::get_schema_keys();
    let errors = AutumnConfig::validate_toml("[deploy]\napp_dr = \"/srv/app\"\n", &schema);
    assert!(
        errors.iter().any(|(path, _)| path == "deploy.app_dr"),
        "a bogus [deploy] child key must be rejected by strict validation, got: {errors:?}"
    );
}

/// #1890: config.rs-internal sections declared AFTER `database` used to vanish
/// from the derived schema because the `statement_timeout` duration field
/// aborted the `SchemaDeserializer` walk. The tolerant `deserialize_any` probe
/// keeps the walk descending, so these sections now expose their child keys and
/// the strict validator can flag typos in them.
#[test]
fn post_database_sections_are_now_schema_covered() {
    let schema = AutumnConfig::get_schema_keys();
    // Roots confirmed (from the regenerated snapshot) to be config.rs-internal
    // structs that expand to child keys after the fix.
    for root in ["log", "cache", "jobs", "telemetry"] {
        assert!(
            schema.get(root).is_some_and(|k| !k.is_empty()),
            "[{root}] must have child keys after the #1890 fix; empty means the walk aborted before it"
        );
    }
    let errors = AutumnConfig::validate_toml("[log]\nnot_a_real_key = true\n", &schema);
    assert!(
        errors.iter().any(|(p, _)| p == "log.not_a_real_key"),
        "a bogus [log] child key must be flagged now, got: {errors:?}"
    );
}
