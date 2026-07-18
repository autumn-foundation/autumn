//! CI guard: schema-key snapshot integrity.
//!
//! Ensures that any config key removed from the compiled schema is either:
//!   (a) still present in the schema (no drift), or
//!   (b) registered in `DEPRECATED_CONFIG_KEYS` (proper deprecation ramp).
//!
//! # Coverage scope (important)
//!
//! `schema_leaf_paths()` is derived by the `SchemaDeserializer`, which recurses
//! into every derived-`Deserialize` struct it reaches, regardless of the module
//! the type is declared in. External-module config types (e.g. `SecurityConfig`,
//! `AuthConfig`, `SessionConfig`) therefore expose their nested fields in the
//! snapshot just like config.rs-internal types. (Before the #1890 adaptive
//! multi-pass walk, sections declared after a walk-aborting field — the
//! `database.statement_timeout` duration, or the seq/map-only `jobs.queues`
//! visitor — appeared only as single-segment *root* leaves; that was an artifact
//! of the abort, not of their module.) `get_schema_keys` is shared with the
//! strict unknown-key validator, so these nested external keys are now covered by
//! strict validation too (warn-first during the #1890 rollout).
//!
//! Consequences for the registered deprecated keys, which live in external
//! modules (`security.rate_limit.*`):
//!   * Removal of the whole `security` section IS caught here (its root and
//!     child leaves disappear and the registry-root check below fires).
//!   * Removal of an individual external *leaf* (e.g. the `trusted_proxies`
//!     field) is now visible to this snapshot as well, and remains additionally
//!     guarded by the honored-value integration tests in
//!     `tests/config_deprecation.rs`, which access the fields directly (deletion
//!     breaks compilation) and assert each registered key still loads and still
//!     emits its WARN.
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

