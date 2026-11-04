//! `autumn graph check-boundaries` end to end (#3549).
//!
//! Each test writes a small Cargo workspace, a `boundaries.toml` and a
//! committed graph into a `TempDir`, then runs the real `autumn` binary with
//! `--graph`, so no app is built. The workspace has only path dependencies,
//! so `cargo metadata` needs no network.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use autumn_web::graph::manifest::build;
use autumn_web::graph::{ModelGraphDescriptor, RouteGraphDescriptor};
use tempfile::TempDir;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create dir");
    }
    fs::write(path, contents).expect("write file");
}

const MODELS: &[ModelGraphDescriptor] = &[
    ModelGraphDescriptor {
        model: "Contact",
        model_path: "crm_core::models::Contact",
        table: "contacts",
        relations: &[],
        module_path: "crm_core::models",
        file: "crm-core/src/lib.rs",
        line: 1,
        crate_version: "0.1.0",
    },
    ModelGraphDescriptor {
        model: "Campaign",
        model_path: "crm_marketing::models::Campaign",
        table: "campaigns",
        relations: &[],
        module_path: "crm_marketing::models",
        file: "crm-marketing/src/lib.rs",
        line: 1,
        crate_version: "0.1.0",
    },
];

const fn route(handler: &'static str, path: &'static str) -> RouteGraphDescriptor {
    RouteGraphDescriptor {
        handler,
        module_path: "crm_marketing::routes",
        method: "GET",
        path,
        static_route: false,
        file: "crm-marketing/src/lib.rs",
        line: 1,
        crate_version: "0.1.0",
        signature_symbols: &[],
        body_symbols: &[],
        sql_symbols: &[],
    }
}

const ROUTES: &[RouteGraphDescriptor] = &[
    RouteGraphDescriptor {
        body_symbols: &["Campaign", "contacts"],
        ..route("report", "/report")
    },
    RouteGraphDescriptor {
        body_symbols: &["c", "campaigns", "contacts", "ct"],
        sql_symbols: &["c", "campaigns", "contacts", "ct"],
        ..route("joined", "/joined")
    },
];

fn boundaries(may_read: &str) -> String {
    format!(
        "baseline = \"boundaries-baseline.json\"\n\n\
         [boundaries.core]\nowns = [\"contacts\"]\ncrates = [\"crm-core\"]\n\n\
         [boundaries.marketing]\nowns = [\"campaigns\"]\ncrates = [\"crm-marketing\"]\n\
         may_read = [{may_read}]\n"
    )
}

/// A two-crate workspace in which `crm-marketing` depends on `crm-core`.
fn fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crm-core\", \"crm-marketing\"]\nresolver = \"2\"\n",
    );
    write(
        root,
        "crm-core/Cargo.toml",
        "[package]\nname = \"crm-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(root, "crm-core/src/lib.rs", "");
    write(
        root,
        "crm-marketing/Cargo.toml",
        "[package]\nname = \"crm-marketing\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ncrm-core = { path = \"../crm-core\" }\n",
    );
    write(root, "crm-marketing/src/lib.rs", "");
    let graph = build(&[], 0, ROUTES, MODELS, &[], &[]);
    write(root, "architecture-graph.json", &graph.to_json());
    write(root, "boundaries.toml", &boundaries(""));
    dir
}

fn check(root: &Path, extra: &[&str]) -> Output {
    Command::new(autumn_bin())
        .args([
            "graph",
            "check-boundaries",
            "--graph",
            "architecture-graph.json",
        ])
        .args(extra)
        .current_dir(root)
        .output()
        .expect("run autumn graph check-boundaries")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_cross_boundary_read_fails_and_names_both_boundaries() {
    let dir = fixture();
    let output = check(dir.path(), &[]);
    let text = stdout(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("marketing -> core: route GET /report reads table `contacts`"),
        "{text}"
    );
    // The raw-SQL JOIN is caught too.
    assert!(text.contains("GET /joined"), "{text}");
    // The crate rule: marketing depends on the crate with the core schema.
    assert!(text.contains("crm-marketing -> crm-core"), "{text}");
}

#[test]
fn may_read_makes_the_check_pass() {
    let dir = fixture();
    write(dir.path(), "boundaries.toml", &boundaries("\"contacts\""));
    let output = check(dir.path(), &[]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("No new boundary violations"));
}

#[test]
fn the_baseline_ratchet_only_shrinks() {
    let dir = fixture();
    let root = dir.path();

    // Adopt: write today's violations to the baseline.
    let output = check(root, &["--update-baseline"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let baseline = fs::read_to_string(root.join("boundaries-baseline.json")).expect("baseline");
    assert_eq!(baseline.matches("\"rule\"").count(), 3, "{baseline}");

    // Known violations pass.
    let output = check(root, &[]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("3 known violation(s)"));

    // Fix them: the stale baseline fails until it shrinks.
    write(root, "boundaries.toml", &boundaries("\"contacts\""));
    let output = check(root, &[]);
    assert!(!output.status.success());
    assert!(
        stdout(&output).contains("--update-baseline"),
        "{}",
        stdout(&output)
    );
    assert!(check(root, &["--update-baseline"]).status.success());
    let baseline = fs::read_to_string(root.join("boundaries-baseline.json")).expect("baseline");
    assert!(!baseline.contains("\"rule\""), "{baseline}");
    assert!(check(root, &[]).status.success());

    // A new violation fails against the shrunk baseline.
    write(root, "boundaries.toml", &boundaries(""));
    assert!(!check(root, &[]).status.success());
}

#[test]
fn json_output_lists_new_violations() {
    let dir = fixture();
    let output = check(dir.path(), &["--json"]);
    assert!(!output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    let new = json["new"].as_array().expect("new");
    assert_eq!(new.len(), 3, "{json}");
    assert!(
        new.iter()
            .any(|v| v["rule"] == "table" && v["table"] == "contacts")
    );
    assert!(
        new.iter()
            .any(|v| v["rule"] == "crate" && v["crate"] == "crm-marketing")
    );
}

#[test]
fn a_missing_config_fails_with_a_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = check(dir.path(), &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("boundaries.toml"));
}
