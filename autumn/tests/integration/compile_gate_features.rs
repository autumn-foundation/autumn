//! Spec for the compile-gate features (#2795).
//!
//! Each feature gates one module, or one feature of a dependency (`axum-ws`).
//! All are default-on, so a default build is unchanged. The no-DB daemon
//! starter turns them off, except `error-reporting`, the panic-catch layer.

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
    ("push", "push"),
    ("cluster", "cluster"),
    ("error-reporting", "reporting"),
    ("plugin-conformance", "plugin_conformance"),
    ("runtime-config", "runtime_config"),
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
    ("oauth2", "jwt"),
    ("reporting", "error-reporting"),
    ("ws", "axum-ws"),
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
    "jwt",
    "push",
    "cluster",
    "error-reporting",
    "plugin-conformance",
    "runtime-config",
    "axum-ws",
];

fn manifest() -> toml::Table {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    std::fs::read_to_string(path)
        .expect("read Cargo.toml")
        .parse()
        .expect("parse Cargo.toml")
}

/// A file of the repository, by its path from the repository root.
fn repo_file(path: &str) -> String {
    let full = format!("{}/../{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {full}: {e}"))
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
        assert!(
            module_has_cfg(&lines, module, feature),
            "`{module}` lacks `#[cfg(feature = \"{feature}\")]`"
        );
    }
}

/// True when `pub mod <module>;` in lib.rs has a `cfg` that names `feature`.
fn module_has_cfg(lines: &[&str], module: &str, feature: &str) -> bool {
    let decl = format!("pub mod {module};");
    let at = lines
        .iter()
        .position(|l| l.trim() == decl)
        .unwrap_or_else(|| panic!("no `{decl}` in lib.rs"));
    let gate = format!("feature = \"{feature}\"");
    lines[..at]
        .iter()
        .rev()
        .take_while(|l| l.trim_start().starts_with("#["))
        .any(|l| (l.contains("cfg(") || l.contains("cfg_attr(not(")) && l.contains(&gate))
}

/// `error-reporting` is the panic-catch layer. `reporting` adds failure
/// capture, the `capsule` module. An app that names `reporting` keeps both.
#[test]
fn failure_capture_stays_with_the_reporting_feature() {
    let lib = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read lib.rs");
    let lines: Vec<&str> = lib.lines().collect();
    assert!(module_has_cfg(&lines, "capsule", "reporting"));
    assert!(!module_has_cfg(&lines, "capsule", "error-reporting"));
    let default = names(&features(&manifest())["default"]);
    assert!(
        default.contains("reporting"),
        "`reporting` stays default-on"
    );
}

/// `[failure_capture]` parses in every build. Boot refuses `enabled = true`
/// in a build without `reporting`.
#[test]
fn failure_capture_config_parses_in_every_build() {
    assert!(!field_is_gated(
        "pub failure_capture: FailureCaptureConfig,"
    ));
}

/// True when a `cfg` attribute sits on this `AutumnConfig` field.
fn field_is_gated(field: &str) -> bool {
    let config = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/config.rs"))
        .expect("read config.rs");
    let lines: Vec<&str> = config.lines().collect();
    let decl = lines
        .iter()
        .position(|l| l.trim() == field)
        .unwrap_or_else(|| panic!("no `{field}` in config.rs"));
    lines[..decl]
        .iter()
        .rev()
        .take_while(|l| l.trim_start().starts_with("#[") || l.trim_start().starts_with("///"))
        .any(|l| l.contains("cfg("))
}

/// Each dependency that always compiles is named in a line of code. A crate
/// that no code names costs build time in every app.
#[test]
fn every_always_on_dependency_is_named_in_the_source() {
    let mut source = String::new();
    read_rust_sources(
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")),
        &mut source,
    );
    // A comment that mentions a crate is not a use of it.
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(|line| [line, "\n"])
        .collect();
    let m = manifest();
    let deps = m["dependencies"].as_table().expect("[dependencies]");
    let mut unused = Vec::new();
    for (name, spec) in deps {
        let optional = spec.get("optional").and_then(toml::Value::as_bool) == Some(true);
        if optional {
            continue;
        }
        let ident = name.replace('-', "_");
        let named = ["::", "!", ";", " as "]
            .iter()
            .any(|suffix| names_crate(&code, &format!("{ident}{suffix}")));
        if !named {
            unused.push(name.as_str());
        }
    }
    assert!(
        unused.is_empty(),
        "no line of code under src/ names these dependencies: {unused:?}. \
         Remove each one, or make it optional behind the feature that uses it."
    );
}