/// Feature-gated NESTED keys (not whole root sections). `auth` is always
/// present, but its `webauthn` subtree only compiles under the `webauthn`
/// feature, so when that feature is off these snapshot leaves are legitimately
/// absent from `schema_leaf_paths()` — exclude by prefix rather than failing.
/// Keep in sync with `config.rs`'s `#[cfg(feature = "...")]` nested fields —
/// [`feature_gated_leaf_prefixes_match_config_when_enabled`] below self-checks
/// this list whenever a listed feature happens to be enabled.
const fn feature_gated_leaf_prefixes() -> [(bool, &'static str); 1] {
    [(cfg!(feature = "webauthn"), "auth.webauthn")]
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
    let disabled_leaf_prefixes: Vec<&str> = feature_gated_leaf_prefixes()
        .into_iter()
        .filter(|(enabled, _)| !enabled)
        .map(|(_, prefix)| prefix)
        .collect();

    // Keys in snapshot but absent from current schema without a registry
    // entry, excluding root sections whose gating feature isn't compiled
    // into this test binary, and nested subtrees whose gating feature is off
    // (e.g. `auth.webauthn.*` under the always-present `auth` root — see module
    // docs and `feature_gated_leaf_prefixes`).
    let removed_without_deprecation: Vec<&str> = snapshot
        .iter()
        .filter(|k| !current.contains(k.as_str()))
        .filter(|k| !registry.contains(k.as_str()))
        .filter(|k| !disabled_roots.contains(k.split('.').next().unwrap_or(k.as_str())))
        .filter(|k| {
            !disabled_leaf_prefixes
                .iter()
                .any(|p| k.as_str() == *p || k.strip_prefix(p).is_some_and(|r| r.starts_with('.')))
        })
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

/// Self-check for [`feature_gated_leaf_prefixes`]: whenever a listed feature
/// happens to be enabled in this build (e.g. via `--all-features`, or workspace
/// feature unification with `cargo test --workspace`), its mapped nested prefix
/// must actually appear in the compiled schema. Catches the prefix list going
/// stale (a feature renamed, or the nested field renamed/removed) instead of
/// silently letting a real removal slip past the guard above as "expected".
#[test]
fn feature_gated_leaf_prefixes_match_config_when_enabled() {
    let current = AutumnConfig::schema_leaf_paths();
    let stale: Vec<&str> = feature_gated_leaf_prefixes()
        .into_iter()
        .filter(|(enabled, _)| *enabled)
        .map(|(_, prefix)| prefix)
        .filter(|prefix| {
            !current.iter().any(|k| {
                k.as_str() == *prefix || k.strip_prefix(*prefix).is_some_and(|r| r.starts_with('.'))
            })
        })
        .collect();

    assert!(
        stale.is_empty(),
        "feature_gated_leaf_prefixes() in this file is stale: these features are enabled in \
         this build but their mapped nested prefix is missing from the compiled schema: \
         {stale:?}\nUpdate the mapping to match config.rs's current \
         #[cfg(feature = \"...\")] nested fields.",
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
    // External-module types now descend too (the #1890 adaptive walk no longer
    // aborts before them), so both the bare `security` root leaf AND its nested
    // fields appear in the schema.
    assert!(
        leaves.contains("security"),
        "security must appear as a root-level schema leaf"
    );
    assert!(
        leaves.contains("security.rate_limit.trusted_proxies"),
        "external-module leaves now descend into the schema after the #1890 \
         adaptive-walker fix; if this vanished, the walk is aborting before \
         [security] again"
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

/// #1890 (follow-up): `jobs.queues` uses a seq/map-only visitor
/// (`JobQueuesConfig`) that rejected the scalar `deserialize_any` probe and
/// aborted the walk at `jobs.queues`, dropping every section declared AFTER
/// `[jobs]` from the derived schema. The adaptive multi-pass walk escalates that
/// path to a map probe (empty map → visitor yields an empty config → no abort),
/// so the walk continues and enumerates the later sections. This asserts those
/// post-`jobs` sections now expose child keys and are strictly validated.
#[test]
fn post_jobs_sections_are_now_schema_covered() {
    let schema = AutumnConfig::get_schema_keys();
    // Config.rs-internal, always-on roots declared after `[jobs]`, confirmed
    // from the regenerated snapshot to expand to child keys once the walk stops
    // aborting at `jobs.queues`.
    for root in [
        "scheduler",
        "resilience",
        "seo",
        "dev",
        "compression",
        "backup",
    ] {
        assert!(
            schema.get(root).is_some_and(|k| !k.is_empty()),
            "[{root}] must have child keys after the adaptive-walker fix; empty means the walk still aborts before it (see #1890 / jobs.queues)"
        );
    }
    // `jobs` sub-structs declared after the `jobs.queues` abort point are covered too.
    assert!(
        schema.get("jobs.redis").is_some_and(|k| !k.is_empty()),
        "jobs.redis must descend now that jobs.queues no longer aborts the walk"
    );
    // `jobs.queues` itself stays present, as a leaf (dynamic queue names, no
    // fixed child keys — we intentionally do not descend into it).
    assert!(
        AutumnConfig::schema_leaf_paths().contains("jobs.queues"),
        "jobs.queues must remain in the schema as a leaf"
    );
    // End-to-end: the reviewer's example — a bogus deep key under [resilience]
    // is flagged now that the walk reaches [resilience].
    let errors = AutumnConfig::validate_toml("[resilience]\nnot_a_real_key = true\n", &schema);
    assert!(
        errors.iter().any(|(p, _)| p == "resilience.not_a_real_key"),
        "a bogus [resilience] child key must be flagged now, got: {errors:?}"
    );
}

#[test]
fn manual_schema_sections_are_registered() {
    let schema = autumn_web::config::AutumnConfig::get_schema_keys();
    // Walker-opaque untagged scalar-or-table sections must still expose their
    // table fields so strict validation descends (see MANUAL_SCHEMA_SECTIONS).
    let tz = schema
        .get("time_zone")
        .expect("time_zone must be in schema");
    assert!(
        tz.contains("identifier") && tz.contains("sources"),
        "time_zone table fields must be registered, got: {tz:?}"
    );
    // End-to-end: a typo under [time_zone] is now flagged (was silently accepted).
    let errors = autumn_web::config::AutumnConfig::validate_toml(
        "[time_zone]\nidentifer = \"UTC\"\n",
        &schema,
    );
    assert!(
        errors.iter().any(|(p, _)| p == "time_zone.identifer"),
        "a bogus [time_zone] child key must be flagged now, got: {errors:?}"
    );
}

#[test]
fn dynamic_key_sections_are_not_strict_validated() {
    // Flatten maps and HashMap<String,_> config sections have ARBITRARY valid
    // child keys the schema walker can't enumerate (e.g. OAuth2 provider names
    // like `github` under [auth.oauth2], or hosts under
    // [resilience.circuit_breaker.hosts]). They deserialize via
    // SchemaDeserializer::deserialize_map, which registers NO schema entry, so
    // `schema.get(path)` is None and validate_toml_table skips their children —
    // no false-positive "unknown key". This guards against a future change that
    // would give these a restrictive schema entry and break valid configs.
    let schema = autumn_web::config::AutumnConfig::get_schema_keys();

    // Always-present dynamic sections must have NO restrictive schema entry.
    for dynamic in ["resilience.circuit_breaker.hosts", "jobs.queues"] {
        let entry = schema.get(dynamic);
        assert!(
            entry.is_none(),
            "{dynamic} is a dynamic-key section and must NOT have a restrictive \
             schema entry (that would false-positive valid child keys); got {entry:?}",
        );
    }
    // End-to-end: valid dynamic child keys are accepted (no unknown-key error).
    let errors = autumn_web::config::AutumnConfig::validate_toml(
        "[resilience.circuit_breaker.hosts.api]\nopen_duration_secs = 5\n",
        &schema,
    );
    assert!(
        errors
            .iter()
            .all(|(p, _)| !p.starts_with("resilience.circuit_breaker.hosts")),
        "valid keys under a dynamic map section must not be flagged; got {errors:?}"
    );
}

#[cfg(feature = "oauth2")]
#[test]
fn flattened_oauth2_providers_are_not_flagged() {
    // Regression guard for the reviewer's exact example: [auth.oauth2.<name>] is
    // a valid provider via `OAuth2Config::providers` (`#[serde(flatten)]`), so it
    // must NOT be reported as an unknown key (which would hard-fail under
    // strict_config_enforce_all).
    let schema = autumn_web::config::AutumnConfig::get_schema_keys();
    let entry = schema.get("auth.oauth2");
    assert!(
        entry.is_none(),
        "auth.oauth2 is a flatten map and must have no restrictive schema entry; got {entry:?}",
    );
    let errors = autumn_web::config::AutumnConfig::validate_toml(
        "[auth.oauth2.github]\nclient_id = \"x\"\nclient_secret = \"y\"\n",
        &schema,
    );
    assert!(
        errors.iter().all(|(p, _)| !p.starts_with("auth.oauth2")),
        "flattened OAuth2 provider keys must not be flagged as unknown; got {errors:?}"
    );
}

#[cfg(feature = "http-client")]
#[test]
fn http_client_base_urls_map_is_not_flagged() {
    let schema = autumn_web::config::AutumnConfig::get_schema_keys();
    let entry = schema.get("http.client.base_urls");
    assert!(
        entry.is_none(),
        "http.client.base_urls is a HashMap and must have no restrictive schema entry; got {entry:?}",
    );
    let errors = autumn_web::config::AutumnConfig::validate_toml(
        "[http.client.base_urls]\nstripe = \"https://api.stripe.com\"\n",
        &schema,
    );
    assert!(
        errors
            .iter()
            .all(|(p, _)| !p.starts_with("http.client.base_urls")),
        "arbitrary base_urls map keys must not be flagged; got {errors:?}"
    );
}
