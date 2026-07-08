use std::fs;
use std::process::Command;

fn scaffold(project_name: &str) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");

    let output = Command::new(autumn_bin)
        .args(["new", project_name])
        .current_dir(temp_dir.path())
        .output()
        .expect("failed to run `autumn new`");

    assert!(
        output.status.success(),
        "autumn new failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    temp_dir
}

#[test]
fn cloud_native_scaffold_emits_container_artifacts() {
    let temp_dir = scaffold("cloudy-app");
    let project_dir = temp_dir.path().join("cloudy-app");

    assert!(project_dir.join("Dockerfile").is_file());
    assert!(project_dir.join(".dockerignore").is_file());
}

#[test]
fn cloud_native_scaffold_includes_probe_and_telemetry_examples() {
    let temp_dir = scaffold("ops-app");
    let config = fs::read_to_string(temp_dir.path().join("ops-app/autumn.toml")).unwrap();

    assert!(config.contains(r#"# live_path = "/live""#));
    assert!(config.contains(r#"# ready_path = "/ready""#));
    assert!(config.contains(r#"# startup_path = "/startup""#));
    assert!(config.contains("# [telemetry]"));
    assert!(config.contains(r"# enabled = true"));
    assert!(config.contains(r#"# otlp_endpoint = "http://otel-collector:4317""#));
}

#[test]
fn cloud_native_scaffold_dockerfile_is_production_ready() {
    let temp_dir = scaffold("container-app");
    let project_dir = temp_dir.path().join("container-app");
    let dockerfile = fs::read_to_string(project_dir.join("Dockerfile")).unwrap();
    let dockerignore = fs::read_to_string(project_dir.join(".dockerignore")).unwrap();
    let msrv = option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0");

    assert!(dockerfile.contains(&format!("FROM rust:{msrv}-bookworm AS builder")));
    assert!(!dockerfile.contains("rust:1.86"));
    assert!(dockerfile.contains("FROM debian:bookworm-slim AS runtime"));
    assert!(dockerfile.contains("curl -fsSL"));
    assert!(dockerfile.contains("target/autumn/tailwindcss"));
    assert!(dockerfile.contains("cargo build --release"));
    assert!(dockerfile.contains("COPY --from=builder /app/target/release/container-app"));
    assert!(dockerfile.contains("USER autumn"));
    assert!(dockerfile.contains(r#"CMD ["container-app"]"#));

    assert!(dockerignore.contains("/target"));
    assert!(dockerignore.contains("/.git"));
    assert!(dockerignore.contains("static/css/autumn.css"));
}

#[test]
fn ci_workflow_is_scaffolded() {
    let temp_dir = scaffold("ci-app");
    let project_dir = temp_dir.path().join("ci-app");

    assert!(
        project_dir.join(".github/workflows/ci.yml").is_file(),
        "autumn new must write .github/workflows/ci.yml"
    );
}

#[test]
fn ci_workflow_contains_expected_jobs() {
    let temp_dir = scaffold("ci-jobs-app");
    let project_dir = temp_dir.path().join("ci-jobs-app");
    let ci = fs::read_to_string(project_dir.join(".github/workflows/ci.yml")).unwrap();

    assert!(
        ci.contains("cargo fmt --all -- --check"),
        "ci.yml must run cargo fmt --check"
    );
    assert!(
        ci.contains("cargo clippy") && ci.contains("-D warnings"),
        "ci.yml must run cargo clippy -D warnings"
    );
    assert!(ci.contains("cargo build"), "ci.yml must run cargo build");
    assert!(ci.contains("cargo test"), "ci.yml must run cargo test");
}

#[test]
fn ci_workflow_pins_msrv_toolchain() {
    let temp_dir = scaffold("ci-msrv-app");
    let project_dir = temp_dir.path().join("ci-msrv-app");
    let ci = fs::read_to_string(project_dir.join(".github/workflows/ci.yml")).unwrap();
    let msrv = option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0");

    assert!(
        ci.contains(&format!("dtolnay/rust-toolchain@{msrv}")),
        "ci.yml must pin the Rust toolchain to the MSRV via dtolnay/rust-toolchain@<msrv>"
    );
    assert!(
        !ci.contains("rust-toolchain@stable"),
        "ci.yml must not use rust-toolchain@stable; it must be pinned to MSRV"
    );
}

#[test]
fn ci_workflow_provisions_postgres() {
    let temp_dir = scaffold("ci-pg-app");
    let project_dir = temp_dir.path().join("ci-pg-app");
    let ci = fs::read_to_string(project_dir.join(".github/workflows/ci.yml")).unwrap();

    assert!(
        ci.contains("postgres"),
        "ci.yml must provision a Postgres service for DB-dependent tests"
    );
    assert!(
        ci.contains("DATABASE_URL"),
        "ci.yml must set DATABASE_URL for DB-dependent tests"
    );
}

#[test]
fn ci_workflow_has_no_unsubstituted_placeholders() {
    let temp_dir = scaffold("ci-placeholder-app");
    let project_dir = temp_dir.path().join("ci-placeholder-app");
    let ci = fs::read_to_string(project_dir.join(".github/workflows/ci.yml")).unwrap();

    assert!(
        !ci.contains("{{"),
        "ci.yml must not contain unsubstituted template placeholders"
    );
    assert!(
        ci.contains("ci-placeholder-app"),
        "ci.yml must substitute the project name"
    );
}
