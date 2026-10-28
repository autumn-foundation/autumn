//! Spec for the compile-gate features (#2795).
//!
//! Each feature gates one module that most apps never use. All are default-on,
//! so a default build is unchanged. The no-DB daemon starter turns them off.

use std::collections::BTreeSet;

/// Feature name and the module it gates.
///
/// `job-tracking` is not here. Its module stays compiled, because the job
/// runtime wraps payloads in its envelope. The feature gates the status route.
const GATES: &[(&str, &str)] = &[
    ("ledger", "ledger"),
    ("experiments", "experiments"),
    ("feature-flags", "feature_flags"),
    ("idempotency", "idempotency"),
    ("custom-domains", "custom_domain"),
    ("upgrade", "upgrade"),
    ("nested-form", "nested_form"),
    ("alerts", "alerts"),
    ("data-retention", "data_retention"),
    ("version-history", "version_history"),
    ("wizard", "wizard"),
    ("money", "money"),
    ("gdpr", "gdpr"),
    ("slo", "slo"),
    ("graph", "graph"),
];

/// A feature that must switch on another one.
const IMPLIES: &[(&str, &str)] = &[
    ("experiments", "feature-flags"),
    ("ledger", "version-history"),
    ("data-retention", "gdpr"),
    ("db", "ledger"),
    ("db", "money"),
    ("db", "data-retention"),
    ("acme", "custom-domains"),
];

const ALL_FEATURES: &[&str] = &[
    "ledger",
    "experiments",
    "feature-flags",
    "idempotency",
    "custom-domains",
    "job-tracking",
    "upgrade",
    "nested-form",
    "alerts",
    "data-retention",
    "version-history",
    "wizard",
    "money",
    "gdpr",
    "slo",
    "graph",
];

fn manifest() -> toml::Table {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    std::fs::read_to_string(path)
        .expect("read Cargo.toml")
        .parse()
        .expect("parse Cargo.toml")
}

fn features(m: &toml::Table) -> &toml::Table {
    m["features"].as_table().expect("[features]")
}

fn names(v: &toml::Value) -> BTreeSet<String> {
    v.as_array()
        .expect("feature list")
        .iter()
        .map(|x| x.as_str().expect("feature entry").to_owned())
        .collect()
}

#[test]
fn every_gate_is_a_feature() {
    let m = manifest();
    for feature in ALL_FEATURES {
        assert!(
            features(&m).contains_key(*feature),
            "missing feature `{feature}`"
        );
    }
}

#[test]
fn every_gate_is_default_on() {
    let m = manifest();
    let default = names(&features(&m)["default"]);
    for feature in ALL_FEATURES {
        assert!(
            default.contains(*feature),
            "`{feature}` must be in `default`"
        );
    }
}

#[test]
fn every_gated_module_has_a_cfg() {
    let lib = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read lib.rs");
    let lines: Vec<&str> = lib.lines().collect();
    for (feature, module) in GATES {
        let decl = format!("pub mod {module};");
        let at = lines
            .iter()
            .position(|l| l.trim() == decl)
            .unwrap_or_else(|| panic!("no `{decl}` in lib.rs"));
        let gate = format!("feature = \"{feature}\"");
        let cfg = lines[..at]
            .iter()
            .rev()
            .take_while(|l| l.trim_start().starts_with("#["))
            .any(|l| l.contains("cfg(") && l.contains(&gate));
        assert!(cfg, "`{module}` lacks `#[cfg({gate})]`");
    }
}

#[test]
fn psl_is_optional_and_owned_by_custom_domains() {
    let m = manifest();
    let psl = &m["dependencies"]["psl"];
    assert_eq!(psl.get("optional").and_then(toml::Value::as_bool), Some(true));
    assert!(names(&features(&m)["custom-domains"]).contains("dep:psl"));
}

#[test]
fn dependent_features_switch_on_what_they_need() {
    let m = manifest();
    for (feature, needs) in IMPLIES {
        assert!(
            names(&features(&m)[*feature]).contains(*needs),
            "`{feature}` must enable `{needs}`"
        );
    }
}