/// True when `use_site` starts an identifier somewhere in `code`, so `url::`
/// does not match inside `database_url::`.
fn names_crate(code: &str, use_site: &str) -> bool {
    code.match_indices(use_site).any(|(at, _)| {
        !code[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

fn read_rust_sources(dir: &std::path::Path, out: &mut String) {
    for entry in std::fs::read_dir(dir).expect("read source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            read_rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push_str(&std::fs::read_to_string(&path).expect("read source"));
            out.push('\n');
        }
    }
}

#[test]
fn psl_is_optional_and_owned_by_custom_domains() {
    let m = manifest();
    let psl = &m["dependencies"]["psl"];
    assert_eq!(
        psl.get("optional").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert!(names(&features(&m)["custom-domains"]).contains("dep:psl"));
}

/// `jsonwebtoken` and `p256` compile only when their feature is on. Between
/// them they bring in `rsa` 0.9 (RUSTSEC-2023-0071) and `crypto-bigint` 0.5
/// (RUSTSEC-2026-0387), which an app that uses neither should not compile.
#[test]
fn jwt_and_push_crates_are_optional_and_owned_by_their_feature() {
    let m = manifest();
    for (dep, feature) in [("jsonwebtoken", "jwt"), ("p256", "push")] {
        assert_eq!(
            m["dependencies"][dep]
                .get("optional")
                .and_then(toml::Value::as_bool),
            Some(true),
            "`{dep}` must be optional"
        );
        let owner = format!("dep:{dep}");
        assert!(
            names(&features(&m)[feature]).contains(&owner),
            "`{feature}` must enable `{owner}`"
        );
        for (name, list) in features(&m) {
            if name != feature {
                assert!(
                    !names(list).contains(&owner),
                    "only `{feature}` may enable `{owner}`, not `{name}`"
                );
            }
        }
    }
}

/// The cluster runtime is gated. Its `[cluster]` section is not: an app
/// without the feature still parses the section, so config stays portable.
#[test]
fn cluster_config_parses_in_every_build() {
    assert!(
        !field_is_gated("pub cluster: ClusterConfig,"),
        "`[cluster]` must parse with the feature off"
    );
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

/// axum's WebSocket code and the tungstenite stack compile only with
/// `axum-ws`. The workspace entry must not turn it on for every build.
#[test]
fn axum_ws_is_owned_by_its_feature() {
    let m = manifest();
    assert!(names(&features(&m)["axum-ws"]).contains("axum/ws"));
    for (name, list) in features(&m) {
        if name != "axum-ws" {
            assert!(
                !names(list).contains("axum/ws"),
                "only `axum-ws` may enable `axum/ws`, not `{name}`"
            );
        }
    }
    let workspace: toml::Table = repo_file("Cargo.toml").parse().expect("parse Cargo.toml");
    let axum = &workspace["workspace"]["dependencies"]["axum"];
    assert!(
        !names(&axum["features"]).contains("ws"),
        "the workspace `axum` entry must not name `ws`"
    );
}

/// The CLI builds `autumn-web` with default features off, and
/// `autumn plugin check` uses the conformance module.
#[test]
fn the_cli_names_the_plugin_conformance_feature() {
    let cli: toml::Table = repo_file("autumn-cli/Cargo.toml")
        .parse()
        .expect("parse autumn-cli/Cargo.toml");
    let autumn_web = &cli["dependencies"]["autumn-web"];
    assert_eq!(
        autumn_web
            .get("default-features")
            .and_then(toml::Value::as_bool),
        Some(false),
        "this spec is for a CLI that turns default features off"
    );
    assert!(names(&autumn_web["features"]).contains("plugin-conformance"));
}

/// The docs builds turn default features off and name their own lists. A
/// gate that a list does not name removes its module from the docs, and the
/// links to that module break.
#[test]
fn the_docs_feature_lists_name_the_new_gates() {
    let m = manifest();
    let docs_rs = names(&m["package"]["metadata"]["docs"]["rs"]["features"]);
    let script = repo_file("scripts/check-docs.sh");
    let line = script
        .lines()
        .find(|l| l.starts_with("AUTUMN_WEB_DOCS_FEATURES="))
        .expect("the docs feature list in scripts/check-docs.sh");
    let in_script: BTreeSet<&str> = line
        .trim_start_matches("AUTUMN_WEB_DOCS_FEATURES=")
        .trim_matches('"')
        .split(',')
        .collect();
    for feature in ["plugin-conformance", "runtime-config", "axum-ws"] {
        assert!(
            docs_rs.contains(feature),
            "the docs.rs list must name `{feature}`"
        );
        assert!(
            in_script.contains(feature),
            "scripts/check-docs.sh must name `{feature}`"
        );
    }
}

/// The semver gate compares the public API with the last release. A module
/// that the release had with no feature looks removed, unless the gate turns
/// the new feature on for the current build.
#[test]
fn the_semver_gate_names_the_new_module_gates() {
    let script = repo_file("scripts/check-semver.sh");
    let line = script
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("autumn_web_semver_current_only="))
        .expect("the current-only feature list in scripts/check-semver.sh");
    let listed: BTreeSet<&str> = line
        .trim_start_matches("autumn_web_semver_current_only=")
        .trim_matches('"')
        .split(',')
        .collect();
    for feature in ["plugin-conformance", "runtime-config"] {
        assert!(
            listed.contains(feature),
            "scripts/check-semver.sh must name `{feature}`"
        );
    }
}
