//! The console REPL stays out of normal builds (issue #2148).
//!
//! Rhai and its glue come in only through autumn-web's `repl` feature.
//! `autumn console --repl` turns it on for one run. Nothing else may.

use std::path::Path;

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

/// Every feature that `default` turns on, followed through other features.
fn default_closure(features: &toml::Table) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut queue = vec!["default".to_owned()];
    while let Some(name) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        queue.extend(strings(features.get(&name)));
        seen.push(name);
    }
    seen
}

#[test]
fn repl_is_not_a_default_feature_and_its_dependencies_are_optional() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let autumn = manifest(&crate_dir.join("Cargo.toml"));
    let features = autumn["features"].as_table().expect("[features]");

    let repl = strings(features.get("repl"));
    assert!(repl.contains(&"dep:rhai".to_owned()), "{repl:?}");
    assert!(repl.contains(&"dep:rustyline".to_owned()), "{repl:?}");

    let defaults = default_closure(features);
    for banned in ["repl", "dep:rhai", "dep:rustyline", "rhai", "rustyline"] {
        assert!(
            !defaults.iter().any(|f| f == banned),
            "`default` must not reach `{banned}`: {defaults:?}"
        );
    }

    let deps = autumn["dependencies"].as_table().expect("[dependencies]");
    for name in ["rhai", "rustyline"] {
        let optional = deps
            .get(name)
            .and_then(|d| d.get("optional"))
            .and_then(toml::Value::as_bool);
        assert_eq!(optional, Some(true), "`{name}` must be optional");
    }
}

#[test]
fn no_workspace_member_turns_the_repl_on() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let workspace = manifest(&root.join("Cargo.toml"));
    let members = strings(workspace["workspace"].get("members"));
    assert!(!members.is_empty());

    for member in members {
        let doc = manifest(&root.join(&member).join("Cargo.toml"));
        for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(dep) = doc.get(table).and_then(|t| t.get("autumn-web")) else {
                continue;
            };
            assert!(
                !strings(dep.get("features")).contains(&"repl".to_owned()),
                "{member}: [{table}] autumn-web must not enable `repl`"
            );
        }
        if let Some(features) = doc.get("features").and_then(toml::Value::as_table) {
            for (name, value) in features {
                assert!(
                    !strings(Some(value)).contains(&"autumn-web/repl".to_owned()),
                    "{member}: feature `{name}` must not enable `autumn-web/repl`"
                );
            }
        }
    }
}
