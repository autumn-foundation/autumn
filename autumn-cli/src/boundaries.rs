//! `autumn graph check-boundaries`: table-ownership boundaries (#3549).
//!
//! This module reads the files: `boundaries.toml`, the architecture graph,
//! the baseline and `cargo metadata`. The rules are in
//! `autumn_web::graph::boundaries`. `autumn doctor` uses the same loaders.
//!
//! See `docs/guide/table-ownership.md`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use autumn_web::graph::boundaries::{self, Baseline, BoundaryConfig, CrateGraph, Ratchet};
use autumn_web::graph::manifest::ArchitectureGraph;
use serde_json::Value;

use crate::graph::BuildTarget;

/// What `cargo metadata` gives the check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CargoFacts {
    /// The normal dependencies of every crate.
    pub crates: CrateGraph,
    /// Crates that declare the tables they own, with those tables.
    pub plugins: Vec<(String, Vec<String>)>,
}

/// Read the crate graph and the plugin-owned tables from `cargo metadata`.
///
/// A plugin declares its tables in its `Cargo.toml`:
///
/// ```toml
/// [package.metadata.autumn.boundary]
/// owns = ["admin_audit_log"]
/// ```
///
/// Only normal dependencies go in the graph. A dev or build dependency does
/// not ship, so it cannot reach a table at run time.
///
/// # Errors
///
/// Returns a message when the JSON has no `packages` or no `resolve`.
pub fn parse_cargo_metadata(metadata: &Value) -> Result<CargoFacts, String> {
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("cargo metadata has no `packages`")?;
    let mut facts = CargoFacts::default();
    // A package that resolves at two versions gets the key `name@version`,
    // so the dependencies of one version are not read as the other's.
    let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for name in packages
        .iter()
        .filter_map(|p| p.get("name").and_then(Value::as_str))
    {
        *seen.entry(name).or_default() += 1;
    }
    let mut names = std::collections::BTreeMap::new();
    for package in packages {
        let (Some(id), Some(name)) = (
            package.get("id").and_then(Value::as_str),
            package.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        let key = match package.get("version").and_then(Value::as_str) {
            Some(version) if seen.get(name).copied().unwrap_or(0) > 1 => {
                format!("{name}@{version}")
            }
            _ => name.to_owned(),
        };
        facts.crates.deps.entry(key.clone()).or_default();
        if let Some(lib) = library_name(package)
            && lib != name.replace('-', "_")
        {
            facts.crates.libs.insert(key.clone(), lib);
        }
        names.insert(id.to_owned(), key);
        let owns = package
            .pointer("/metadata/autumn/boundary/owns")
            .and_then(Value::as_array);
        if let Some(owns) = owns {
            let tables: Vec<String> = owns
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            if !tables.is_empty() && !facts.plugins.iter().any(|(n, _)| n == name) {
                facts.plugins.push((name.to_owned(), tables));
            }
        }
    }
    let nodes = metadata
        .pointer("/resolve/nodes")
        .and_then(Value::as_array)
        .ok_or("cargo metadata has no `resolve` (do not pass --no-deps)")?;
    for node in nodes {
        let Some(from) = node
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| names.get(id))
        else {
            continue;
        };
        for dep in node
            .get("deps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(to) = dep
                .get("pkg")
                .and_then(Value::as_str)
                .and_then(|id| names.get(id))
            else {
                continue;
            };
            if is_normal_dependency(dep) {
                facts.crates.add(from, to);
            }
        }
    }
    Ok(facts)
}

/// The library target name of a package, if it has one.
fn library_name(package: &Value) -> Option<String> {
    package
        .get("targets")
        .and_then(Value::as_array)?
        .iter()
        .find(|t| {
            t.get("kind")
                .and_then(Value::as_array)
                .is_some_and(|kinds| {
                    kinds
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|k| k.ends_with("lib") || k == "proc-macro")
                })
        })
        .and_then(|t| t.get("name").and_then(Value::as_str))
        .map(|n| n.replace('-', "_"))
}

/// Whether one `resolve` dependency is a normal one.
///
/// Cargo writes `kind: null` for a normal dependency. An old Cargo writes no
/// `dep_kinds`; then the dependency counts as normal, so nothing is missed.
fn is_normal_dependency(dep: &Value) -> bool {
    dep.get("dep_kinds")
        .and_then(Value::as_array)
        .is_none_or(|kinds| {
            kinds
                .iter()
                .any(|k| k.get("kind").is_none_or(Value::is_null))
        })
}

