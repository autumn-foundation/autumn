//! Integration tests for `autumn schema pull` (Postgres database → snapshot IR)
//! and the `schema doctor` database-schema-drift check (the first
//! DB-introspection slice of issue #1975).
//!
//! The Docker-dependent tests require testcontainers and are marked `#[ignore]`,
//! so they only run when explicitly requested (and are swept automatically in
//! CI):
//!
//!   `cargo test -p autumn-cli --test cli_tests -- --ignored schema_pull`
//!
//! The headline test proves the round-trip fidelity acceptance: a schema built
//! from `#[model]` structs, migrated into a live Postgres, then `schema pull`ed
//! back, produces a snapshot that a subsequent `schema diff` reports as
//! unchanged. Credentials never leak into any command output.

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

/// Overwrite the project's models with `content` in the single-file
/// `src/models.rs` layout (removing any scaffolded `src/models/` directory so the
/// single-file layout resolves).
fn write_models(project: &Path, content: &str) {
    let src = project.join("src");
    let _ = std::fs::remove_dir_all(src.join("models"));
    std::fs::write(src.join("models.rs"), content).expect("write models.rs");
}

/// Write a raw diesel migration under `migrations/<name>/`.
fn write_raw_migration(project: &Path, name: &str, up_sql: &str, down_sql: &str) {
    let dir = project.join("migrations").join(name);
    std::fs::create_dir_all(&dir).expect("mkdir migration");
    std::fs::write(dir.join("up.sql"), up_sql).expect("up.sql");
    std::fs::write(dir.join("down.sql"), down_sql).expect("down.sql");
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

fn assert_no_secret_leak(stdout: &str, stderr: &str) {
    assert!(
        !stdout.contains(SECRET_PW) && !stderr.contains(SECRET_PW),
        "credentials leaked!\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// Three well-behaved models covering the full mapped type surface plus a foreign
/// key (`Post.author_id → authors`), a unique column, a `NUMERIC` decimal, a
/// `JSONB` attachment, and a `TIMESTAMPTZ`. `Session` additionally exercises the
/// key judgment call end-to-end: a **UUID primary key** (`id UUID PRIMARY KEY
/// DEFAULT gen_random_uuid()`), whose `gen_random_uuid()` default must survive the
/// round-trip (the model parser recovers it, so pulling it back is not read as
/// drift), plus a `DOUBLE PRECISION` float and a nullable column.
const ROUND_TRIP_MODELS: &str = r"
#[autumn_web::model(managed)]
pub struct Author {
    #[id]
    pub id: i64,
    #[unique]
    pub email: String,
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
    pub views: i32,
    pub published: bool,
    pub rating: rust_decimal::Decimal,
    pub metadata: autumn_web::storage::Blob,
    pub launched_at: chrono::DateTime<chrono::Utc>,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::model(managed)]
pub struct Session {
    #[id]
    pub id: uuid::Uuid,
    pub label: String,
    pub weight: f64,
    pub note: Option<String>,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}
";

/// Build the schema from the models: an empty baseline snapshot, then
/// `schema diff --write-migration`, then `schema migrate` so the tables exist in
/// the live database.
fn build_schema_into_db(project: &Path, envs: &[(&str, &str)]) {
    std::fs::write(project.join("empty_models.rs"), "").expect("empty models");
    run_autumn_ok(
        project,
        &[
            "schema",
            "snapshot",
            "--from",
            "empty_models.rs",
            "--backend",
            "pg",
        ],
        envs,
    );
    run_autumn_ok(
        project,
        &[
            "schema",
            "diff",
            "--write-migration",
            "--name",
            "create_schema",
        ],
        envs,
    );
    run_autumn_ok(project, &["schema", "migrate"], envs);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
#[allow(clippy::too_many_lines)] // linear end-to-end round-trip reads clearest whole
async fn schema_pull_round_trips_models_through_the_database() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_roundtrip_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // 1. Declare the models and materialize them in the live database.
    write_models(&project, ROUND_TRIP_MODELS);
    build_schema_into_db(&project, &envs);

    // 2. Pull the live database back into the snapshot, OVERWRITING the
    //    model-derived baseline with the DB-introspected one.
    let (pull_out, pull_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert!(
        pull_out.contains("pulled schema snapshot") && pull_out.contains("3 table(s)"),
        "pull reports the table count: {pull_out}"
    );
    assert_no_secret_leak(&pull_out, &pull_err);

    // 3. The pulled snapshot faithfully carries both tables and the full type
    //    spread (the FK target, the unique index, the decimal, the attachment).
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"name\": \"authors\""),
        "authors table: {snap}"
    );
    assert!(snap.contains("\"name\": \"posts\""), "posts table: {snap}");
    assert!(
        snap.contains("\"name\": \"author_id\""),
        "fk column present"
    );
    assert!(
        snap.contains("\"table\": \"authors\""),
        "the FK references authors: {snap}"
    );
    assert!(
        snap.contains("idx_authors_email_unique"),
        "unique index: {snap}"
    );
    assert!(
        snap.contains("idx_posts_author_id"),
        "fk auto-index: {snap}"
    );
    assert!(
        snap.contains("\"Decimal\""),
        "decimal type preserved: {snap}"
    );
    assert!(snap.contains("\"Attachment\""), "jsonb→Attachment: {snap}");
    assert!(
        snap.contains("\"TimestampTz\""),
        "timestamptz preserved: {snap}"
    );
    // The UUID-PK judgment call: the sessions table pulls back with a UUID id
    // whose `gen_random_uuid()` default is preserved (so the round-trip below is
    // clean rather than flagging the default as drift), plus a float and a
    // nullable column.
    assert!(
        snap.contains("\"name\": \"sessions\""),
        "sessions table: {snap}"
    );
    assert!(snap.contains("\"Uuid\""), "uuid PK type preserved: {snap}");
    assert!(
        snap.contains("gen_random_uuid()"),
        "uuid PK default preserved: {snap}"
    );
    assert!(
        snap.contains("\"Float64\""),
        "float column preserved: {snap}"
    );
    // No framework/bookkeeping tables leaked into the pulled snapshot.
    assert!(
        !snap.contains("__diesel_schema_migrations"),
        "no diesel table"
    );

    // 4. THE ACCEPTANCE: a subsequent `schema diff` (models vs the pulled
    //    snapshot) reports no changes — the DB-derived snapshot is byte-faithful
    //    to what the models produce, so the round-trip is clean.
    let (diff_out, diff_err) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        diff_out.contains("No schema changes"),
        "round-trip must yield an empty diff:\n{diff_out}"
    );
    assert_no_secret_leak(&diff_out, &diff_err);

    // 5. doctor agrees the database matches the (pulled) snapshot baseline.
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database-schema-drift")
            && doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_preserves_unmappable_types_as_opaque() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_opaque_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // A table with an `inet` column — outside Autumn's mapped surface. Created via
    // a raw migration so the unmappable type reaches the live catalog.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_gadgets",
        "CREATE TABLE gadgets (id BIGSERIAL PRIMARY KEY, addr inet NOT NULL);",
        "DROP TABLE gadgets;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the unmappable column must be preserved as an Opaque type carrying
    // the raw Postgres type name, never dropped.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"name\": \"gadgets\""),
        "gadgets pulled: {snap}"
    );
    assert!(
        snap.contains("\"name\": \"addr\""),
        "opaque column present: {snap}"
    );
    assert!(
        snap.contains("\"Opaque\""),
        "opaque variant emitted: {snap}"
    );
    assert!(
        snap.contains("\"pg_type\": \"inet\""),
        "raw pg type preserved verbatim: {snap}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_doctor_reports_database_schema_drift() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_doctor_drift_app");

    // 1. A simple model, built into the database. The snapshot (advanced at diff
    //    time) now matches the DB, so doctor sees no DB drift.
    write_models(
        &project,
        "#[autumn_web::model(managed)]\npub struct Post {\n    #[id]\n    pub id: i64,\n    pub title: String,\n}\n",
    );
    build_schema_into_db(&project, &envs);

    let (clean_out, _) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        clean_out.contains("database schema matches the snapshot baseline"),
        "doctor is clean before the out-of-band change:\n{clean_out}"
    );

    // 2. Mutate the live database OUT OF BAND (a raw migration the snapshot does
    //    not know about) — `schema migrate` applies it but never advances the
    //    snapshot, so the DB now drifts from the baseline.
    write_raw_migration(
        &project,
        "20990101000000_add_extra",
        "ALTER TABLE posts ADD COLUMN extra TEXT;",
        "ALTER TABLE posts DROP COLUMN extra;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // 3. doctor now reports the database-schema-drift check as a WARN. A WARN
    //    never fails the command (doctor stays runnable), so it still exits 0.
    let (drift_stdout, drift_stderr) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        drift_stdout.contains("database-schema-drift") && drift_stdout.contains("[WARN]"),
        "doctor reports DB drift as a WARN:\n{drift_stdout}"
    );
    assert!(
        drift_stdout.contains("differs from the snapshot")
            && drift_stdout.contains("autumn schema pull"),
        "the drift detail names the remediation:\n{drift_stdout}"
    );
    assert_no_secret_leak(&drift_stdout, &drift_stderr);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_dry_run_reports_diff_without_writing() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_dry_run_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // 1. Build a schema and pull it, so the snapshot reflects the DB.
    write_models(
        &project,
        "#[autumn_web::model(managed)]\npub struct Post {\n    #[id]\n    pub id: i64,\n    pub title: String,\n}\n",
    );
    build_schema_into_db(&project, &envs);
    run_autumn_ok(&project, &["schema", "pull"], &envs);
    let snap_before = std::fs::read_to_string(&snapshot_path).expect("snapshot before dry-run");

    // 2. Mutate the DB out of band so a fresh pull would differ from the snapshot.
    write_raw_migration(
        &project,
        "20990202000000_add_note",
        "ALTER TABLE posts ADD COLUMN note TEXT;",
        "ALTER TABLE posts DROP COLUMN note;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // 3. A dry-run pull reports the pending change but writes NOTHING.
    let (dry_out, dry_err) = run_autumn_ok(&project, &["schema", "pull", "--dry-run"], &envs);
    assert!(
        dry_out.contains("--dry-run") && dry_out.contains("differs from the snapshot"),
        "dry-run reports the diff:\n{dry_out}"
    );
    assert_no_secret_leak(&dry_out, &dry_err);

    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after dry-run");
    assert_eq!(
        snap_before, snap_after,
        "a --dry-run pull must leave the snapshot byte-for-byte unchanged"
    );
    assert!(
        !snap_after.contains("note"),
        "the out-of-band `note` column must NOT be written by a dry-run: {snap_after}"
    );
}

