//! Integration tests for `autumn db pull` (database → models introspection).
//!
//! These require Docker (via testcontainers) and are marked `#[ignore]`, so
//! they only run when explicitly requested:
//!
//!   `cargo test -p autumn-cli --test db_pull -- --ignored --nocapture`
//!
//! The happy-path test proves the headline acceptance criterion: an existing
//! schema is re-derived into Autumn `#[model]`s + `schema.rs` and the generated
//! app passes `cargo check` with zero hand-editing.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const SECRET_PW: &str = "s3cr3t_pw_do_not_leak";

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String, Option<i32>) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .output()
        .expect("failed to run autumn");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

fn run_autumn_ok(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args, envs);
    assert_eq!(
        code,
        Some(0),
        "autumn {args:?} failed (exit={code:?})\nstdout: {stdout}\nstderr: {stderr}",
    );
    (stdout, stderr)
}

fn run_autumn_fail(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args, envs);
    assert_ne!(
        code,
        Some(0),
        "autumn {args:?} should have failed but exited 0\nstdout: {stdout}\nstderr: {stderr}",
    );
    (stdout, stderr)
}

/// `autumn new <name>` in a fresh tempdir, returning that tempdir + project root.
fn fresh_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name], &[]);
    let project = tmp.path().join(name);
    (tmp, project)
}

/// Point the generated project's `autumn-web` dependency at this checkout so
/// `cargo check` builds against local source rather than crates.io.
fn patch_generated_cargo_toml(project_dir: &Path) {
    let cargo_toml_path = project_dir.join("Cargo.toml");
    let mut content = std::fs::read_to_string(&cargo_toml_path).unwrap();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web = workspace_root.join("autumn");
    write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\n",
        autumn_web.display().to_string().replace('\\', "/")
    )
    .unwrap();
    std::fs::write(&cargo_toml_path, content).unwrap();
}

fn write_migration(project: &Path, name: &str, up_sql: &str, down_sql: &str) {
    let dir = project.join("migrations").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("up.sql"), up_sql).unwrap();
    std::fs::write(dir.join("down.sql"), down_sql).unwrap();
}

async fn start_postgres() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
    u16,
) {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .with_password(SECRET_PW)
        .start()
        .await
        .expect("failed to start Postgres testcontainer — is Docker running?");
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    (container, host, port)
}

/// Run `cargo check` in the generated project, returning success + combined output.
fn cargo_check(project: &Path) -> (bool, String) {
    let output = Command::new(env!("CARGO"))
        .args(["check", "--quiet"])
        .current_dir(project)
        .output()
        .expect("failed to run cargo check");
    let combined = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (output.status.success(), combined)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) + cargo check; run with -- --ignored"]
