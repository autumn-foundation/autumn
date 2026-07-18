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
    use testcontainers::ImageExt as _;
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    // Pin a modern Postgres image (the module default is `postgres:11-alpine`).
    // The round-trip fixture's UUID PK generates `DEFAULT gen_random_uuid()`, a
    // built-in only from PostgreSQL 13+, so PG11 would fail `schema migrate` with
    // `function gen_random_uuid() does not exist`.
    let container = Postgres::default()
        .with_password(SECRET_PW)
        .with_tag("16-alpine")
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
        // `created_at TIMESTAMP NOT NULL DEFAULT NOW()` matches the column the
        // managed-model parser synthesizes for `Member` below, so the later
        // model-side `schema diff` is genuinely empty (no spurious ADD COLUMN)
        // and validates only the index-retention behavior.
        "CREATE TABLE members (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL, deleted_at TIMESTAMP, created_at TIMESTAMP NOT NULL DEFAULT NOW());\n\
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

/// A brownfield column-level `UNIQUE` constraint (`email TEXT UNIQUE`) auto-creates
/// a `pg_constraint`-backed index Postgres names (`<table>_email_key`). `schema
/// pull` must RETAIN that constraint-owned index verbatim via its `definition`
/// (the P1 fixed here) — recording it as an ordinary droppable index would make a
/// later model-side `schema diff` emit a `DROP INDEX` that Postgres REJECTS
/// (`cannot drop index ... because constraint ... requires it`), producing a
/// FAILING migration. The model DSL cannot express a UNIQUE constraint (it models
/// uniqueness as unique *indexes*), so a model diff that does not redeclare the
/// unique must be clean: no `DROP INDEX` for the constraint index.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_retains_constraint_owned_unique_index() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_constraint_index_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // A table with a column-level `UNIQUE` constraint, authored via a raw
    // migration so Postgres auto-creates the constraint-backed `accounts_email_key`
    // index exactly as a brownfield database would have it.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_accounts",
        // `created_at TIMESTAMP NOT NULL DEFAULT NOW()` matches the column the
        // managed-model parser synthesizes for `Account` below, so the later
        // model-side `schema diff` is genuinely empty (no spurious ADD COLUMN)
        // and validates only the constraint-owned-index retention.
        "CREATE TABLE accounts (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL UNIQUE, created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE accounts;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the constraint-backed index must be preserved via its definition.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");

    assert!(
        snap.contains("accounts_email_key"),
        "constraint-backed index preserved by its Postgres-assigned name: {snap}"
    );
    assert!(
        snap.contains("\"definition\"") && snap.contains("CREATE UNIQUE INDEX"),
        "the constraint-owned index is retained via its verbatim CREATE UNIQUE INDEX definition: {snap}"
    );

    // doctor agrees the DB matches the pulled baseline (retained index is stable).
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the constraint-owned index:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);

    // A MODEL-side `schema diff` whose model does NOT redeclare the unique must
    // RETAIN the constraint-owned index — no `DROP INDEX` (which Postgres would
    // reject). The diff is clean.
    write_models(
        &project,
        "
#[autumn_web::model(managed)]
pub struct Account {
    #[id]
    pub id: i64,
    pub email: String,
}
",
    );
    let (mdiff_out, mdiff_err) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        !mdiff_out.contains("DROP INDEX") && !mdiff_out.contains("accounts_email_key"),
        "a model-side `schema diff` must NOT drop the constraint-owned index:\n{mdiff_out}"
    );
    assert!(
        mdiff_out.contains("No schema changes"),
        "model diff against the pulled snapshot is clean (constraint-owned index retained):\n{mdiff_out}"
    );
    assert_no_secret_leak(&mdiff_out, &mdiff_err);
}