/// How long `autumn doctor` waits for `cargo metadata`.
///
/// Cargo can wait on its package-cache lock while another build runs. Doctor
/// reports no verdict then; it does not hang.
pub const DOCTOR_BUDGET: Duration = Duration::from_secs(30);

/// Run `cargo metadata` in `dir` and read it.
///
/// `args` are feature flags, so the crate graph matches the built app.
/// `offline` adds `--offline`, so no index is fetched. With a `budget`, the
/// child and its children are stopped when it runs longer. The child may not
/// install a toolchain.
///
/// # Errors
///
/// Returns a message when Cargo fails, runs too long, or prints JSON this
/// cannot read.
pub fn cargo_facts(
    dir: &Path,
    args: &[String],
    offline: bool,
    budget: Option<Duration>,
) -> Result<CargoFacts, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .args(["metadata", "--format-version=1"])
        .args(args)
        .current_dir(dir);
    if offline {
        command.arg("--offline");
    }
    crate::deps::no_toolchain_installs(&mut command);
    let output = crate::deps::run_bounded(&mut command, budget).map_err(|e| {
        if e.kind() == std::io::ErrorKind::Interrupted {
            "cargo metadata did not finish in time".to_owned()
        } else {
            format!("cannot run cargo metadata: {e}")
        }
    })?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("cargo metadata printed bad JSON: {e}"))?;
    parse_cargo_metadata(&json)
}

/// Read and parse `boundaries.toml`.
///
/// # Errors
///
/// Returns a message when the file cannot be read or is not valid.
pub fn load_config(path: &Path) -> Result<BoundaryConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    BoundaryConfig::parse(&text).map_err(|e| e.to_string())
}

/// A path from `boundaries.toml`, relative to the directory of that file.
#[must_use]
pub fn relative_to_config(config: &Path, path: &str) -> PathBuf {
    config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from(path), |dir| dir.join(path))
}

/// Read a committed architecture graph.
///
/// # Errors
///
/// Returns a message when the file cannot be read or is not a graph.
pub fn load_graph(path: &Path) -> Result<ArchitectureGraph, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("{} is not an architecture graph: {e}", path.display()))
}

