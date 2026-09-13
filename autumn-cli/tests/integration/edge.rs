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

/// Like [`project`], but `Cargo.toml` is replaced wholesale by `manifest`
/// afterward — for tests that need a manifest shape `project`'s fixed one
/// doesn't cover (a virtual workspace root, `autobins = false`, a custom
/// `[[bin]]` entry).
fn project_with_manifest(manifest: &str, files: &[(&str, &str)]) -> TempDir {
    let dir = project(files);
    fs::write(dir.path().join("Cargo.toml"), manifest).expect("overwrite Cargo.toml");
    dir
}

/// Run `autumn doctor --json` in `root` and return the parsed report.
///
/// Runs with an empty `PATH` (mirrors `common::run_autumn`'s
/// `psql_free_path` pattern) rather than the ambient one: none of this
/// file's tests assert on `edge_target`'s pass/fail outcome — the only check
/// here whose result depends on a real toolchain probe — so there is nothing
/// for the ambient PATH to serve, and the empty PATH keeps the run hermetic.
fn doctor_json(root: &Path) -> (serde_json::Value, Option<i32>) {
    let empty_path = tempfile::tempdir().expect("create empty PATH dir");
    let out: Output = Command::new(autumn_bin())
        .args(["doctor", "--json"])
        .current_dir(root)
        .env("PATH", empty_path.path())
        .output()
        .expect("failed to run autumn doctor");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    // Parse from the first `{` rather than assuming the whole of stdout is
    // one clean JSON blob: a stray character ahead of it would otherwise
    // fail with a confusing "expected value at line 1 column 1" instead of
    // this clearer panic.
    let json_str = stdout
        .find('{')
        .map_or(stdout.as_str(), |idx| &stdout[idx..]);
    let report = serde_json::from_str(json_str).unwrap_or_else(|e| {
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

/// A well-formed edge handler that is properly registered — no guard, no
/// missing registration, so `edge_routes` only turns non-Pass on the
/// capsule-bin check.
const REGISTERED_EDGE_APP: &str = r#"
use autumn_web::prelude::*;

#[get("/greet")]
#[edge]
pub async fn greet() -> &'static str {
    "hello"
}

pub fn edge_route_list() -> Vec<EdgeRoute> {
    edge_routes![greet]
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
fn doctor_warns_from_a_virtual_workspace_root() {
    // A bare workspace root (`[workspace]`, no `[package]`) has no sources
    // of its own — real sources live under a member crate. Both edge checks
    // must warn instead of silently reporting "no #[edge] routes".
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\"]\n",
    )
    .expect("write Cargo.toml");
    let (report, _) = doctor_json(dir.path());

    for name in ["edge_target", "edge_routes"] {
        let result = check(&report, name);
        assert_eq!(result["status"], "warn", "{result}");
        assert_eq!(
            result["detail"], "Cargo.toml is a workspace root with no [package]",
            "{result}"
        );
        assert!(
            result["hint"]
                .as_str()
                .unwrap_or_default()
                .contains("member crate directory"),
            "{result}"
        );
    }
}

#[test]
fn doctor_warns_capsule_missing_when_autobins_disabled_and_undeclared() {
    // The conventional file exists on disk, but with `autobins = false` and
    // no explicit `[[bin]]` entry, cargo never turns it into a build target.
    let dir = project_with_manifest(
        "[workspace]\n\n[package]\nname = \"edgeapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         autobins = false\n",
        &[
            ("src/main.rs", REGISTERED_EDGE_APP),
            (
                "src/bin/edge-capsule.rs",
                "fn main() { autumn_edge::serve(edgeapp::edge_route_list()); }\n",
            ),
        ],
    );
    let (report, _) = doctor_json(dir.path());

    let edge_routes = check(&report, "edge_routes");
    assert_eq!(edge_routes["status"], "warn", "{edge_routes}");
    assert!(
        edge_routes["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("edge-capsule.rs is missing"),
        "{edge_routes}"
    );
}

#[test]
fn doctor_honors_a_custom_capsule_bin_path() {
    // A custom `[[bin]] path` for `edge-capsule` must be checked directly,
    // not the conventional `src/bin/edge-capsule.rs` (which doesn't exist
    // here at all). The registration lives ONLY in `cmd/edge.rs` — outside
    // `src/` — so this also proves that file is scanned, not just probed for
    // existence (issue #2244).
    let dir = project_with_manifest(
        "[workspace]\n\n[package]\nname = \"edgeapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [[bin]]\nname = \"edge-capsule\"\npath = \"cmd/edge.rs\"\n",
        &[
            ("src/main.rs", UNREGISTERED_EDGE_APP),
            (
                "cmd/edge.rs",
                "fn main() { autumn_edge::serve(edge_routes![greet]); }\n",
            ),
        ],
    );
    let (report, _) = doctor_json(dir.path());

    let edge_routes = check(&report, "edge_routes");
    assert_eq!(
        edge_routes["status"], "pass",
        "a registration written only in a custom [[bin]] path must be seen, \
         not reported as missing: {edge_routes}"
    );
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