/// A range-based `EXCLUDE` constraint (`EXCLUDE USING gist (during WITH &&)`) is
/// backed by a gist index Postgres auto-creates. Retaining ONLY the backing
/// `CREATE INDEX` (`pg_get_indexdef`) would NOT enforce the exclusion on a
/// rollback/recreation — overlapping rows would become possible, a data-integrity
/// loss. `schema pull` must therefore retain the REAL constraint recreation via the
/// index `definition` — `ALTER TABLE … ADD CONSTRAINT … EXCLUDE USING …`
/// (`pg_get_constraintdef`) — so the snapshot round-trips the actual exclusion. A
/// re-pull is byte-stable and `schema doctor` reports no drift.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_retains_exclude_constraint_as_real_constraint() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_exclude_constraint_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // A table with a range EXCLUDE constraint. `tstzrange WITH &&` needs no
    // extension (gist supports range types out of the box), so no `CREATE EXTENSION`
    // is required. `created_at TIMESTAMP NOT NULL DEFAULT NOW()` matches the column
    // the managed-model parser would synthesize; the EXCLUDE itself is unmodellable.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_reservations",
        "CREATE TABLE reservations (id BIGSERIAL PRIMARY KEY, during tstzrange NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT NOW(), EXCLUDE USING gist (during WITH &&));",
        "DROP TABLE reservations;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the EXCLUDE constraint must be retained as the real ADD CONSTRAINT form,
    // NOT the bare backing CREATE INDEX.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");

    assert!(
        snap.contains("EXCLUDE USING gist"),
        "the EXCLUDE constraint is retained via its ADD CONSTRAINT … EXCLUDE USING … definition: {snap}"
    );
    assert!(
        snap.contains("ADD CONSTRAINT"),
        "the retained definition is the real constraint recreation (ALTER TABLE … ADD CONSTRAINT): {snap}"
    );
    assert!(
        snap.contains("\"definition\""),
        "the raw definition field is serialized for the EXCLUDE constraint index: {snap}"
    );

    // A second pull round-trips clean against the just-pulled baseline (byte-stable).
    let snap_before = snap;
    let (pull2_out, pull2_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&pull2_out, &pull2_err);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after second pull");
    assert_eq!(
        snap_before, snap_after,
        "a re-pull of the same database must be byte-for-byte stable (no EXCLUDE-constraint drift)"
    );

    // doctor agrees the DB matches the pulled baseline (retained EXCLUDE is stable).
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the EXCLUDE constraint:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// Brownfield adoption with the natural `#[unique]` annotation (the P2 fix): after
/// `schema pull` retains the constraint-owned `accounts_email_key` index, the user
/// writes the natural model `Account { id, email }` and DOES declare `#[unique]` on
/// `email`. The parser emits its own differently-named `idx_accounts_email_unique`
/// unique index over `[email]`. Before the fix, the model-side `schema diff` saw
/// that name as absent from the baseline and emitted a unique `AddIndex`, which the
/// policy guard then REJECTED (a unique index on a populated table needs dedup
/// verification) — so the user could not keep the `#[unique]` annotation even
/// though the database already enforces that exact uniqueness. After the fix the
/// existing unique index (same column set) satisfies the desired `#[unique]`, so
/// the diff is CLEAN: no `AddIndex`, no `DROP INDEX`, no guard rejection.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_diff_model_unique_matches_pulled_constraint_index() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_model_unique_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // Brownfield `accounts(id, email UNIQUE, created_at)` — Postgres auto-creates
    // the constraint-backed `accounts_email_key` index. `created_at TIMESTAMP NOT
    // NULL DEFAULT NOW()` matches the column the managed-model parser synthesizes
    // for `Account` below, so the diff isolates the unique-index behavior.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_accounts",
        "CREATE TABLE accounts (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL UNIQUE, created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE accounts;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull the brownfield shape (retains the constraint-owned index).
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("accounts_email_key"),
        "constraint-backed index retained by its Postgres-assigned name: {snap}"
    );

    // The natural model DOES declare `#[unique]` on email. The existing unique
    // index must satisfy it — the diff must be clean, with no guard rejection.
    write_models(
        &project,
        "
#[autumn_web::model(managed)]
pub struct Account {
    #[id]
    pub id: i64,
    #[unique]
    pub email: String,
}
",
    );
    let (mdiff_out, mdiff_err) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        !mdiff_out.contains("AddIndex") && !mdiff_out.contains("CREATE UNIQUE INDEX"),
        "a model #[unique] over a brownfield-UNIQUE column must NOT emit AddIndex:\n{mdiff_out}"
    );
    assert!(
        !mdiff_out.contains("DROP INDEX") && !mdiff_out.contains("accounts_email_key"),
        "the retained constraint-owned index must NOT be dropped:\n{mdiff_out}"
    );
    assert!(
        mdiff_out.contains("No schema changes"),
        "model diff with #[unique] against the pulled unique constraint is clean:\n{mdiff_out}"
    );
    assert_no_secret_leak(&mdiff_out, &mdiff_err);
}

