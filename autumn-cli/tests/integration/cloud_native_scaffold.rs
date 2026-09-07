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
    // Codex P2: the runnable Postgres Docker helper must appear EXACTLY ONCE (in
    // the step-2 "Configure the database" section). It previously also lived in
    // the prerequisites block, so a user following the README top-to-bottom
    // started the `{crate}-pg` container, then hit the identical command in step 2
    // — which fails because the container name is already in use. The
    // prerequisites now only cross-reference step 2 instead of repeating it.
    assert_eq!(
        readme.matches("docker run -d").count(),
        1,
        "README.md must contain the runnable `docker run -d …` Postgres helper exactly once \
         (de-duplicated between prerequisites and step 2), got:\n{readme}"
    );
    // Codex P2: a freshly-started `postgres:16` container accepts connections
    // only after first-time initialization finishes, but `autumn db create`
    // connects immediately with no retry. The README must document an explicit
    // readiness wait (e.g. `pg_isready`) AFTER starting the container and BEFORE
    // `autumn db create`, so the golden path doesn't fail with a connection error.
    let pg_isready_at = readme.find("pg_isready");
    let db_create_at = readme.find("autumn db create");
    assert!(
        matches!((pg_isready_at, db_create_at), (Some(r), Some(c)) if r < c),
        "README.md must wait for Postgres readiness (e.g. `pg_isready`) before \
         `autumn db create`, got:\n{readme}"
    );
    // AC #3's pointer to `release init --target docker-compose` is still present
    // (reframed as a deployment-asset generator, not the local DB path).
    assert!(
        readme.contains("autumn release init --target docker-compose"),
        "README.md must still point at `autumn release init --target docker-compose` for \
         deployment assets, got:\n{readme}"
    );
    // Finding 1: the `libpq` prerequisite must appear BEFORE the
    // `cargo install diesel_cli … --features postgres` command, since that
    // command's `postgres` feature (and the base `cargo build`, which links the
    // `db` feature) needs the libpq client library.
    let libpq_at = readme.find("libpq");
    let diesel_at = readme.find("cargo install diesel_cli");
    assert!(
        matches!((libpq_at, diesel_at), (Some(l), Some(d)) if l < d),
        "README.md must introduce the `libpq` prerequisite before the `cargo install \
         diesel_cli … --features postgres` command that needs it, got:\n{readme}"
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
    // Finding 2: `generate scaffold` emits Diesel code that needs the `db`
    // feature the daemon scaffold disables, so following the daemon README must
    // not advertise it (it would leave the app non-compiling).
    assert!(
        !readme.contains("autumn generate scaffold"),
        "daemon README must not advertise `autumn generate scaffold` (its output needs the \
         disabled `db` feature), got:\n{readme}"
    );
    // The `migrations/` layout row references a directory a DB-free daemon
    // scaffold does not have.
    assert!(
        !readme.contains("| `migrations/`"),
        "daemon README must not list a `migrations/` layout row (no migrations dir), got:\n{readme}"
    );
    assert!(
        !readme.contains("Configure the database"),
        "daemon README must not have a `Configure the database` step, got:\n{readme}"
    );
    assert!(
        !readme.contains("libpq"),
        "daemon README must not tell users to install libpq (the db feature is off), got:\n{readme}"
    );
    // The browser-reachable local-run command must be `autumn dev` (which binds
    // TCP on 127.0.0.1:3000), matching the default README — not a socket-bound
    // daemon start. Following it must land the user on http://localhost:3000.
    assert!(
        readme.contains("autumn dev"),
        "daemon README must document `autumn dev` as the browser-reachable local run, \
         got:\n{readme}"
    );
    assert!(
        readme.contains("http://localhost:3000"),
        "daemon README must point the browser at http://localhost:3000, got:\n{readme}"
    );
    // The background daemon start must still be documented, but as the
    // production/background mode that binds a private Unix socket — never paired
    // with a bare localhost:3000 claim. It must point at `autumn serve status`
    // for the reachable socket address.
    assert!(
        readme.contains("autumn serve"),
        "daemon README must document `autumn serve`, got:\n{readme}"
    );
    assert!(
        readme.contains("--daemon"),
        "daemon README must mention the `--daemon` shape it was generated with, got:\n{readme}"
    );
    assert!(
        readme.contains("Unix domain socket") && readme.contains("autumn serve status"),
        "daemon README must document that the background daemon binds a Unix socket and is \
         reached via `autumn serve status`, not a bare localhost:3000, got:\n{readme}"
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
    // The browser-reachable local-run command must be `autumn dev` (which binds
    // TCP on 127.0.0.1:3000 and provisions the bundled cluster). `autumn serve
    // --bundled-pg` implies `--daemon`, so it binds a private Unix socket and is
    // NOT reachable at http://localhost:3000 — the README must not pair it with a
    // bare browser claim.
    assert!(
        readme.contains("autumn dev"),
        "bundled-pg README must document `autumn dev` as the browser-reachable local run, \
         got:\n{readme}"
    );
    assert!(
        readme.contains("http://localhost:3000"),
        "bundled-pg README must point the browser at http://localhost:3000, got:\n{readme}"
    );
    assert!(
        readme.contains("autumn serve --bundled-pg"),
        "bundled-pg README must document `autumn serve --bundled-pg`, got:\n{readme}"
    );
    assert!(
        readme.contains("Unix domain socket") && readme.contains("autumn serve status"),
        "bundled-pg README must document that the background daemon binds a Unix socket and \
         is reached via `autumn serve status`, not a bare localhost:3000, got:\n{readme}"
    );
    // Finding 2 (guard against over-stripping): bundled-pg keeps the `db`
    // feature and auto-applies migrations, so `generate scaffold` is still valid
    // here and must remain in the CLI reference.
    assert!(
        readme.contains("autumn generate scaffold"),
        "bundled-pg README must keep `autumn generate scaffold` (the `db` feature is on), \
         got:\n{readme}"
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
fn ci_workflow_runs_a11y_verify() {
    let temp_dir = scaffold("ci-a11y-app");
    let project_dir = temp_dir.path().join("ci-a11y-app");
    let ci = fs::read_to_string(project_dir.join(".github/workflows/ci.yml")).unwrap();

    assert!(
        ci.contains("a11y verify"),
        "ci.yml must run `autumn a11y verify`"
    );
    assert!(
        ci.contains("scripts/install.sh"),
        "ci.yml must install the autumn CLI via the install script"
    );
}

/// Issue #2495: `ci.yml`'s `a11y verify` and `routes audit` steps compile
/// and introspect the pull request's own code, the same as
/// posture-gate.yml's `manifest` job — so unlike that job's `posture`
/// sibling (which only ever reads JSON, and does probe forward for a
/// compatible release), `ci.yml` must keep installing the CLI pinned to
/// this app's `autumn-web` version and never silently reach for a CLI this
/// project's own compatibility check (`autumn doctor`) would call
/// incompatible.
#[test]
fn ci_workflow_always_installs_the_cli_pinned_to_app_version() {
    let temp_dir = scaffold("ci-pinned-cli-app");
    let project_dir = temp_dir.path().join("ci-pinned-cli-app");
    let ci = fs::read_to_string(project_dir.join(".github/workflows/ci.yml")).unwrap();

    assert!(
        ci.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        "ci.yml must install the CLI pinned to this app's autumn version: {ci}"
    );
    assert!(
        !ci.contains("trunk-dev") && !ci.contains("for bump in"),
        "ci.yml must never fall back to a CLI this project's own \
         compatibility check would call incompatible: {ci}"
    );
}

/// A raw `autumn a11y verify` / `autumn routes audit` invocation against a
/// CLI that lacks the subcommand fails with a cryptic "unknown subcommand"
/// error. Mirroring `posture-gate.yml`'s existing `routes posture --help`
/// probe (#2467), both must be checked for and fail with an actionable
/// `::error::` message before either gate actually runs.
#[test]
fn ci_workflow_probes_for_a11y_and_routes_audit_before_running_them() {
    let temp_dir = scaffold("ci-probe-app");
    let project_dir = temp_dir.path().join("ci-probe-app");
    let ci = fs::read_to_string(project_dir.join(".github/workflows/ci.yml")).unwrap();

    assert!(
        ci.contains("a11y verify --help"),
        "ci.yml must probe for `a11y verify` before running it: {ci}"
    );
    assert!(
        ci.contains("routes audit --help"),
        "ci.yml must probe for `routes audit` before running it: {ci}"
    );
    assert!(
        ci.contains("::error::"),
        "ci.yml's probe must fail with an actionable ::error:: message, not \
         a bare exit: {ci}"
    );

    let probe_pos = ci.find("a11y verify --help").expect("a11y probe present");
    let run_pos = ci
        .rfind("a11y verify .")
        .expect("a11y verify invocation present");
    assert!(
        probe_pos < run_pos,
        "the a11y probe must run before `autumn a11y verify` itself: {ci}"
    );
}

#[test]
fn ci_workflow_runs_routes_audit() {
    let temp_dir = scaffold("ci-routes-audit-app");
    let project_dir = temp_dir.path().join("ci-routes-audit-app");
    let ci = fs::read_to_string(project_dir.join(".github/workflows/ci.yml")).unwrap();

    assert!(
        ci.contains("run: autumn routes audit"),
        "ci.yml must run `autumn routes audit` as a default-on route \
         auth-coverage gate (#1604)"
    );
    // The gate must run after the CLI is installed (the a11y step's `run:`
    // block), not require a second, separate install.
    let a11y_pos = ci
        .find("- name: Accessibility (a11y) verify")
        .expect("a11y verify step present");
    let audit_pos = ci
        .find("run: autumn routes audit")
        .expect("routes audit step present");
    assert!(
        audit_pos > a11y_pos,
        "routes audit step must come after the CLI install (a11y step)"
    );
}

/// Patch a scaffolded project's `Cargo.toml` to build against this workspace's
/// `autumn-web` instead of a published crates.io version, mirroring
/// `seed_model_linking::linked_seed_binary_cargo_checks`.
fn patch_to_local_autumn_web(project: &std::path::Path) {
    use std::fmt::Write as _;

    let cargo_toml_path = project.join("Cargo.toml");
    let mut content = fs::read_to_string(&cargo_toml_path).unwrap();
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web = workspace_root.join("autumn");
    let _ = write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\n",
        autumn_web.display().to_string().replace('\\', "/")
    );
    fs::write(&cargo_toml_path, content).unwrap();
}

/// Regression guard for the Codex review finding on PR #2154: the audit gate
/// wired into scaffolded CI (`ci_workflow_runs_routes_audit`) is worthless —
/// worse, actively hostile to first-run DX — if the scaffold it gates ships
/// with unclassified starter routes. Every fresh `autumn new` app (and
/// `autumn new --api`) must pass `autumn routes audit` with no changes,
/// exactly as the generated CI step will run it.
#[test]
#[ignore = "slow: compiles a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn scaffolded_app_passes_routes_audit_gate() {
    let temp_dir = scaffold("routes-audit-gate-app");
    let project_dir = temp_dir.path().join("routes-audit-gate-app");
    patch_to_local_autumn_web(&project_dir);

    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let audit = Command::new(autumn_bin)
        .args(["routes", "audit"])
        .current_dir(&project_dir)
        .output()
        .expect("failed to run `autumn routes audit`");
    assert!(
        audit.status.success(),
        "a freshly scaffolded app must pass `autumn routes audit` unmodified \
         (every starter handler needs #[public]/#[secured]/#[authorize]):\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr),
    );
}

/// Same guarantee as [`scaffolded_app_passes_routes_audit_gate`], for the
/// `--api` JSON-first starter (`main.api.rs.tmpl`), which has its own set of
/// starter handlers.
#[test]
#[ignore = "slow: compiles a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn scaffolded_api_app_passes_routes_audit_gate() {
    let temp_dir = scaffold_with_flags("routes-audit-gate-api-app", &["--api"]);
    let project_dir = temp_dir.path().join("routes-audit-gate-api-app");
    patch_to_local_autumn_web(&project_dir);

    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let audit = Command::new(autumn_bin)
        .args(["routes", "audit"])
        .current_dir(&project_dir)
        .output()
        .expect("failed to run `autumn routes audit`");
    assert!(
        audit.status.success(),
        "a freshly scaffolded --api app must pass `autumn routes audit` unmodified:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr),
    );
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
