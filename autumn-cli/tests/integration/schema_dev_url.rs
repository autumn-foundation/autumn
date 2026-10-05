//! `autumn schema diff --dev-url` against a live Postgres (#1975, Decision 2).
//!
//! The tests need Docker (testcontainers), so they are `#[ignore]`d. CI runs
//! them in its Docker sweep. To run them by hand:
//!
//!   `cargo test -p autumn-cli --test cli_tests -- --ignored schema_dev_url`

use std::path::Path;
use std::process::Command;

use diesel::{Connection as _, PgConnection, RunQueryDsl as _};

const SECRET_PW: &str = "s3cr3t_dev_url_pw";

const MODELS_V1: &str = r"
#[autumn_web::model(managed)]
pub struct Author {
    #[id]
    pub id: i64,
    pub name: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::model(managed)]
pub struct Post {
    #[id]
    pub id: i64,
    #[references]
    pub author_id: i64,
    pub title: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}
";

/// `MODELS_V1` plus a nullable `posts.body`.
fn models_v2() -> String {
    MODELS_V1.replace(
        "    pub title: String,\n",
        "    pub title: String,\n    pub body: Option<String>,\n",
    )
}

fn run_autumn(dir: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let output = Command::new(env!("CARGO_BIN_EXE_autumn"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    );
    assert!(
        !stdout.contains(SECRET_PW) && !stderr.contains(SECRET_PW),
        "credentials leaked!\nstdout: {stdout}\nstderr: {stderr}"
    );
    (stdout, stderr, output.status.code())
}

fn run_ok(dir: &Path, args: &[&str]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args);
    assert_eq!(
        code,
        Some(0),
        "autumn {args:?}\nstdout: {stdout}\nstderr: {stderr}"
    );
    (stdout, stderr)
}

/// A minimal project whose first migration the tool generates from
/// `MODELS_V1`. The snapshot advances with that migration.
fn project_with_initial_migration() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"devurl\"\n").expect("Cargo.toml");
    std::fs::create_dir_all(dir.join("src")).expect("mkdir src");
    std::fs::write(dir.join("empty_models.rs"), "").expect("empty models");
    run_ok(
        dir,
        &[
            "schema",
            "snapshot",
            "--from",
            "empty_models.rs",
            "--backend",
            "pg",
        ],
    );
    std::fs::write(dir.join("src/models.rs"), MODELS_V1).expect("models v1");
    run_ok(
        dir,
        &[
            "schema",
            "diff",
            "--backend",
            "pg",
            "--write-migration",
            "--name",
            "init",
        ],
    );
    // A migration version is a 1-second timestamp. Give `init` an early version
    // so a later migration from the same second always runs after it.
    let migrations = dir.join("migrations");
    let generated = std::fs::read_dir(&migrations)
        .expect("read migrations")
        .next()
        .expect("one migration")
        .expect("entry")
        .path();
    std::fs::rename(generated, migrations.join("20000101000000_init")).expect("rename init");
    root
}

async fn start_postgres() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
) {
    use testcontainers::ImageExt as _;
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .with_password(SECRET_PW)
        .with_tag("16-alpine")
        .start()
        .await
        .expect("failed to start Postgres testcontainer — is Docker running?");
    let host = container.get_host().await.expect("host").to_string();
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    (container, url)
}