/// A covering `INCLUDE` index (`CREATE UNIQUE INDEX … ON t (a) INCLUDE (b)`) must
/// be RETAINED verbatim via its `definition`, not recorded as a plain `(a, b)`
/// unique index. Recording it as an ordinary index would widen the uniqueness
/// scope from `(a)` to `(a, b)` and drop the covering behavior on recreation — and
/// the model DSL cannot express the key/INCLUDE split. So the snapshot keeps the
/// verbatim `CREATE UNIQUE INDEX … INCLUDE …` definition and a later model-side
/// `schema diff` is clean (the definition index is retained, never recreated).
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_retains_covering_include_index() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_include_index_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // A table with a covering unique index, authored via a raw migration so
    // Postgres records the `INCLUDE` clause. `created_at TIMESTAMP NOT NULL DEFAULT
    // NOW()` matches the column the managed-model parser synthesizes for `Cover`
    // below, so the later model-side `schema diff` is genuinely empty and validates
    // only the INCLUDE-index retention.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_covers",
        "CREATE TABLE covers (id BIGSERIAL PRIMARY KEY, a TEXT NOT NULL, b TEXT, created_at TIMESTAMP NOT NULL DEFAULT NOW());\n\
         CREATE UNIQUE INDEX covers_a_inc_b ON covers (a) INCLUDE (b);",
        "DROP TABLE covers;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the covering index must be preserved via its verbatim definition.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");

    assert!(
        snap.contains("covers_a_inc_b"),
        "the covering index is preserved by name: {snap}"
    );
    assert!(
        snap.contains("\"definition\"") && snap.contains("INCLUDE"),
        "the covering index is retained via its verbatim CREATE UNIQUE INDEX … INCLUDE … definition: {snap}"
    );

    // doctor agrees the DB matches the pulled baseline (retained index is stable).
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the covering index:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);

    // A MODEL-side `schema diff` whose model does NOT redeclare the index must
    // RETAIN it — no recreation, clean diff.
    write_models(
        &project,
        "
#[autumn_web::model(managed)]
pub struct Cover {
    #[id]
    pub id: i64,
    pub a: String,
    pub b: Option<String>,
}
",
    );
    let (mdiff_out, mdiff_err) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        !mdiff_out.contains("covers_a_inc_b"),
        "a model-side `schema diff` must NOT recreate or drop the covering index:\n{mdiff_out}"
    );
    assert!(
        mdiff_out.contains("No schema changes"),
        "model diff against the pulled snapshot is clean (covering index retained):\n{mdiff_out}"
    );
    assert_no_secret_leak(&mdiff_out, &mdiff_err);
}