/// Read a baseline. A file that does not exist is no baseline.
///
/// # Errors
///
/// Returns a message when the file exists but cannot be read or parsed.
pub fn load_baseline(path: &Path) -> Result<Option<Baseline>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Baseline::parse(&text)
            .map(Some)
            .map_err(|e| format!("{} is not a boundary baseline: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Run both rules and apply the baseline.
///
/// The plugins in `facts` join the config first.
///
/// # Errors
///
/// Returns a message when one table, node or crate matches two boundaries
/// equally.
pub fn evaluate(
    mut config: BoundaryConfig,
    graph: Option<&ArchitectureGraph>,
    facts: Option<&CargoFacts>,
    baseline: Option<&Baseline>,
) -> Result<(Ratchet, Vec<boundaries::Violation>), String> {
    if let Some(facts) = facts {
        for (krate, owns) in &facts.plugins {
            config.add_plugin(krate, owns);
        }
    }
    let report =
        boundaries::check(&config, graph, facts.map(|f| &f.crates)).map_err(|e| e.to_string())?;
    let all = report.violations.clone();
    Ok((boundaries::ratchet(report, baseline), all))
}

/// Options for `autumn graph check-boundaries`.
pub struct CheckBoundariesOptions<'a> {
    /// The `boundaries.toml` path.
    pub config: &'a str,
    /// A committed graph to check, instead of building the app.
    pub graph: Option<&'a str>,
    /// The baseline path. Overrides `baseline` in the config.
    pub baseline: Option<&'a str>,
    /// Write the current violations to the baseline, and pass.
    pub update_baseline: bool,
    /// Print the result as JSON.
    pub json: bool,
    /// How to build the app when `graph` is `None`.
    pub build: BuildTarget<'a>,
}

/// Print a message and exit 1.
fn fail(message: &str) -> ! {
    eprintln!("\u{2717} {message}");
    std::process::exit(1);
}

/// Run `autumn graph check-boundaries`.
///
/// It builds the app (or reads `--graph`) and runs `cargo metadata` in the
/// current directory, as the build does.
pub fn run(opts: &CheckBoundariesOptions<'_>) {
    eprintln!("\u{1F342} autumn graph check-boundaries\n");
    let config_path = Path::new(opts.config);
    let config = load_config(config_path).unwrap_or_else(|e| fail(&e));
    let baseline_path = opts.baseline.map(PathBuf::from).or_else(|| {
        config
            .baseline
            .as_deref()
            .map(|p| relative_to_config(config_path, p))
    });
    if opts.update_baseline && baseline_path.is_none() {
        fail("--update-baseline needs --baseline PATH or `baseline` in boundaries.toml");
    }
    let baseline = match (&baseline_path, opts.update_baseline) {
        (Some(path), false) => load_baseline(path).unwrap_or_else(|e| fail(&e)),
        _ => None,
    };
    let graph = opts.graph.map_or_else(
        || crate::graph::dump(&opts.build),
        |path| load_graph(Path::new(path)).unwrap_or_else(|e| fail(&e)),
    );
    let facts = cargo_facts(Path::new("."), &opts.build.features.to_args(), false, None)
        .unwrap_or_else(|e| fail(&format!("crate rule: {e}")));
    let (result, all) = evaluate(config, Some(&graph), Some(&facts), baseline.as_ref())
        .unwrap_or_else(|e| fail(&e));

    if let (true, Some(path)) = (opts.update_baseline, &baseline_path) {
        // The old file may hold a merge conflict. It only gives the count.
        let old = load_baseline(path)
            .ok()
            .flatten()
            .map_or(0, |b| b.violations.len());
        let next = Baseline::from_violations(&all);
        write_baseline(path, &next).unwrap_or_else(|e| fail(&e));
        eprintln!(
            "\u{2713} Wrote {} violation(s) to {} (was {old}).",
            next.violations.len(),
            path.display()
        );
        return;
    }

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result)
                .unwrap_or_else(|e| fail(&format!("cannot write the JSON report: {e}")))
        );
    } else {
        print!("{}", result.render());
    }
    if !result.passes() {
        std::process::exit(1);
    }
}

