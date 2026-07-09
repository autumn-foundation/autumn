use std::fs;
use std::process::Command;

fn scaffold(project_name: &str) -> tempfile::TempDir {
    scaffold_with_flags(project_name, &[])
}

fn scaffold_with_flags(project_name: &str, flags: &[&str]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");

    let mut args = vec!["new", project_name];
    args.extend_from_slice(flags);
    let output = Command::new(autumn_bin)
        .args(&args)
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
fn cloud_native_scaffold_generates_readme_golden_path() {
    let temp_dir = scaffold("readme-app");
    let project_dir = temp_dir.path().join("readme-app");

    let readme_path = project_dir.join("README.md");
    assert!(
        readme_path.is_file(),
        "autumn new must write a README.md at the project root"
    );

    let readme = fs::read_to_string(&readme_path).unwrap();
    // Golden-path commands (AC #6): the README must document the two commands
    // that take a clean checkout to a serving route.
    assert!(
        readme.contains("autumn migrate"),
        "README.md must document `autumn migrate`, got:\n{readme}"
    );
    assert!(
        readme.contains("autumn dev"),
        "README.md must document `autumn dev`, got:\n{readme}"
    );
    // The golden path must configure the database BEFORE `autumn migrate` —
    // migrate needs a resolved URL and errors ("No database URL found") on the
    // base scaffold, where the `[database]` block ships commented out. The
    // README must tell the user to enable it. Assert on stable substrings so
    // minor wording changes don't break the test.
    assert!(
        readme.contains("[database]"),
        "README.md must tell the user to enable the `[database]` block before \
         `autumn migrate`, got:\n{readme}"
    );
    assert!(
        readme.contains("autumn.toml"),
        "README.md must reference `autumn.toml` for enabling the database, got:\n{readme}"
    );
    // Finding 1: the DB-bootstrap step must NOT dead-end on a `release init`
    // file-exists error. `autumn new` already wrote Dockerfile/.dockerignore, so
    // `autumn release init --target docker-compose` aborts before emitting the
    // compose file unless --force (which would clobber the scaffold's Dockerfile).
    // The golden path bootstraps a throwaway local Postgres with `docker run`
    // instead, matching the `url` in the `[database]` block.
    assert!(
        readme.contains("docker run") && readme.contains("postgres:16"),
        "README.md DB-bootstrap must offer a working `docker run … postgres:16` one-liner \
         (not dead-end on `release init`), got:\n{readme}"
    );
    // AC #3's pointer to `release init --target docker-compose` is still present
    // (reframed as a deployment-asset generator, not the local DB path).
    assert!(
        readme.contains("autumn release init --target docker-compose"),
        "README.md must still point at `autumn release init --target docker-compose` for \
         deployment assets, got:\n{readme}"
    );
    // The project name must be substituted everywhere (AC #5) — no leftover
    // template tokens.
    assert!(
        readme.contains("readme-app"),
        "README.md must substitute the project name, got:\n{readme}"
    );
    assert!(
        !readme.contains("{{"),
        "README.md must not contain unsubstituted template placeholders, got:\n{readme}"
    );
}

#[test]
fn cloud_native_scaffold_readme_is_flag_aware_for_i18n_and_seed() {
    // AC #7: with `--with-i18n --with-seed`, the generated README must document
    // the extra steps those flags introduce (a seed section and an i18n
    // section) while still substituting every template token.
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");

    let output = Command::new(autumn_bin)
        .args(["new", "flagged-app", "--with-i18n", "--with-seed"])
        .current_dir(temp_dir.path())
        .output()
        .expect("failed to run `autumn new`");

    assert!(
        output.status.success(),
        "autumn new failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let readme_path = temp_dir.path().join("flagged-app").join("README.md");
    let readme = fs::read_to_string(&readme_path).unwrap();

    // Seed section (`--with-seed`): must point at `autumn seed`.
    assert!(
        readme.contains("autumn seed"),
        "flag-aware README must document `autumn seed`, got:\n{readme}"
    );
    // i18n section (`--with-i18n`): must reference the `t!(` macro and the
    // scaffolded locale bundle.
    assert!(
        readme.contains("t!("),
        "flag-aware README must reference the `t!(` macro, got:\n{readme}"
    );
    assert!(
        readme.contains("i18n/en.ftl"),
        "flag-aware README must reference the i18n/en.ftl bundle, got:\n{readme}"
    );
    // Appended flag sections must not reintroduce template tokens.
    assert!(
        !readme.contains("{{"),
        "flag-aware README must not contain unsubstituted placeholders, got:\n{readme}"
    );
}

#[test]
fn cloud_native_scaffold_daemon_readme_is_db_free() {
    // Finding 2: `--daemon` scaffolds a DB-free app (no `db` feature, no
    // migrations) that runs via `autumn serve`. The README must reflect that
    // shape — not the DB-first golden path (install libpq, configure Postgres,
    // `autumn migrate`).
    let temp_dir = scaffold_with_flags("daemon-readme-app", &["--daemon"]);
    let readme_path = temp_dir.path().join("daemon-readme-app").join("README.md");
    let readme = fs::read_to_string(&readme_path).unwrap();

    // The DB-first steps must be gone.
    assert!(
        !readme.contains("autumn migrate"),
        "daemon README must not tell users to run `autumn migrate` (no DB / no migrations), \
         got:\n{readme}"
    );
    assert!(
        !readme.contains("Configure the database"),
        "daemon README must not have a `Configure the database` step, got:\n{readme}"
    );
    assert!(
        !readme.contains("libpq"),
        "daemon README must not tell users to install libpq (the db feature is off), got:\n{readme}"
    );
    // The real run path must be documented.
    assert!(
        readme.contains("autumn serve"),
        "daemon README must document `autumn serve`, got:\n{readme}"
    );
    assert!(
        readme.contains("--daemon"),
        "daemon README must mention the `--daemon` shape it was generated with, got:\n{readme}"
    );
    // Project name substituted; no leftover template tokens.
    assert!(
        readme.contains("daemon-readme-app"),
        "daemon README must substitute the project name, got:\n{readme}"
    );
    assert!(
        !readme.contains("{{"),
        "daemon README must not contain unsubstituted template placeholders, got:\n{readme}"
    );
}

#[test]
fn cloud_native_scaffold_bundled_pg_readme_auto_provisions_db() {
    // Finding 2: `--bundled-pg` embeds and manages its own Postgres, so the
    // README must not tell users to configure an external `[database]` or run
    // migrations by hand — it runs via `autumn serve --bundled-pg`.
    let temp_dir = scaffold_with_flags("bundled-readme-app", &["--bundled-pg"]);
    let readme_path = temp_dir.path().join("bundled-readme-app").join("README.md");
    let readme = fs::read_to_string(&readme_path).unwrap();

    assert!(
        !readme.contains("autumn migrate"),
        "bundled-pg README must not tell users to run `autumn migrate` (auto-applied), \
         got:\n{readme}"
    );
    assert!(
        !readme.contains("Configure the database"),
        "bundled-pg README must not have a `Configure the database` step, got:\n{readme}"
    );
    assert!(
        readme.contains("autumn serve --bundled-pg"),
        "bundled-pg README must document `autumn serve --bundled-pg`, got:\n{readme}"
    );
    assert!(
        !readme.contains("{{"),
        "bundled-pg README must not contain unsubstituted template placeholders, got:\n{readme}"
    );
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