/// Dropping a model column covered by a RETAINED constraint-owned unique index
/// must not leave the snapshot stale. Postgres cascade-drops the index with the
/// column (`ALTER TABLE … DROP COLUMN`), so the generated up-migration is a bare
/// `DROP COLUMN` (never a rejected `DROP INDEX`), the advanced snapshot prunes the
/// index (so `doctor` reports NO drift after apply), and the down-migration
/// restores the index verbatim.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_diff_dropping_uniquely_indexed_column_stays_clean() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_drop_unique_col_app");

    // Brownfield `accounts(id, email UNIQUE, note)` — Postgres auto-creates the
    // constraint-backed `accounts_email_key` index.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_accounts",
        "CREATE TABLE accounts (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL UNIQUE, note TEXT);",
        "DROP TABLE accounts;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull the brownfield shape (retains the constraint-owned index).
    run_autumn_ok(&project, &["schema", "pull"], &envs);

    // Model DROPS `email` (keeps id + note). Generate the migration; the snapshot
    // advances at generation time.
    write_models(
        &project,
        "
#[autumn_web::model(managed)]
pub struct Account {
    #[id]
    pub id: i64,
    pub note: Option<String>,
}
",
    );
    let (diff_out, diff_err) = run_autumn_ok(
        &project,
        &[
            "schema",
            "diff",
            "--write-migration",
            "--name",
            "drop_email",
            "--allow-destructive",
        ],
        &envs,
    );
    assert_no_secret_leak(&diff_out, &diff_err);

    // Read the generated up/down SQL.
    let mig_root = project.join("migrations");
    let mut up_sql = String::new();
    let mut down_sql = String::new();
    for entry in std::fs::read_dir(&mig_root).expect("migrations dir") {
        let dir = entry.expect("entry").path();
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("drop_email"))
        {
            up_sql = std::fs::read_to_string(dir.join("up.sql")).unwrap_or_default();
            down_sql = std::fs::read_to_string(dir.join("down.sql")).unwrap_or_default();
        }
    }
    assert!(
        up_sql.contains("DROP COLUMN email"),
        "up migration drops the column: {up_sql}"
    );
    assert!(
        !up_sql.contains("DROP INDEX"),
        "up migration must NOT emit a DROP INDEX for the cascade-dropped constraint index: {up_sql}"
    );
    assert!(
        down_sql.contains("CREATE UNIQUE INDEX") && down_sql.contains("accounts_email_key"),
        "down migration restores the cascade-dropped retained index: {down_sql}"
    );

    // Apply the migration (Postgres cascades the index away with the column).
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // doctor must report NO database-schema drift: the advanced snapshot pruned
    // the retained index to match the cascade-dropped DB (no false perpetual drift).
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports no drift after dropping the uniquely-indexed column:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// Dropping a column whose name merely appears as a STRING LITERAL in an unrelated
/// partial index's predicate (`WHERE kind = 'email'`) must NOT prune that index —
/// Postgres keeps it (the index depends on `id`/`kind`, never `email`). This guards
/// the exact-`pg_depend` dependency detection (Codex P1): the prior string scan of
/// the `definition` text false-matched the `'email'` literal, wrongly pruned the
/// retained index from the projected snapshot, and left `doctor` perpetually
/// drifting (or the down migration recreating an index Postgres never dropped).
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_diff_dropping_column_named_in_index_predicate_literal_stays_clean() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_predicate_literal_app");

    // Brownfield `events(id, kind, email)` with a PARTIAL index on `id` whose
    // predicate is a string literal `kind = 'email'` — it depends on `id` and
    // `kind`, NOT on the `email` column. `created_at` matches the synthesized model
    // column so the only real change below is the DROP COLUMN.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_events",
        "CREATE TABLE events (id BIGSERIAL PRIMARY KEY, kind TEXT, email TEXT, created_at TIMESTAMP NOT NULL DEFAULT NOW());\n\
         CREATE INDEX events_id_when_email_idx ON events (id) WHERE kind = 'email';",
        "DROP TABLE events;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);
    run_autumn_ok(&project, &["schema", "pull"], &envs);

    // Model DROPS `email` (keeps id + kind). The partial index does not depend on
    // `email`, so it must survive both the projection and the actual DROP COLUMN.
    write_models(
        &project,
        "
#[autumn_web::model(managed)]
pub struct Event {
    #[id]
    pub id: i64,
    pub kind: Option<String>,
}
",
    );
    let (diff_out, diff_err) = run_autumn_ok(
        &project,
        &[
            "schema",
            "diff",
            "--write-migration",
            "--name",
            "drop_email",
            "--allow-destructive",
        ],
        &envs,
    );
    assert_no_secret_leak(&diff_out, &diff_err);

    let mig_root = project.join("migrations");
    let mut up_sql = String::new();
    let mut down_sql = String::new();
    for entry in std::fs::read_dir(&mig_root).expect("migrations dir") {
        let dir = entry.expect("entry").path();
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("drop_email"))
        {
            up_sql = std::fs::read_to_string(dir.join("up.sql")).unwrap_or_default();
            down_sql = std::fs::read_to_string(dir.join("down.sql")).unwrap_or_default();
        }
    }
    assert!(
        up_sql.contains("DROP COLUMN email"),
        "up migration drops the column: {up_sql}"
    );
    // The unrelated partial index is NOT cascade-dropped, so the down migration
    // must NOT try to recreate it (Postgres never removed it).
    assert!(
        !down_sql.contains("events_id_when_email_idx"),
        "down migration must NOT recreate an index Postgres never dropped: {down_sql}"
    );

    // Apply: Postgres drops only the column and KEEPS the partial index.
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // doctor must be clean: the projected snapshot retained the partial index
    // (exact dependency detection), matching the live DB that still has it.
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports no drift — the partial index was not falsely pruned:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// Dropping TWO columns that a single RETAINED index depends on (an expression +
/// partial index `… ON t (lower(email)) WHERE tenant_id IS NOT NULL`, cascade-
/// dropped once BOTH `email` and `tenant_id` are removed) must produce a down
/// migration that re-adds BOTH columns and recreates the index EXACTLY ONCE, after
/// both re-adds. Recreating it inline after only the first re-add would (a) fail —
/// the other dependent column is still absent — and (b) duplicate the
/// `CREATE INDEX`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_diff_dropping_two_indexed_columns_recreates_index_once() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_drop_two_indexed_cols_app");

    // Brownfield `members(id, email, tenant_id, created_at)` with an expression +
    // partial index depending on BOTH `email` (via `lower(email)`) and `tenant_id`
    // (via the `WHERE` predicate). `created_at` matches the synthesized model column
    // so the only real changes are the two DROP COLUMNs.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_members",
        "CREATE TABLE members (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL, tenant_id BIGINT, created_at TIMESTAMP NOT NULL DEFAULT NOW());\n\
         CREATE INDEX members_active_idx ON members (lower(email)) WHERE tenant_id IS NOT NULL;",
        "DROP TABLE members;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);
    run_autumn_ok(&project, &["schema", "pull"], &envs);

    // Model DROPS both `email` and `tenant_id` (keeps only id + synthesized
    // created_at). The index depends on both, so it is cascade-dropped by the
    // DROP COLUMNs; the down migration must restore both columns then the index.
    write_models(
        &project,
        "
#[autumn_web::model(managed)]
pub struct Member {
    #[id]
    pub id: i64,
}
",
    );
    let (diff_out, diff_err) = run_autumn_ok(
        &project,
        &[
            "schema",
            "diff",
            "--write-migration",
            "--name",
            "drop_both",
            "--allow-destructive",
        ],
        &envs,
    );
    assert_no_secret_leak(&diff_out, &diff_err);

    let mig_root = project.join("migrations");
    let mut up_sql = String::new();
    let mut down_sql = String::new();
    for entry in std::fs::read_dir(&mig_root).expect("migrations dir") {
        let dir = entry.expect("entry").path();
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("drop_both"))
        {
            up_sql = std::fs::read_to_string(dir.join("up.sql")).unwrap_or_default();
            down_sql = std::fs::read_to_string(dir.join("down.sql")).unwrap_or_default();
        }
    }
    assert!(
        up_sql.contains("DROP COLUMN email") && up_sql.contains("DROP COLUMN tenant_id"),
        "up migration drops both columns: {up_sql}"
    );
    assert!(
        !up_sql.contains("DROP INDEX"),
        "up migration must NOT emit a DROP INDEX for the cascade-dropped index: {up_sql}"
    );

    // DOWN: re-adds BOTH columns and recreates the index EXACTLY ONCE, after both.
    assert!(
        down_sql.contains("ADD COLUMN email") && down_sql.contains("ADD COLUMN tenant_id"),
        "down migration re-adds both dependent columns: {down_sql}"
    );
    assert_eq!(
        down_sql.matches("members_active_idx").count(),
        1,
        "the multi-column retained index must be recreated exactly once (deduped): {down_sql}"
    );
    let email_at = down_sql.find("ADD COLUMN email").expect("email re-add");
    let tenant_at = down_sql
        .find("ADD COLUMN tenant_id")
        .expect("tenant_id re-add");
    let index_at = down_sql
        .find("members_active_idx")
        .expect("index recreation");
    assert!(
        email_at < index_at && tenant_at < index_at,
        "both dependent columns must be re-added BEFORE the index recreation: {down_sql}"
    );

    // Apply the up migration (Postgres cascades the index away with the columns).
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // doctor must report NO drift: the advanced snapshot pruned the index to match.
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports no drift after dropping both indexed columns:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// A brownfield `SERIAL PRIMARY KEY` (int4) column must be introspected faithfully:
/// its `nextval(...)` sequence default is PRESERVED (not stripped like the int8
/// `BIGSERIAL` id shape), so a recreation of the table reconstructs it as `SERIAL`
/// (auto-increment) rather than a plain `INTEGER PRIMARY KEY` that silently loses the
/// sequence. The faithful-representation acceptance is a byte-stable re-pull plus a
/// clean `doctor`, and the preserved `nextval` default visible in the snapshot.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_preserves_int4_serial_primary_key() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_int4_serial_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // A brownfield table whose primary key is an int4 `SERIAL` (not `BIGSERIAL`),
    // authored via a raw migration so Postgres records the `nextval(...)` default.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_counters",
        "CREATE TABLE counters (id SERIAL PRIMARY KEY, label TEXT NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE counters;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the int4 SERIAL id's sequence default must survive (NOT be stripped like
    // the int8 BIGSERIAL id shape), so the emitter can reconstruct it as `SERIAL`.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("counters"),
        "the brownfield table is captured: {snap}"
    );
    assert!(
        snap.contains("nextval"),
        "the int4 SERIAL PK's nextval default is PRESERVED (not stripped): {snap}"
    );

    // A re-pull of the same database is byte-for-byte stable (no int4-serial drift).
    let snap_before = snap;
    let (pull2_out, pull2_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&pull2_out, &pull2_err);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after second pull");
    assert_eq!(
        snap_before, snap_after,
        "a re-pull of the same database must be byte-for-byte stable (int4 serial faithful)"
    );

    // doctor agrees the database still matches the pulled baseline (no drift from the
    // preserved int4 SERIAL primary key).
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the int4 SERIAL PK:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// A brownfield int8 primary key backed by a **shared/custom** sequence the column
/// does NOT own (`id BIGINT PRIMARY KEY DEFAULT nextval('global_ids')`) must have its
/// `nextval(...)` default PRESERVED verbatim — NOT stripped like a conventional owned
/// `BIGSERIAL` id, and NOT re-emitted as a fresh `BIGSERIAL` (which would mint a new
/// owned sequence and silently change ID allocation, abandoning `global_ids`). Only a
/// sequence the column *owns* (an `OWNED BY` `pg_depend` `deptype = 'a'` row) is the
/// default-less `BIGSERIAL` shape. The faithful-representation acceptance is the
/// preserved `nextval('global_ids'...)` default visible in the snapshot, a byte-stable
/// re-pull, and a clean `doctor`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_preserves_shared_sequence_int8_primary_key() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_shared_seq_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // A brownfield table whose int8 PK draws from a STANDALONE sequence it does not
    // own (a shared/global id allocator), authored via a raw migration so Postgres
    // records the `nextval('global_ids'...)` default WITHOUT an `OWNED BY` dependency.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_events",
        "CREATE SEQUENCE global_ids;\n\
         CREATE TABLE events (id BIGINT PRIMARY KEY DEFAULT nextval('global_ids'), label TEXT NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE events;\nDROP SEQUENCE global_ids;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the shared-sequence id's default must survive verbatim — NOT stripped to
    // None (which would recreate it as a fresh owned BIGSERIAL) and NOT rewritten.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("events"),
        "the brownfield table is captured: {snap}"
    );
    assert!(
        snap.contains("global_ids"),
        "the shared-sequence int8 PK's nextval('global_ids') default is PRESERVED \
         (not stripped, not re-emitted as BIGSERIAL): {snap}"
    );

    // A re-pull of the same database is byte-for-byte stable (no shared-sequence drift).
    let snap_before = snap;
    let (pull2_out, pull2_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&pull2_out, &pull2_err);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after second pull");
    assert_eq!(
        snap_before, snap_after,
        "a re-pull of the same database must be byte-for-byte stable (shared-sequence faithful)"
    );

    // doctor agrees the database still matches the pulled baseline (no drift from the
    // preserved shared-sequence primary key).
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the shared-sequence int8 PK:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// A brownfield single-column **UUID** primary key whose default is NOT the Autumn
/// convention `gen_random_uuid()` — here a UUID PK with **no** default at all — must
/// pull back preserving "no default", NOT the convention `gen_random_uuid()`. The
/// no-default case is used deliberately so the test needs no `uuid-ossp` extension:
/// `schema pull` records the id column with no default, a re-pull is byte-stable, and
/// `schema doctor` reports no drift. This guards the emitter fix that gates the
/// `IdKind::Uuid` shape on the stored default (see `diff::pk_kind_for`) so recreation
/// never silently invents a `gen_random_uuid()` default the database never had.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_preserves_uuid_pk_without_convention_default() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_uuid_no_default_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // A brownfield table whose primary key is a bare `uuid` with NO default — the
    // application supplies the id, so Postgres records no column default. Authored via
    // a raw migration (needs no `uuid-ossp` / `pgcrypto` extension).
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_tokens",
        "CREATE TABLE tokens (id uuid PRIMARY KEY, label TEXT NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE tokens;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the UUID id is captured, but with NO `gen_random_uuid()` default invented.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("tokens"),
        "the brownfield table is captured: {snap}"
    );
    assert!(
        snap.contains("\"Uuid\""),
        "the UUID PK type is preserved: {snap}"
    );
    assert!(
        !snap.contains("gen_random_uuid()"),
        "a non-convention UUID PK must NOT gain a gen_random_uuid() default on pull: {snap}"
    );

    // A re-pull of the same database is byte-for-byte stable (no UUID-default drift).
    let snap_before = snap;
    let (pull2_out, pull2_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&pull2_out, &pull2_err);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after second pull");
    assert_eq!(
        snap_before, snap_after,
        "a re-pull of the same database must be byte-for-byte stable (UUID-no-default faithful)"
    );

    // doctor agrees the database still matches the pulled baseline (no drift from the
    // preserved default-less UUID primary key).
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the default-less UUID PK:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// A **partial** unique index (`… ON accounts(email) WHERE …`) enforces uniqueness
/// only for rows matching its predicate, so it must NOT be treated as satisfying a
/// model `#[unique] email` (which demands table-wide uniqueness). Unlike a full
/// brownfield `UNIQUE` constraint (which DOES satisfy `#[unique]` — see
/// `schema_diff_model_unique_matches_pulled_constraint_index`), the model diff here
/// must emit the unique `AddIndex`, which the policy guard then refuses on the
/// pre-existing `email` column with `UniqueIndexRequiresDedup` — the decisive signal
/// that the partial index did NOT suppress the model `#[unique]`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_diff_partial_unique_index_does_not_satisfy_model_unique() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_partial_unique_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // Brownfield `accounts(id, email, created_at)` with a PARTIAL unique index on
    // `email` (only enforces uniqueness where `email <> ''`). `created_at TIMESTAMP
    // NOT NULL DEFAULT NOW()` matches the column the managed-model parser synthesizes
    // for `Account` below, so the diff isolates the unique-index behavior.
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_accounts",
        "CREATE TABLE accounts (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT NOW());\n\
         CREATE UNIQUE INDEX accounts_email_active_key ON accounts (email) WHERE email <> '';",
        "DROP TABLE accounts;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the partial unique index is retained verbatim and flagged partial.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("accounts_email_active_key"),
        "the partial unique index is retained by name: {snap}"
    );
    assert!(
        snap.contains("\"is_partial\"") && snap.contains("WHERE"),
        "the pulled index is flagged partial and keeps its predicate: {snap}"
    );

    // The natural model declares `#[unique]` on email. A PARTIAL unique index does
    // NOT enforce table-wide uniqueness, so the diff must NOT be suppressed — it emits
    // the unique AddIndex, which the guard refuses on the pre-existing column.
    write_models(
        &project,
        "
#[autumn_web::model(managed)]
pub struct Account {
    #[id]
    pub id: i64,
    #[unique]
    pub email: String,
}
",
    );
    let (mdiff_out, mdiff_err) = run_autumn_fail(&project, &["schema", "diff"], &envs);
    let combined = format!("{mdiff_out}{mdiff_err}");
    assert!(
        combined.contains("cannot add unique index") && combined.contains("accounts"),
        "a partial unique index must NOT satisfy the model #[unique]; the diff must emit \
         the unique AddIndex (guard-refused on the pre-existing column):\n{combined}"
    );
    assert_no_secret_leak(&mdiff_out, &mdiff_err);
}