/// Write a baseline, creating parent directories as needed.
fn write_baseline(path: &Path, baseline: &Baseline) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, baseline.to_json())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> Value {
        serde_json::json!({
            "packages": [
                {"id": "m 0.1.0", "name": "crm-marketing", "metadata": null},
                {"id": "c 0.1.0", "name": "crm-core", "metadata": null},
                {"id": "t 0.1.0", "name": "test-kit", "metadata": null},
                {"id": "p 0.1.0", "name": "admin-plugin",
                 "metadata": {"autumn": {"boundary": {"owns": ["admin_log"]}}}}
            ],
            "resolve": {"nodes": [
                {"id": "m 0.1.0", "deps": [
                    {"pkg": "c 0.1.0", "dep_kinds": [{"kind": null, "target": null}]},
                    {"pkg": "t 0.1.0", "dep_kinds": [{"kind": "dev", "target": null}]}
                ]},
                {"id": "t 0.1.0", "deps": [
                    {"pkg": "c 0.1.0"}
                ]}
            ]}
        })
    }

    #[test]
    fn normal_dependencies_are_read_and_dev_dependencies_are_not() {
        let facts = parse_cargo_metadata(&metadata()).expect("valid");
        let marketing = &facts.crates.deps["crm-marketing"];
        assert!(marketing.contains("crm-core"));
        assert!(!marketing.contains("test-kit"));
        // No `dep_kinds` counts as normal.
        assert!(facts.crates.deps["test-kit"].contains("crm-core"));
        // A crate with no dependencies is still known.
        assert!(facts.crates.deps.contains_key("admin-plugin"));
    }

    #[test]
    fn a_plugin_declares_its_tables_in_package_metadata() {
        let facts = parse_cargo_metadata(&metadata()).expect("valid");
        assert_eq!(
            facts.plugins,
            [("admin-plugin".to_owned(), vec!["admin_log".to_owned()])]
        );
    }

    #[test]
    fn metadata_without_resolve_is_an_error() {
        let err = parse_cargo_metadata(&serde_json::json!({"packages": []}))
            .expect_err("--no-deps output has no resolve");
        assert!(err.contains("resolve"), "{err}");
    }

    #[test]
    fn paths_in_the_config_are_relative_to_its_directory() {
        assert_eq!(
            relative_to_config(Path::new("app/boundaries.toml"), "b.json"),
            PathBuf::from("app/b.json")
        );
        assert_eq!(
            relative_to_config(Path::new("boundaries.toml"), "b.json"),
            PathBuf::from("b.json")
        );
    }

    #[test]
    fn a_missing_baseline_file_is_no_baseline() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(load_baseline(&dir.path().join("none.json")), Ok(None));
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").expect("write");
        assert!(load_baseline(&bad).is_err());
    }

    #[test]
    fn a_read_of_a_plugin_table_is_a_violation() {
        use autumn_web::graph::manifest::build;
        use autumn_web::graph::{ModelGraphDescriptor, RouteGraphDescriptor};
        const MODELS: &[ModelGraphDescriptor] = &[ModelGraphDescriptor {
            model: "AdminLog",
            model_path: "admin_plugin::AdminLog",
            table: "admin_log",
            relations: &[],
            module_path: "admin_plugin::models",
            file: "src/lib.rs",
            line: 1,
            crate_version: "0.1.0",
        }];
        const ROUTES: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            handler: "audit",
            module_path: "app::routes",
            method: "GET",
            path: "/audit",
            static_route: false,
            file: "src/main.rs",
            line: 1,
            crate_version: "0.1.0",
            signature_symbols: &[],
            body_symbols: &["admin_log"],
            sql_symbols: &[],
        }];
        let graph = build(&[], 0, ROUTES, MODELS, &[], &[]);
        let config =
            BoundaryConfig::parse("[boundaries.app]\nmodules = [\"app\"]\n").expect("valid");
        let facts = parse_cargo_metadata(&metadata()).expect("valid");
        let (result, _) = evaluate(config, Some(&graph), Some(&facts), None).expect("runs");
        let text = result.render();
        assert!(!result.passes(), "{text}");
        assert!(text.contains("app -> admin-plugin"), "{text}");
    }

    #[test]
    fn two_versions_of_one_package_keep_their_own_dependencies() {
        let json = serde_json::json!({
            "packages": [
                {"id": "m", "name": "marketing", "version": "0.1.0"},
                {"id": "h1", "name": "helper", "version": "1.0.0"},
                {"id": "h2", "name": "helper", "version": "2.0.0"},
                {"id": "o", "name": "other", "version": "0.1.0"},
                {"id": "s", "name": "core-schema", "version": "0.1.0"}
            ],
            "resolve": {"nodes": [
                {"id": "m", "deps": [{"pkg": "h1"}]},
                {"id": "o", "deps": [{"pkg": "h2"}]},
                {"id": "h2", "deps": [{"pkg": "s"}]}
            ]}
        });
        let facts = parse_cargo_metadata(&json).expect("valid");
        let deps = &facts.crates.deps;
        assert_eq!(
            deps["marketing"].iter().collect::<Vec<_>>(),
            ["helper@1.0.0"]
        );
        assert!(deps["helper@1.0.0"].is_empty());
        assert!(deps["helper@2.0.0"].contains("core-schema"));
    }

    #[test]
    fn a_build_dependency_is_not_in_the_crate_graph() {
        let json = serde_json::json!({
            "packages": [
                {"id": "a", "name": "app"},
                {"id": "b", "name": "codegen"}
            ],
            "resolve": {"nodes": [
                {"id": "a", "deps": [
                    {"pkg": "b", "dep_kinds": [{"kind": "build", "target": null}]}
                ]}
            ]}
        });
        let facts = parse_cargo_metadata(&json).expect("valid");
        assert!(facts.crates.deps["app"].is_empty());
    }

    #[test]
    fn a_library_name_that_differs_is_recorded() {
        let json = serde_json::json!({
            "packages": [
                {"id": "a", "name": "crm-core-pkg",
                 "targets": [{"kind": ["lib"], "name": "crm_core"}]},
                {"id": "b", "name": "crm-web",
                 "targets": [{"kind": ["lib"], "name": "crm_web"}]}
            ],
            "resolve": {"nodes": []}
        });
        let facts = parse_cargo_metadata(&json).expect("valid");
        assert_eq!(
            facts.crates.libs.get("crm-core-pkg").map(String::as_str),
            Some("crm_core")
        );
        assert!(!facts.crates.libs.contains_key("crm-web"));
    }
}
