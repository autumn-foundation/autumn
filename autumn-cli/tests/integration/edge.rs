//! Integration tests for the edge lane of `autumn build` and the `edge_target`
//! / `edge_routes` doctor checks (issue #1790).
//!
//! Each test scaffolds a throwaway project in a `TempDir` and drives the real
//! `autumn` binary against it. The fixture `.rs` files are only ever *parsed* by
//! the source scanner, so they need to be syntactically valid Rust but need not
//! compile or link — and the scaffold's `Cargo.toml` deliberately has no
//! dependencies, so nothing here may reach a real `cargo build`.
//!
//! That constraint is also the point of the build-path tests: the `--edge` and
//! `--embed` refusals are specified to happen BEFORE the native cargo build, so
//! they are driven through the empty-`PATH` runner (where a `cargo` invocation
//! could not even spawn) and additionally assert that no compile was started.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::common::run_autumn;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// A minimal project: a dependency-free `Cargo.toml` plus the given
/// `(relative path, contents)` sources.
fn project(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\n\n[package]\nname = \"edgeapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create source dir");
        }
        fs::write(path, contents).expect("write source");
    }
    dir
}

/// Run `autumn doctor --json` in `root` with the ambient `PATH` (doctor probes
/// the real toolchain) and return the parsed report.
fn doctor_json(root: &Path) -> (serde_json::Value, Option<i32>) {
    let out: Output = Command::new(autumn_bin())
        .args(["doctor", "--json"])
        .current_dir(root)
        .output()
        .expect("failed to run autumn doctor");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let report = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "`autumn doctor --json` must emit parseable JSON: {e}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (report, out.status.code())
}

/// Pull one check out of a `doctor --json` report by name.
fn check<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .expect("report carries a checks array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("doctor must emit a `{name}` check; got {report}"))
}

/// A handler marked `#[edge]` that also carries `#[secured]` — a pair the
/// `#[edge]` macro rejects, so doctor must fail on it before the build does.
const GUARDED_EDGE_APP: &str = r#"
use autumn_web::prelude::*;

#[get("/dashboard")]
#[edge]
#[secured]
pub async fn dashboard() -> &'static str {
    "private"
}

pub fn edge_route_list() -> Vec<EdgeRoute> {
    edge_routes![dashboard]
}
"#;

/// A well-formed edge handler that nothing registers.
const UNREGISTERED_EDGE_APP: &str = r#"
use autumn_web::prelude::*;

#[get("/greet")]
#[edge]
pub async fn greet() -> &'static str {
    "hello"
}
"#;

/// An ordinary app with no edge routes at all.
const PLAIN_APP: &str = r#"
use autumn_web::prelude::*;

#[get("/")]
pub async fn home() -> &'static str {
    "hello"
}
"#;

#[test]
fn doctor_fails_on_edge_route_with_auth_guard() {
    let dir = project(&[
        ("src/main.rs", GUARDED_EDGE_APP),
        (
            "src/bin/edge-capsule.rs",
            "fn main() { autumn_edge::serve(edgeapp::edge_route_list()); }\n",
        ),
    ]);
    let (report, code) = doctor_json(dir.path());

    assert_ne!(
        code,
        Some(0),
        "an #[edge] route behind #[secured] must fail doctor: {report}"
    );
    let edge_routes = check(&report, "edge_routes");
    assert_eq!(edge_routes["status"], "fail", "{edge_routes}");
    let detail = edge_routes["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("dashboard @ src/main.rs:"),
        "detail must name the handler and its location: {detail}"
    );
    assert!(detail.contains("#[secured]"), "{detail}");
    assert!(
        edge_routes["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("unauthenticated read-path"),
        "the hint must say why the pair is impossible: {edge_routes}"
    );
}

#[test]
fn doctor_warns_on_unregistered_edge_route() {
    let dir = project(&[("src/main.rs", UNREGISTERED_EDGE_APP)]);
    let (report, _) = doctor_json(dir.path());

    let edge_routes = check(&report, "edge_routes");
    assert_eq!(edge_routes["status"], "warn", "{edge_routes}");
    assert!(
        edge_routes["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("greet @ src/main.rs:"),
        "{edge_routes}"
    );
    assert!(
        edge_routes["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("edge_routes![]"),
        "{edge_routes}"
    );
}

#[test]
fn doctor_edge_checks_pass_without_edge_routes() {
    // Deliberately asserts on the two checks rather than on the exit code: this
    // must hold on a runner that has no `wasm32-wasip1` target installed, which
    // is exactly what "no edge routes ⇒ no wasm requirement" means.
    let dir = project(&[("src/main.rs", PLAIN_APP)]);
    let (report, _) = doctor_json(dir.path());

    for name in ["edge_target", "edge_routes"] {
        let result = check(&report, name);
        assert_eq!(result["status"], "pass", "{result}");
        assert_eq!(result["detail"], "no #[edge] routes", "{result}");
    }
}

#[test]
fn build_edge_flag_without_edge_routes_fails_before_compiling() {
    let dir = project(&[("src/main.rs", PLAIN_APP)]);
    // Empty PATH: a `cargo` invocation could not even spawn here, so reaching
    // the native build would surface as a different failure entirely.
    let (stdout, stderr, code) = run_autumn(dir.path(), &["build", "--edge"], &[]);
    let combined = format!("{stdout}{stderr}");

    assert_ne!(code, Some(0), "{combined}");
    assert!(
        combined.contains("no #[edge] routes found"),
        "the error must say what is missing: {combined}"
    );
    assert!(
        combined.contains("edge_routes![]"),
        "the error must say how to fix it: {combined}"
    );
    assert!(
        !combined.contains("Compiling"),
        "the flag check must run before the native build: {combined}"
    );
}

#[test]
fn build_embed_refuses_edge_routes_before_compiling() {
    let dir = project(&[
        ("src/main.rs", UNREGISTERED_EDGE_APP),
        (
            "src/bin/edge-capsule.rs",
            "fn main() { autumn_edge::serve(edge_routes![greet]); }\n",
        ),
    ]);
    let (stdout, stderr, code) = run_autumn(dir.path(), &["build", "--embed"], &[]);
    let combined = format!("{stdout}{stderr}");

    assert_ne!(code, Some(0), "{combined}");
    assert!(
        combined.contains("edge capsule build is not yet supported with --embed"),
        "{combined}"
    );
    assert!(combined.contains("#1790"), "{combined}");
    assert!(
        !combined.contains("Compiling"),
        "the refusal must run before the native build: {combined}"
    );
}