/// Fix 1 (same-name coverage): a brownfield PARTIAL unique index deliberately
/// NAMED with the model's conventional index name (`idx_members_email_unique`)
/// hits the same-NAME match branch in `diff_indexes`. Because it is
/// definition-carrying it is RETAINED, but a partial index does NOT enforce
/// table-wide uniqueness — so it must NOT suppress the model's `#[unique] email`.
/// The model diff must still emit the full unique `AddIndex`, which the policy
/// guard then refuses on the pre-existing `email` column — the decisive signal
/// that the same-named partial index did NOT silently satisfy the annotation.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_diff_same_named_partial_unique_index_does_not_satisfy_model_unique() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_same_named_partial_unique_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // Brownfield `members(id, email, created_at)` with a PARTIAL unique index on
    // `email` that carries the MODEL'S CONVENTIONAL NAME `idx_members_email_unique`
    // (the parser emits exactly this name for `#[unique] email` on `Member`).
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_members",
        "CREATE TABLE members (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT NOW());\n\
         CREATE UNIQUE INDEX idx_members_email_unique ON members (email) WHERE email IS NOT NULL;",
        "DROP TABLE members;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the partial unique index is retained verbatim and flagged partial.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("idx_members_email_unique"),
        "the conventionally-named partial unique index is retained by name: {snap}"
    );
    assert!(
        snap.contains("\"is_partial\"") && snap.contains("WHERE"),
        "the pulled index is flagged partial and keeps its predicate: {snap}"
    );

    // The natural model declares `#[unique]` on email, whose parser emits an index
    // named `idx_members_email_unique` — the SAME name as the retained partial
    // index. The same-NAME branch must NOT suppress it (a partial index cannot
    // enforce table-wide uniqueness): the diff emits the unique AddIndex, which the
    // guard refuses on the pre-existing column.
    write_models(
        &project,
        "
#[autumn_web::model(managed)]
pub struct Member {
    #[id]
    pub id: i64,
    #[unique]
    pub email: String,
}
",
    );
    let (mdiff_out, mdiff_err) = run_autumn_fail(&project, &["schema", "diff"], &envs);
    let combined = format!("{mdiff_out}{mdiff_err}");
    assert!(
        combined.contains("cannot add unique index") && combined.contains("members"),
        "a same-named partial unique index must NOT satisfy the model #[unique]; the diff \
         must emit the unique AddIndex (guard-refused on the pre-existing column):\n{combined}"
    );
    assert_no_secret_leak(&mdiff_out, &mdiff_err);
}

