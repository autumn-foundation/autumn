//! End-to-end tests for `autumn slo generate` (issue #3069).
//!
//! Each test runs the real binary in a temporary project.

use std::fs;

use crate::common::run_autumn;
use tempfile::TempDir;

/// A project with a `Cargo.toml` and the given `autumn.toml`.
fn project(autumn_toml: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo_shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(dir.path().join("autumn.toml"), autumn_toml).expect("write autumn.toml");
    dir
}

const SLOS: &str = "[[slo]]\nname = \"availability\"\nobjective = 99.9\nsli = \"availability\"\n\n\
                    [[slo]]\nname = \"latency\"\nobjective = 99\nsli = \"latency\"\n\
                    route = \"/api/orders\"\nthreshold_ms = 250\n";

#[test]
fn generate_writes_six_files_and_check_accepts_them() {
    let dir = project(SLOS);
    let (stdout, stderr, code) = run_autumn(dir.path(), &["slo", "generate"], &[]);
    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    for name in [
        "prometheus-rules.yaml",
        "prometheus-rule.yaml",
        "grafana-dashboard.json",
        "argo-analysis-template.yaml",
        "flagger-metric-templates.yaml",
        "helm-values.yaml",
    ] {
        let path = dir.path().join("deploy/slo").join(name);
        assert!(
            path.is_file(),
            "{} missing; stdout:\n{stdout}",
            path.display()
        );
    }
    // The package name is the default app name, made Kubernetes-safe.
    let values = fs::read_to_string(dir.path().join("deploy/slo/helm-values.yaml")).unwrap();
    assert!(values.contains("\"demo-shop-slo\""), "{values}");

    let (stdout, stderr, code) = run_autumn(dir.path(), &["slo", "generate", "--check"], &[]);
    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("up to date"), "{stdout}");
}

#[test]
fn check_fails_when_an_slo_changes() {
    let dir = project(SLOS);
    let (_, stderr, code) = run_autumn(dir.path(), &["slo", "generate"], &[]);
    assert_eq!(code, Some(0), "{stderr}");
    fs::write(
        dir.path().join("autumn.toml"),
        SLOS.replace("objective = 99.9", "objective = 99.95"),
    )
    .unwrap();
    let (_, stderr, code) = run_autumn(dir.path(), &["slo", "generate", "--check"], &[]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("prometheus-rules.yaml"), "{stderr}");
}

#[test]
fn a_bad_slo_fails_with_an_actionable_message() {
    let dir = project(
        "[[slo]]\nname = \"latency\"\nobjective = 99\nsli = \"latency\"\nthreshold_ms = 300\n",
    );
    let (_, stderr, code) = run_autumn(dir.path(), &["slo", "generate"], &[]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("[[slo]] latency"), "{stderr}");
    assert!(stderr.contains("250, 500"), "{stderr}");
    assert!(!dir.path().join("deploy/slo").exists());
}

#[test]
fn no_slo_tables_fails() {
    let dir = project("[server]\nport = 3000\n");
    let (_, stderr, code) = run_autumn(dir.path(), &["slo", "generate"], &[]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("no [[slo]] tables"), "{stderr}");
}

#[test]
fn options_reach_the_output() {
    let dir = project(SLOS);
    let (_, stderr, code) = run_autumn(
        dir.path(),
        &[
            "slo",
            "generate",
            "--out-dir",
            "ops",
            "--app",
            "shop",
            "--selector",
            "job=\"shop\"",
            "--prometheus-url",
            "http://prom:9090",
        ],
        &[],
    );
    assert_eq!(code, Some(0), "{stderr}");
    let argo = fs::read_to_string(dir.path().join("ops/argo-analysis-template.yaml")).unwrap();
    assert!(argo.contains("name: \"shop-slo\""), "{argo}");
    assert!(argo.contains("\"http://prom:9090\""), "{argo}");
    assert!(argo.contains(",job=\"shop\""), "{argo}");
}