fn public_tables(url: &str) -> Vec<String> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Text)]
        name: String,
    }
    let mut conn = PgConnection::establish(url).expect("connect");
    diesel::sql_query(
        "SELECT table_name AS name FROM information_schema.tables \
         WHERE table_schema = 'public' ORDER BY table_name",
    )
    .load::<Row>(&mut conn)
    .expect("list tables")
    .into_iter()
    .map(|r| r.name)
    .collect()
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn schema_dev_url_diffs_against_the_replay_and_leaves_the_database_empty() {
    let (_container, url) = start_postgres().await;
    let root = project_with_initial_migration();
    let dir = root.path();
    // A tool-made project: the replay matches the snapshot, so no warning.
    let (out, err) = run_ok(
        dir,
        &["schema", "diff", "--backend", "pg", "--dev-url", &url],
    );
    assert!(out.contains("No schema changes"), "{out}");
    assert!(!err.contains("warning"), "{err}");
    // The snapshot is not needed with --dev-url.
    std::fs::remove_file(dir.join(".autumn/schema-snapshot.json")).expect("rm snapshot");
    std::fs::write(dir.join("src/models.rs"), models_v2()).expect("models v2");

    let (out, _) = run_ok(
        dir,
        &["schema", "diff", "--backend", "pg", "--dev-url", &url],
    );
    assert!(out.contains("1 change(s)"), "{out}");
    assert!(out.contains("ADD COLUMN posts.body"), "{out}");
    assert!(public_tables(&url).is_empty(), "the replay must roll back");

    // --write-migration writes the delta and creates the snapshot.
    run_ok(
        dir,
        &[
            "schema",
            "diff",
            "--backend",
            "pg",
            "--dev-url",
            &url,
            "--write-migration",
            "--name",
            "add_body",
        ],
    );
    let snapshot = std::fs::read_to_string(dir.join(".autumn/schema-snapshot.json"))
        .expect("the snapshot is written");
    assert!(
        !snapshot.contains("\"managed\": false"),
        "model tables stay managed: {snapshot}"
    );
    let (out, _) = run_ok(
        dir,
        &["schema", "diff", "--backend", "pg", "--dev-url", &url],
    );
    assert!(out.contains("No schema changes"), "{out}");
    assert!(public_tables(&url).is_empty(), "the replay must roll back");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn schema_dev_url_refuses_a_database_that_is_not_empty() {
    let (_container, url) = start_postgres().await;
    let mut conn = PgConnection::establish(&url).expect("connect");
    // A table outside `public` counts too.
    for sql in [
        "CREATE SCHEMA app",
        "CREATE TABLE app.leftover (id BIGINT PRIMARY KEY)",
    ] {
        diesel::sql_query(sql).execute(&mut conn).expect("create");
    }
    let root = project_with_initial_migration();

    let (_, err, code) = run_autumn(
        root.path(),
        &["schema", "diff", "--backend", "pg", "--dev-url", &url],
    );
    assert_ne!(code, Some(0));
    assert!(
        err.contains("not empty") && err.contains("app.leftover"),
        "{err}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn schema_dev_url_sees_hand_written_migrations_and_warns_on_snapshot_drift() {
    let (_container, url) = start_postgres().await;
    let root = project_with_initial_migration();
    let dir = root.path();
    let raw = dir.join("migrations/2099-01-01-000000_hand_written");
    std::fs::create_dir_all(&raw).expect("mkdir");
    std::fs::write(
        raw.join("up.sql"),
        "ALTER TABLE posts ADD COLUMN legacy TEXT NULL;\n\
         CREATE TABLE audit_log (id BIGSERIAL PRIMARY KEY, note TEXT NULL);\n",
    )
    .expect("up.sql");
    std::fs::write(raw.join("down.sql"), "SELECT 1;\n").expect("down.sql");

    // The offline snapshot cannot see the hand-written column.
    let (out, _) = run_ok(dir, &["schema", "diff", "--backend", "pg"]);
    assert!(out.contains("No schema changes"), "{out}");

    // The replay can: the models lack `legacy`, so the diff is a refused drop.
    // The table without a model (`audit_log`) is never dropped.
    let (_, err, code) = run_autumn(
        dir,
        &["schema", "diff", "--backend", "pg", "--dev-url", &url],
    );
    assert_ne!(code, Some(0), "{err}");
    assert!(
        err.contains("snapshot does not match the migrations"),
        "{err}"
    );
    let refusal = err
        .lines()
        .find(|l| l.starts_with("error:"))
        .unwrap_or_else(|| panic!("no error line: {err}"));
    assert!(refusal.contains("DROP COLUMN posts.legacy"), "{refusal}");
    assert!(!refusal.contains("audit_log"), "{refusal}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn schema_dev_url_refuses_a_new_table_named_like_a_replayed_view() {
    let (_container, url) = start_postgres().await;
    let root = project_with_initial_migration();
    let dir = root.path();
    let raw = dir.join("migrations/2099-01-01-000000_report_view");
    std::fs::create_dir_all(&raw).expect("mkdir");
    std::fs::write(
        raw.join("up.sql"),
        "CREATE VIEW reports AS SELECT id FROM posts;\n",
    )
    .expect("up.sql");
    std::fs::write(raw.join("down.sql"), "DROP VIEW reports;\n").expect("down.sql");
    let models = format!(
        "{MODELS_V1}\n#[autumn_web::model(managed)]\npub struct Report {{\n    #[id]\n    pub id: i64,\n}}\n"
    );
    std::fs::write(dir.join("src/models.rs"), models).expect("models");

    let (_, err, code) = run_autumn(
        dir,
        &["schema", "diff", "--backend", "pg", "--dev-url", &url],
    );
    assert_ne!(code, Some(0), "{err}");
    assert!(err.contains("`reports` is already a"), "{err}");

    // A standalone composite type shares the namespace too.
    std::fs::write(
        raw.join("up.sql"),
        "CREATE VIEW reports AS SELECT id FROM posts;\nCREATE TYPE report_rows AS (id bigint);\n",
    )
    .expect("up.sql");
    let models = format!(
        "{MODELS_V1}\n#[autumn_web::model(managed)]\npub struct ReportRow {{\n    #[id]\n    pub id: i64,\n}}\n"
    );
    std::fs::write(dir.join("src/models.rs"), models).expect("models");
    let (_, err, code) = run_autumn(
        dir,
        &["schema", "diff", "--backend", "pg", "--dev-url", &url],
    );
    assert_ne!(code, Some(0), "{err}");
    assert!(err.contains("`report_rows` is already a"), "{err}");
}