/// Fix 2 (fail-closed floor for Postgres DOMAIN columns): a column typed on a
/// domain (`CREATE DOMAIN email_dom AS text CHECK (…)`) reports its base type in
/// `udt_name` (`text`) but names the domain in `information_schema.columns`. Pull
/// must preserve the domain verbatim as an `Opaque` type carrying the domain name
/// — NOT flatten it to `Text`, which would silently drop the domain's identity and
/// validation on recreation. Re-pull is byte-stable and doctor is clean.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_preserves_domain_column_as_opaque() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_domain_opaque_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_contacts",
        "CREATE DOMAIN email_dom AS text CHECK (VALUE LIKE '%@%');\n\
         CREATE TABLE contacts (id BIGSERIAL PRIMARY KEY, email email_dom NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE contacts; DROP DOMAIN email_dom;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the domain column is preserved as Opaque{pg_type: "email_dom"}, not Text.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"name\": \"email\""),
        "the domain column is present: {snap}"
    );
    assert!(
        snap.contains("\"Opaque\"") && snap.contains("\"pg_type\": \"email_dom\""),
        "the domain column is preserved as Opaque carrying the domain name (NOT Text): {snap}"
    );

    // Re-pull is byte-for-byte stable.
    let snap_before = snap;
    let (pull2_out, pull2_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&pull2_out, &pull2_err);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after second pull");
    assert_eq!(
        snap_before, snap_after,
        "a re-pull of the same database must be byte-for-byte stable (no domain drift)"
    );

    // doctor agrees the DB matches the pulled baseline.
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the domain column:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// Fail-closed floor for length-limited character types: a `VARCHAR(32)` /
/// `CHAR(2)` column reports `udt_name` `varchar` / `bpchar`, which the shared
/// mapped surface would otherwise flatten to `Text`, silently dropping the
/// DB-enforced length limit (recreation would emit an unconstrained `TEXT`). Pull
/// must preserve the length verbatim as `Opaque` carrying `varchar(32)` /
/// `char(2)`, while an unbounded `TEXT` column stays `Text`. Re-pull is byte-stable
/// and doctor is clean.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_preserves_length_limited_char_columns_as_opaque() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_varchar_opaque_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_labels",
        "CREATE TABLE labels (id BIGSERIAL PRIMARY KEY, code VARCHAR(32) NOT NULL, \
         kind CHAR(2) NOT NULL, note TEXT, created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE labels;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the length-limited columns are preserved as Opaque{varchar(32)} /
    // Opaque{char(2)}, never flattened to Text; the unbounded `note` stays Text.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"name\": \"code\"") && snap.contains("\"pg_type\": \"varchar(32)\""),
        "VARCHAR(32) is preserved as Opaque varchar(32) (NOT Text): {snap}"
    );
    assert!(
        snap.contains("\"name\": \"kind\"") && snap.contains("\"pg_type\": \"char(2)\""),
        "CHAR(2) is preserved as Opaque char(2) (NOT Text): {snap}"
    );
    // `note` (unbounded TEXT) must map to Text, not Opaque.
    let note_idx = snap
        .find("\"name\": \"note\"")
        .expect("note column present");
    let note_slice = &snap[note_idx..];
    assert!(
        note_slice.contains("\"Text\""),
        "the unbounded TEXT column stays Text: {snap}"
    );

    // Re-pull is byte-for-byte stable.
    let snap_before = snap;
    let (pull2_out, pull2_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&pull2_out, &pull2_err);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after second pull");
    assert_eq!(
        snap_before, snap_after,
        "a re-pull of the same database must be byte-for-byte stable (no varchar-length drift)"
    );

    // doctor agrees the DB matches the pulled baseline.
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the length-limited columns:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// Fix 3 (fail-closed floor for cross-schema FK targets): a FK referencing a table
/// outside `public` (`REFERENCES auth.accounts(id)`) must be recorded
/// schema-qualified (`auth.accounts`) so recreation emits `REFERENCES
/// auth.accounts(id)` — never a wrong bare `REFERENCES accounts(id)` pointing at a
/// (nonexistent) `public.accounts`. Re-pull is byte-stable and doctor is clean.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_qualifies_cross_schema_foreign_key_target() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_cross_schema_fk_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_sessions",
        "CREATE SCHEMA auth;\n\
         CREATE TABLE auth.accounts (id BIGSERIAL PRIMARY KEY);\n\
         CREATE TABLE public.sessions (id BIGSERIAL PRIMARY KEY, account_id BIGINT REFERENCES auth.accounts(id), created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE public.sessions; DROP TABLE auth.accounts; DROP SCHEMA auth;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the FK target is recorded schema-qualified as `auth.accounts`.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"table\": \"auth.accounts\""),
        "the cross-schema FK target is recorded schema-qualified: {snap}"
    );
    assert!(
        !snap.contains("\"table\": \"accounts\""),
        "the FK target must NOT be recorded as a bare (wrong) `accounts`: {snap}"
    );

    // Re-pull is byte-for-byte stable.
    let snap_before = snap;
    let (pull2_out, pull2_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&pull2_out, &pull2_err);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after second pull");
    assert_eq!(
        snap_before, snap_after,
        "a re-pull of the same database must be byte-for-byte stable (no cross-schema FK drift)"
    );

    // doctor agrees the DB matches the pulled baseline.
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the cross-schema FK:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
}