/// Expression and partial indexes must be **preserved verbatim** by `schema pull`
/// (via each index's `pg_get_indexdef` `definition`), never silently dropped or
/// degraded to a plain column index (the P1 fixed here). An expression index over
/// `lower(email)` has an `indkey` of `0` for the expression column (no matching
/// `pg_attribute` row), and a partial index carries a `WHERE` predicate — both
/// were lost by the prior inner-join fetch. After pulling, both index names and
/// their definition text (`lower(email)` / the `WHERE` predicate) appear in the
/// snapshot, and a second pull reports no drift against the just-pulled baseline.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_preserves_expression_and_partial_indexes() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_expr_index_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // A table with an EXPRESSION index (`lower(email)`) and a PARTIAL index
    // (`WHERE deleted_at IS NULL`), created via a raw migration so they reach the
    // live catalog exactly as authored.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_members",
        "CREATE TABLE members (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL, deleted_at TIMESTAMP);\n\
         CREATE INDEX members_lower_email_idx ON members (lower(email));\n\
         CREATE INDEX members_active_email_idx ON members (email) WHERE deleted_at IS NULL;",
        "DROP TABLE members;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: both non-representable indexes must be preserved, not dropped.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");

    assert!(
        snap.contains("members_lower_email_idx"),
        "expression index preserved by name: {snap}"
    );
    assert!(
        snap.contains("lower(email)"),
        "expression index definition preserved verbatim: {snap}"
    );
    assert!(
        snap.contains("members_active_email_idx"),
        "partial index preserved by name: {snap}"
    );
    assert!(
        // pg_get_indexdef renders the predicate; assert the WHERE clause survived.
        snap.contains("WHERE") && snap.contains("deleted_at IS NULL"),
        "partial index predicate preserved verbatim: {snap}"
    );
    assert!(
        snap.contains("\"definition\""),
        "the raw definition field is serialized for non-representable indexes: {snap}"
    );

    // A second pull round-trips clean against the just-pulled baseline: two
    // introspected snapshots of the same DB agree, so no drift is reported.
    let snap_before = snap;
    let (pull2_out, pull2_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&pull2_out, &pull2_err);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after second pull");
    assert_eq!(
        snap_before, snap_after,
        "a re-pull of the same database must be byte-for-byte stable (no expression/partial drift)"
    );

    // doctor agrees the database still matches the pulled baseline (no drift from
    // the preserved expression/partial indexes).
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with expression/partial indexes:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);

    // A MODEL-side `schema diff` against the pulled snapshot must RETAIN the
    // expression/partial indexes — the model DSL cannot express them, so their
    // absence from the desired side is not a removal. The diff is clean (no
    // destructive DropIndex that would clobber `lower(email)` with a plain index
    // or strip the partial predicate).
    write_models(
        &project,
        "
#[autumn_web::model(managed)]
pub struct Member {
    #[id]
    pub id: i64,
    pub email: String,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}
",
    );
    let (mdiff_out, mdiff_err) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        !mdiff_out.contains("DROP INDEX")
            && !mdiff_out.contains("members_lower_email_idx")
            && !mdiff_out.contains("members_active_email_idx"),
        "a model-side `schema diff` must NOT drop the unmodellable expression/partial indexes:\n{mdiff_out}"
    );
    assert!(
        mdiff_out.contains("No schema changes"),
        "model diff against the pulled snapshot is clean (definition indexes retained):\n{mdiff_out}"
    );
    assert_no_secret_leak(&mdiff_out, &mdiff_err);
}

/// `SQLite` introspection is a future slice: `schema pull` against a `SQLite` URL
/// is refused loudly (no Docker needed — the refusal happens before any
/// connection), names `SQLite`, and writes no snapshot.
#[test]
fn schema_pull_refuses_sqlite_backend() {
    let (_tmp, project) = fresh_project("pull_sqlite_refusal_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");
    let existed_before = snapshot_path.exists();

    let envs = [("AUTUMN_DATABASE__URL", "sqlite://./pull_test.db")];
    let (stdout, stderr) = run_autumn_fail(&project, &["schema", "pull"], &envs);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("SQLite") && combined.to_lowercase().contains("postgres only"),
        "refusal names SQLite as a future slice:\n{combined}"
    );

    // No snapshot was written by the refused pull.
    assert_eq!(
        existed_before,
        snapshot_path.exists(),
        "a refused SQLite pull must not create a snapshot file"
    );
}