async fn db_pull_regenerates_models_from_existing_schema_and_compiles() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_app");
    patch_generated_cargo_toml(&project);

    // 1. Greenfield: generate a model covering a spread of types, then migrate
    //    so the table physically exists in the database.
    run_autumn_ok(
        &project,
        &[
            "generate",
            "model",
            "Post",
            "title:String",
            "body:Text",
            "views:i32",
            "score:f64",
            "token:Uuid",
            "published:bool",
            "summary:Option<String>",
        ],
        &envs,
    );
    run_autumn_ok(&project, &["migrate"], &envs);

    // 2. Simulate brownfield adoption: throw away the Rust artifacts, keeping
    //    only the live database table.
    std::fs::remove_dir_all(project.join("src/models")).unwrap();
    std::fs::remove_file(project.join("src/schema.rs")).unwrap();
    let migrations_before = std::fs::read_dir(project.join("migrations"))
        .unwrap()
        .count();

    // 3. Pull the schema back into Autumn artifacts, including repositories.
    //    Unscoped: Autumn's own framework tables (created by `autumn migrate`)
    //    are skipped by default, so only the user table `posts` is pulled.
    let (stdout, _) = run_autumn_ok(&project, &["db", "pull", "--with-repository"], &envs);
    assert!(
        stdout.contains("post.rs"),
        "pull should report files:\n{stdout}"
    );
    // Framework tables must not have been turned into models.
    assert!(!project.join("src/models/autumn_job.rs").exists());
    assert!(!project.join("src/models/api_token.rs").exists());

    // 4. The model honors Autumn conventions and the inverse type mapping.
    let model = std::fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(model.contains("#[autumn_web::model]"));
    assert!(model.contains("#[id]"));
    assert!(model.contains("pub id: i64,"));
    assert!(model.contains("pub title: String,"));
    assert!(model.contains("pub views: i32,"));
    assert!(model.contains("pub score: f64,"));
    assert!(model.contains("pub token: uuid::Uuid,"));
    assert!(model.contains("pub published: bool,"));
    assert!(model.contains("pub summary: Option<String>,"));
    assert!(model.contains("#[default]\n    pub created_at: chrono::NaiveDateTime,"));

    let schema = std::fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(schema.contains("posts (id)"));
    assert!(schema.contains("summary -> Nullable<Text>,"));

    let repo = std::fs::read_to_string(project.join("src/repositories/post.rs")).unwrap();
    assert!(
        repo.contains("#[autumn_web::repository(Post, table = \"posts\", api = \"/api/posts\")]")
    );

    let main = std::fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("mod models;"));
    assert!(main.contains("mod schema;"));
    assert!(main.contains("mod repositories;"));

    // 5. db pull is read-only: it must NOT have written a new migration.
    let migrations_after = std::fs::read_dir(project.join("migrations"))
        .unwrap()
        .count();
    assert_eq!(
        migrations_before, migrations_after,
        "db pull must not emit migrations"
    );

    // 6. The regenerated app compiles with zero hand-editing (AC7).
    let (ok, output) = cargo_check(&project);
    assert!(ok, "generated app must `cargo check` cleanly:\n{output}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn db_pull_fails_loudly_on_unsupported_column_type() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_unsupported_app");

    // A table with a NUMERIC column — outside the documented type surface.
    write_migration(
        &project,
        "20260101000000_create_ledgers",
        "CREATE TABLE ledgers (id BIGSERIAL PRIMARY KEY, amount NUMERIC NOT NULL, \
         created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE ledgers;",
    );
    run_autumn_ok(&project, &["migrate"], &envs);

    // An explicitly-requested table with an unsupported type must fail loudly,
    // name the offending column, list supported types, and never leak creds.
    // (An unscoped pull would instead skip it — see the test below.)
    let (stdout, stderr) = run_autumn_fail(&project, &["db", "pull", "ledgers"], &envs);
    assert!(
        stderr.contains("ledgers.amount"),
        "error must name the column: {stderr}"
    );
    assert!(
        stderr.contains("Supported:"),
        "error must list supported types: {stderr}"
    );
    assert!(
        !stdout.contains(SECRET_PW) && !stderr.contains(SECRET_PW),
        "credentials leaked!\nstdout: {stdout}\nstderr: {stderr}"
    );
    // Nothing should have been written for the failed table.
    assert!(!project.join("src/models/ledger.rs").exists());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn db_pull_unscoped_skips_unsupported_tables_and_pulls_the_rest() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_mixed_app");
    patch_generated_cargo_toml(&project);

    // One supported table and one with an unmapped (NUMERIC) column.
    run_autumn_ok(
        &project,
        &["generate", "model", "Post", "title:String"],
        &envs,
    );
    write_migration(
        &project,
        "20260101000000_create_ledgers",
        "CREATE TABLE ledgers (id BIGSERIAL PRIMARY KEY, amount NUMERIC NOT NULL);",
        "DROP TABLE ledgers;",
    );
    run_autumn_ok(&project, &["migrate"], &envs);
    std::fs::remove_dir_all(project.join("src/models")).unwrap();
    std::fs::remove_file(project.join("src/schema.rs")).unwrap();

    // Unscoped pull: ledgers is skipped (unsupported type), posts is generated.
    let (_, stderr) = run_autumn_ok(&project, &["db", "pull"], &envs);
    assert!(
        stderr.contains("Skipping table 'ledgers'"),
        "unsupported table should be skipped with a notice: {stderr}"
    );
    assert!(project.join("src/models/post.rs").exists());
    assert!(!project.join("src/models/ledger.rs").exists());
}