/// Fix 4 (preserve CHECK constraints): a real table-level `CHECK (price >= 0)`
/// must be pulled into the snapshot's `Table.checks` — previously `checks` was
/// always left empty, silently dropping the constraint on recreation. The column
/// `NOT NULL` is not a check (it is `attnotnull`), so it never appears here.
/// Re-pull is byte-stable and doctor is clean.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn schema_pull_preserves_check_constraints() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/postgres");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_tmp, project) = fresh_project("pull_check_constraint_app");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_create_products",
        "CREATE TABLE products (id BIGSERIAL PRIMARY KEY, price INTEGER NOT NULL CHECK (price >= 0), created_at TIMESTAMP NOT NULL DEFAULT NOW());",
        "DROP TABLE products;",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    // Pull: the CHECK constraint is preserved in the products table's checks.
    let (out, err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&out, &err);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"name\": \"products\""),
        "products pulled: {snap}"
    );
    assert!(
        snap.contains("\"checks\"") && snap.contains("price >= 0"),
        "the CHECK constraint predicate is preserved on pull (not dropped): {snap}"
    );

    // Re-pull is byte-for-byte stable.
    let snap_before = snap;
    let (pull2_out, pull2_err) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert_no_secret_leak(&pull2_out, &pull2_err);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("snapshot after second pull");
    assert_eq!(
        snap_before, snap_after,
        "a re-pull of the same database must be byte-for-byte stable (no check drift)"
    );

    // doctor agrees the DB matches the pulled baseline.
    let (doc_out, doc_err) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the DB matches the snapshot with the CHECK constraint:\n{doc_out}"
    );
    assert_no_secret_leak(&doc_out, &doc_err);
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
