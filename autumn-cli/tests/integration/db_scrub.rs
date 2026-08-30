//! Integration tests for `autumn db scrub` (issue #1602).
//!
//! The headline test is the AC #6 round-trip: seed known PII → `autumn db
//! backup` → `autumn db scrub --artifact …` into a separate staging database →
//! assert **zero** occurrences of any original value anywhere in the result.
//!
//! Requires Docker (via testcontainers) AND `pg_dump`/`pg_restore` on `PATH`, so
//! every test here is `#[ignore]`d:
//!
//! ```text
//! cargo test -p autumn-cli --test cli_tests -- --ignored --nocapture db_scrub
//! ```
//!
//! `scrubbed_database_migrates_clean_and_boots_the_app` additionally compiles
//! and boots a generated app, so it also needs the `diesel` CLI and runs in the
//! generator-conformance Postgres gate rather than the general Docker sweep.
//!
//! Each test drives the real `autumn` binary as a subprocess with a
//! per-child working directory and environment (never mutating this process's
//! cwd or env), so it has no process-wide side effects and lives in the
//! consolidated test binary per the workspace test-layout guidelines.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio_postgres::{Client, NoTls};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// The PII values seeded before every scrub. **None** of these may survive
/// anywhere in the scrubbed database.
const SECRETS: &[&str] = &[
    "alice@real-corp.example",
    "bob@real-corp.example",
    "Alice Realname",
    "Bob Realname",
    "+14155550101",
    "lives at 42 Real Street",
    "tok_REALSECRET_alice",
    "tok_REALSECRET_bob",
    "my real secret comment",
];

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

async fn connect(url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("connect to {url:?} failed: {e}"));
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// The fixture schema: a `users` table carrying five different shapes of PII
/// plus a deliberately-safe `role`, a `comments` child table whose foreign key
/// must survive the scrub intact, and a framework-owned `autumn_jobs` table
/// whose queued payload embeds PII the column classification never sees
/// (introspection filters `autumn_*` out of the classified universe).
const FIXTURE_SCHEMA: &str = "\
    CREATE TABLE users ( \
        id BIGSERIAL PRIMARY KEY, \
        email TEXT NOT NULL UNIQUE, \
        full_name TEXT NOT NULL, \
        phone VARCHAR(32), \
        bio TEXT, \
        api_token TEXT NOT NULL, \
        role TEXT NOT NULL, \
        created_at TIMESTAMP NOT NULL DEFAULT NOW() \
    ); \
    CREATE TABLE comments ( \
        id BIGSERIAL PRIMARY KEY, \
        user_id BIGINT NOT NULL REFERENCES users(id), \
        body TEXT NOT NULL, \
        created_at TIMESTAMP NOT NULL DEFAULT NOW() \
    );";

/// Framework-owned tables, applied only by the Docker-only tests. The
/// app-booting test must not create these itself: `autumn migrate` runs the
/// framework's own migrations, which own `autumn_jobs` with a different shape.
///
/// `autumn_sync_rows` (a real built-in payload carrier) is opted into
/// `[framework] purge`; `autumn_jobs` is another one left un-purged, so the
/// warning path is exercised too. Both are minimal stand-ins for the real
/// framework tables — the scrub only ever reads their names.
const FIXTURE_FRAMEWORK_TABLES: &str = "\
    CREATE TABLE autumn_sync_rows ( \
        id BIGSERIAL PRIMARY KEY, \
        payload TEXT NOT NULL \
    ); \
    INSERT INTO autumn_sync_rows (payload) \
        VALUES ('{\"to\": \"alice@real-corp.example\"}'); \
    CREATE TABLE autumn_jobs ( \
        id BIGSERIAL PRIMARY KEY, \
        args TEXT NOT NULL \
    ); \
    INSERT INTO autumn_jobs (args) VALUES ('{\"note\": \"nothing sensitive\"}');";

const FIXTURE_ROWS: &str = "\
    INSERT INTO users (email, full_name, phone, bio, api_token, role) VALUES \
        ('alice@real-corp.example', 'Alice Realname', '+14155550101', \
         'lives at 42 Real Street', 'tok_REALSECRET_alice', 'admin'), \
        ('bob@real-corp.example', 'Bob Realname', NULL, NULL, \
         'tok_REALSECRET_bob', 'member'); \
    INSERT INTO comments (user_id, body) \
        SELECT id, 'my real secret comment' FROM users;";

/// The per-app declaration. `api_token` is deliberately absent: it carries
/// `#[encrypted]` in the model and must be classified with no declaration at
/// all. `comments` is absent entirely: it is registered with the GDPR anonymize
/// strategy.
const SCRUB_TOML: &str = r#"
[defaults]
safe_columns = ["id", "created_at"]

[tables.users]
safe = ["role"]

[tables.users.pii]
email = "email"
full_name = "name"
phone = "phone"
bio = "redact"

# Framework-owned tables are not column-classified; this app opts into
# emptying its outbox, whose payloads embed customer addresses.
[framework]
purge = ["autumn_sync_rows"]
"#;

/// Write the project files the scrub reads its automatic classification from:
/// a model with an `#[encrypted]` column and a GDPR anonymize registration.
fn write_project_sources(dir: &Path) {
    let models = dir.join("src").join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::fs::write(
        models.join("user.rs"),
        r"
use autumn_web::model;

#[model]
pub struct User {
    #[id]
    pub id: i64,
    pub email: String,
    pub full_name: String,
    /// At-rest encrypted — PII by construction, so `autumn db scrub`
    /// classifies it without any declaration.
    #[encrypted]
    pub api_token: String,
}
",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("state.rs"),
        r#"
use autumn_web::gdpr::{GdprRegistry, ModelRegistration};

pub fn gdpr_registry() -> GdprRegistry {
    GdprRegistry::new()
        .register(ModelRegistration::hard_delete("sessions"))
        .register(ModelRegistration::anonymize("comments"))
}
"#,
    )
    .unwrap();
    write_encryption_credentials(dir);
}

/// Give the fixture app real `active_record_encryption` credentials.
///
/// `User::api_token` is `#[encrypted]`, and a scrub REFUSES to rewrite an
/// encrypted column without the target's key — writing plaintext there would
/// make every later repository read of that row fail as malformed ciphertext.
/// So provisioning the key is part of the drill these tests cover, not
/// incidental setup, and it goes through the same `autumn credentials edit`
/// an operator would use rather than reaching past the CLI.
///
/// Both profiles get a key: the tests that exercise the production-profile
/// refusal must fail on THAT guard, not on a missing key.
fn write_encryption_credentials(dir: &Path) {
    let editor = dir.join("fixture-editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\ncat > \"$1\" <<'TOML'\n[active_record_encryption]\n\
         primary_key = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n\
         deterministic_key = \"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210\"\n\
         key_derivation_salt = \"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\"\n\
         TOML\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    for profile in ["dev", "production"] {
        let output = Command::new(autumn_bin())
            .args(["credentials", "edit", "--env", profile])
            .current_dir(dir)
            .env("EDITOR", &editor)
            .env("VISUAL", &editor)
            .output()
            .expect("run `autumn credentials edit`");
        assert!(
            output.status.success(),
            "provisioning {profile} credentials failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

/// Every occurrence of `needle` anywhere in any character-typed column of any
/// non-system table, as `table.column` labels. The sweep is deliberately
/// schema-driven rather than a hand-written column list, so a column added to
/// the fixture cannot slip past the assertion.
async fn occurrences(client: &Client, needle: &str) -> Vec<String> {
    let columns = client
        .query(
            "SELECT table_name, column_name FROM information_schema.columns \
             WHERE table_schema = 'public' \
             ORDER BY table_name, ordinal_position",
            &[],
        )
        .await
        .expect("read column catalog");

    let mut hits = Vec::new();
    for row in columns {
        let table: String = row.get(0);
        let column: String = row.get(1);
        let sql = format!(
            "SELECT count(*) FROM \"{}\" WHERE position($1 in \"{}\"::text) > 0",
            table.replace('"', "\"\""),
            column.replace('"', "\"\"")
        );
        let count: i64 = client
            .query_one(&sql, &[&needle])
            .await
            .unwrap_or_else(|e| panic!("scanning {table}.{column} failed: {e}"))
            .get(0);
        if count > 0 {
            hits.push(format!("{table}.{column} ({count} row(s))"));
        }
    }
    hits
}

async fn assert_no_secrets_survive(client: &Client) {
    let mut report = String::new();
    for secret in SECRETS {
        let hits = occurrences(client, secret).await;
        if !hits.is_empty() {
            let _ = writeln!(report, "  {secret:?} still present in: {}", hits.join(", "));
        }
    }
    assert!(
        report.is_empty(),
        "scrubbed database still contains original PII:\n{report}"
    );
}

async fn start_postgres() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
    u16,
) {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres testcontainer — is Docker running?");
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    (container, host, port)
}

// ─── AC #6: the round trip ──────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers) and pg_dump/pg_restore on PATH"]
#[allow(clippy::too_many_lines)]
async fn scrub_round_trip_leaves_zero_original_values() {
    let (_pg, host, port) = start_postgres().await;
    let admin_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let source_url = format!("postgres://postgres:postgres@{host}:{port}/scrub_source");
    let staging_url = format!("postgres://postgres:postgres@{host}:{port}/scrub_staging");

    // ── Two databases: the "production" source and the staging target ───────
    let admin = connect(&admin_url).await;
    admin
        .batch_execute("CREATE DATABASE scrub_source; CREATE DATABASE scrub_staging;")
        .await
        .unwrap();

    let source = connect(&source_url).await;
    source.batch_execute(FIXTURE_SCHEMA).await.unwrap();
    source
        .batch_execute(FIXTURE_FRAMEWORK_TABLES)
        .await
        .unwrap();
    source.batch_execute(FIXTURE_ROWS).await.unwrap();

    // Sanity: the seeded values really are there to begin with, so a passing
    // post-scrub assertion cannot be vacuous.
    for secret in SECRETS {
        assert!(
            !occurrences(&source, secret).await.is_empty(),
            "fixture must actually contain {secret:?} before the scrub"
        );
    }

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_project_sources(dir);

    // ── Back up the source ──────────────────────────────────────────────────
    let source_envs = [("AUTUMN_DATABASE__URL", source_url.as_str())];
    run_autumn_ok(dir, &["db", "backup"], &source_envs);
    let run_dir = newest_run_dir(&dir.join("backups").join("dev"));
    assert!(
        run_dir.join("manifest.json").is_file() && run_dir.join("control.dump").is_file(),
        "the backup must have produced a real artifact, not just a log line: {}",
        run_dir.display()
    );

    // ── Fail-closed: with no declaration the scrub refuses ──────────────────
    let staging_envs = [("AUTUMN_DATABASE__URL", staging_url.as_str())];
    let (_o, refusal) = run_autumn_fail(
        dir,
        &["db", "scrub", "--artifact", run_dir.to_str().unwrap()],
        &staging_envs,
    );
    assert!(
        refusal.contains("neither PII-classified nor declared safe"),
        "an undeclared schema must be refused: {refusal}"
    );
    assert!(
        refusal.contains("users.bio") && refusal.contains("users.role"),
        "the refusal must list the unclassified columns: {refusal}"
    );
    assert!(
        refusal.contains("[tables.users.pii]"),
        "the refusal must print a paste-ready stanza: {refusal}"
    );
    // The restore has already run by the time classification can see the schema,
    // so the refusal must say — loudly — that the target now holds raw data.
    assert!(
        refusal.contains("ALREADY RESTORED") && refusal.contains("UNSCRUBBED"),
        "a post-restore refusal must warn that the target holds unscrubbed data: {refusal}"
    );

    // ── Declare, then scrub the restored copy ───────────────────────────────
    std::fs::write(dir.join("scrub.toml"), SCRUB_TOML).unwrap();
    let (_o, scrub_err) = run_autumn_ok(
        dir,
        &["db", "scrub", "--artifact", run_dir.to_str().unwrap()],
        &staging_envs,
    );
    assert!(
        scrub_err.contains("Scrub complete"),
        "scrub should complete: {scrub_err}"
    );
    // The two automatic classifications must be visible in the report.
    assert!(
        scrub_err.contains("users.api_token") && scrub_err.contains("#[encrypted]"),
        "an #[encrypted] column must be scrubbed without a declaration: {scrub_err}"
    );
    assert!(
        scrub_err.contains("comments.body") && scrub_err.contains("gdpr:anonymize"),
        "a GDPR-anonymize table must be scrubbed without a declaration: {scrub_err}"
    );
    // Framework-owned tables are outside the classified universe: the opted-in
    // one is emptied, and the one left alone is warned about by name.
    assert!(
        scrub_err.contains("autumn_sync_rows") && scrub_err.contains("emptied"),
        "the opted-in framework table must be emptied: {scrub_err}"
    );
    assert!(
        scrub_err.contains("autumn_jobs") && scrub_err.contains("NOT scrubbed"),
        "an un-purged framework payload table must be warned about: {scrub_err}"
    );

    // ── AC #1/#6: zero original values survive ──────────────────────────────
    let staging = connect(&staging_url).await;
    assert_no_secrets_survive(&staging).await;

    // ── AC #4: the copy is still a working database ─────────────────────────
    let user_rows: i64 = staging
        .query_one("SELECT count(*) FROM users", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(user_rows, 2, "a scrub anonymizes rows, it never drops them");

    let distinct_emails: i64 = staging
        .query_one("SELECT count(DISTINCT email) FROM users", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(distinct_emails, 2, "the UNIQUE email column stays unique");

    let outbox_rows: i64 = staging
        .query_one("SELECT count(*) FROM autumn_sync_rows", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        outbox_rows, 0,
        "the purged framework table must be empty after the scrub"
    );

    let orphans: i64 = staging
        .query_one(
            "SELECT count(*) FROM comments c \
             LEFT JOIN users u ON u.id = c.user_id WHERE u.id IS NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(orphans, 0, "foreign keys must still resolve after a scrub");

    let nulls: i64 = staging
        .query_one(
            "SELECT count(*) FROM users WHERE email IS NULL OR full_name IS NULL \
             OR api_token IS NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(nulls, 0, "NOT NULL columns must still hold values");

    // A NULL in the source stays NULL — a scrub anonymizes values, it does not
    // invent them.
    let preserved_nulls: i64 = staging
        .query_one(
            "SELECT count(*) FROM users WHERE phone IS NULL AND bio IS NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(preserved_nulls, 1, "Bob's NULL phone/bio must stay NULL");

    // An explicitly-safe column keeps its real value.
    let roles: Vec<String> = staging
        .query("SELECT role FROM users ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(roles, vec!["admin".to_owned(), "member".to_owned()]);

    // The replacement email is still a syntactically valid, undeliverable one.
    let emails: Vec<String> = staging
        .query("SELECT email FROM users ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    for email in &emails {
        assert!(
            email.starts_with("scrubbed+") && email.ends_with("@example.invalid"),
            "unexpected replacement address: {email}"
        );
    }

    // ── The source database was never touched ───────────────────────────────
    let source_email: String = source
        .query_one("SELECT email FROM users ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        source_email, "alice@real-corp.example",
        "the scrub must not reach back into the database the artifact came from"
    );
}

/// AC #1's second input shape: scrubbing a resolved database URL in place, with
/// no artifact involved.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn scrub_anonymizes_a_resolved_database_url_in_place() {
    let (_pg, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let client = connect(&url).await;
    client.batch_execute(FIXTURE_SCHEMA).await.unwrap();
    client
        .batch_execute(FIXTURE_FRAMEWORK_TABLES)
        .await
        .unwrap();
    client.batch_execute(FIXTURE_ROWS).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_project_sources(dir);
    std::fs::write(dir.join("scrub.toml"), SCRUB_TOML).unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    // `--check` proves the classification is complete and writes nothing.
    let (_o, check_err) = run_autumn_ok(dir, &["db", "scrub", "--check"], &envs);
    assert!(
        check_err.contains("Every column is classified"),
        "check should confirm a complete classification: {check_err}"
    );
    assert!(
        !occurrences(&client, "Alice Realname").await.is_empty(),
        "--check must not write anything"
    );
    let outbox_before: i64 = client
        .query_one("SELECT count(*) FROM autumn_sync_rows", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(outbox_before, 1, "--check must not empty a purged table");

    // `--dry-run` prints the statement and still writes nothing.
    let (_o, dry_err) = run_autumn_ok(dir, &["db", "scrub", "--dry-run"], &envs);
    assert!(
        dry_err.contains("UPDATE \"users\" SET"),
        "dry run should print the statement: {dry_err}"
    );
    assert!(
        !occurrences(&client, "Alice Realname").await.is_empty(),
        "--dry-run must not write anything"
    );

    run_autumn_ok(dir, &["db", "scrub"], &envs);
    assert_no_secrets_survive(&client).await;
}

/// AC #3, at the sharpest edge: adding a column to a schema whose declaration
/// was previously complete must break the scrub, not slip through.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_newly_added_column_breaks_a_previously_passing_scrub() {
    let (_pg, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let client = connect(&url).await;
    client.batch_execute(FIXTURE_SCHEMA).await.unwrap();
    client
        .batch_execute(FIXTURE_FRAMEWORK_TABLES)
        .await
        .unwrap();
    client.batch_execute(FIXTURE_ROWS).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_project_sources(dir);
    std::fs::write(dir.join("scrub.toml"), SCRUB_TOML).unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    run_autumn_ok(dir, &["db", "scrub", "--check"], &envs);

    client
        .batch_execute("ALTER TABLE users ADD COLUMN ssn TEXT;")
        .await
        .unwrap();

    // `--check` is the gate the docs tell teams to put in CI, so it is the one
    // that has to go red — a plain `db scrub` failing would not prove it.
    let (check_out, check_err) = run_autumn_fail(dir, &["db", "scrub", "--check"], &envs);
    assert!(
        check_err.contains("users.ssn"),
        "--check must name the undeclared column: {check_err}"
    );
    assert!(
        check_out.contains("[tables.users.pii]") && check_out.contains("ssn = \"auto\""),
        "the paste-ready stanza must go to stdout so it can be appended to \
         scrub.toml: {check_out}"
    );

    let (_o, stderr) = run_autumn_fail(dir, &["db", "scrub"], &envs);
    assert!(
        stderr.contains("users.ssn"),
        "the new column must be named in the refusal: {stderr}"
    );
    assert!(
        !occurrences(&client, "Alice Realname").await.is_empty(),
        "a refused scrub must not have written anything"
    );
}

/// AC #3's other rot mode: a renamed column leaves a stale declaration behind.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_renamed_column_is_reported_as_a_stale_declaration() {
    let (_pg, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let client = connect(&url).await;
    client.batch_execute(FIXTURE_SCHEMA).await.unwrap();
    client
        .batch_execute(FIXTURE_FRAMEWORK_TABLES)
        .await
        .unwrap();
    client
        .batch_execute("ALTER TABLE users RENAME COLUMN bio TO about;")
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_project_sources(dir);
    std::fs::write(dir.join("scrub.toml"), SCRUB_TOML).unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_o, stderr) = run_autumn_fail(dir, &["db", "scrub"], &envs);
    assert!(
        stderr.contains("users.bio") && stderr.contains("drifted"),
        "a stale declaration must be reported: {stderr}"
    );
}

/// AC #5: the production profile guard, identical in shape to `autumn db drop`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn scrub_refuses_a_production_profile_without_force() {
    let (_pg, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let client = connect(&url).await;
    client.batch_execute(FIXTURE_SCHEMA).await.unwrap();
    client
        .batch_execute(FIXTURE_FRAMEWORK_TABLES)
        .await
        .unwrap();
    client.batch_execute(FIXTURE_ROWS).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_project_sources(dir);
    std::fs::write(dir.join("scrub.toml"), SCRUB_TOML).unwrap();

    let prod_envs = [
        ("AUTUMN_DATABASE__URL", url.as_str()),
        ("AUTUMN_ENV", "prod"),
    ];
    let (_o, stderr) = run_autumn_fail(dir, &["db", "scrub"], &prod_envs);
    assert!(
        stderr.contains("Refusing to scrub") && stderr.contains("prod"),
        "prod must be refused without --force: {stderr}"
    );
    assert!(
        !stderr.contains("postgres://"),
        "the refusal must never print the connection URL: {stderr}"
    );
    assert!(
        !occurrences(&client, "Alice Realname").await.is_empty(),
        "a refused scrub must not have written anything"
    );

    // With --force it proceeds (the operator has said they mean it).
    run_autumn_ok(dir, &["db", "scrub", "--force"], &prod_envs);
    assert_no_secrets_survive(&client).await;
}

/// AC #1's artifact output: `--output` re-dumps the scrubbed database, so the
/// artifact handed to a teammate is itself scrubbed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers) and pg_dump/pg_restore on PATH"]
async fn scrub_output_writes_a_scrubbed_artifact() {
    let (_pg, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let client = connect(&url).await;
    client.batch_execute(FIXTURE_SCHEMA).await.unwrap();
    client
        .batch_execute(FIXTURE_FRAMEWORK_TABLES)
        .await
        .unwrap();
    client.batch_execute(FIXTURE_ROWS).await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_project_sources(dir);
    std::fs::write(dir.join("scrub.toml"), SCRUB_TOML).unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    run_autumn_ok(dir, &["db", "scrub", "--output", "scrubbed"], &envs);

    let run_dir = newest_run_dir(&dir.join("scrubbed").join("dev"));
    let dump = std::fs::read(run_dir.join("control.dump")).expect("scrubbed artifact");
    // A custom-format dump is compressed, so search the plain-text listing
    // instead: `pg_restore` is on PATH for this test by construction.
    let listing = Command::new("pg_restore")
        .arg("--file")
        .arg(run_dir.join("plain.sql"))
        .arg(run_dir.join("control.dump"))
        .status()
        .expect("pg_restore to plain SQL");
    assert!(listing.success(), "pg_restore --file should succeed");
    let plain = std::fs::read_to_string(run_dir.join("plain.sql")).unwrap();
    assert!(!dump.is_empty(), "the artifact must not be empty");
    for secret in SECRETS {
        assert!(
            !plain.contains(secret),
            "the scrubbed artifact still contains {secret:?}"
        );
    }
}

/// The newest run directory under a backup profile root.
fn newest_run_dir(profile_root: &Path) -> PathBuf {
    let mut runs: Vec<PathBuf> = std::fs::read_dir(profile_root)
        .unwrap_or_else(|e| panic!("reading {}: {e}", profile_root.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    runs.sort();
    runs.pop()
        .unwrap_or_else(|| panic!("no backup run under {}", profile_root.display()))
}

// ─── AC #4: the app boots against the scrubbed database ─────────────────────

/// RAII guard that kills the child server on drop (even on test failure).
/// A running app plus the files its output is redirected to.
struct ServerGuard {
    child: std::process::Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl ServerGuard {
    /// Everything the app has written, for a failure message.
    ///
    /// The output goes to FILES rather than pipes, which is load-bearing rather
    /// than incidental. Reading a pipe with `read_to_string` blocks until EOF,
    /// and EOF arrives only once every writer has closed it. A healthy server
    /// never closes its pipes, so draining one would block until the workflow's
    /// 90-minute timeout — the success path would hang instead of passing,
    /// which is precisely the path this test exists to prove. The failure paths
    /// hid it: a child that already died has reached EOF, so the read returns.
    ///
    /// Killing the child first is not enough either. The app is spawned through
    /// `cargo run`, so the server is a GRANDchild that inherits these handles:
    /// killing `cargo` leaves the server holding the pipe open. Files sidestep
    /// the whole question — no EOF to wait for, no pipe buffer to fill, and the
    /// output survives however the process tree happens to die.
    fn drain(&mut self) -> String {
        let read = |path: &PathBuf| std::fs::read_to_string(path).unwrap_or_default();
        format!("{}{}", read(&self.stdout), read(&self.stderr))
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn patch_generated_cargo_toml(project_dir: &Path) {
    let manifest = project_dir.join("Cargo.toml");
    let mut content = std::fs::read_to_string(&manifest).expect("read generated Cargo.toml");
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web = workspace_root.join("autumn");
    write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\n",
        autumn_web.display().to_string().replace('\\', "/")
    )
    .expect("write to String is infallible");
    std::fs::write(&manifest, content).expect("patch generated Cargo.toml");
}

/// The one criterion the Docker round-trip cannot cover on its own: after the
/// FULL drill (seed → `db backup` → `db scrub --artifact` → zero original
/// values), `autumn migrate` reports a clean status against the scrubbed
/// database and a real generated app boots against it and answers `GET /health`
/// with 200.
///
/// Scaffolds, migrates, seeds, scrubs, re-checks migrations and then compiles
/// and boots the app, so it needs the `diesel` CLI on `PATH` in addition to
/// Docker — it runs in the generator-conformance Postgres gate.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: compiles and boots a generated app; needs Docker + diesel CLI"]
#[allow(clippy::too_many_lines)]
async fn scrubbed_database_migrates_clean_and_boots_the_app() {
    let (_pg, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    // ── Scaffold a real app ─────────────────────────────────────────────────
    let tmp = tempfile::tempdir().unwrap();
    let new_output = Command::new(autumn_bin())
        .args(["new", "scrub-app"])
        .current_dir(tmp.path())
        .output()
        .expect("run `autumn new`");
    assert!(
        new_output.status.success(),
        "autumn new failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&new_output.stdout),
        String::from_utf8_lossy(&new_output.stderr),
    );
    let project = tmp.path().join("scrub-app");
    patch_generated_cargo_toml(&project);
    write_project_sources(&project);
    std::fs::write(project.join("scrub.toml"), SCRUB_TOML).unwrap();

    // ── A migration carrying the PII fixture ────────────────────────────────
    let migration = project
        .join("migrations")
        .join("20260101000000_create_scrub_fixture");
    std::fs::create_dir_all(&migration).unwrap();
    std::fs::write(migration.join("up.sql"), FIXTURE_SCHEMA).unwrap();
    std::fs::write(
        migration.join("down.sql"),
        "DROP TABLE comments; DROP TABLE users;",
    )
    .unwrap();

    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    run_autumn_ok(&project, &["migrate"], &envs);

    let client = connect(&url).await;
    client.batch_execute(FIXTURE_ROWS).await.unwrap();

    // ── The full AC #6 chain in one test: backup → scrub the restored copy ──
    // Going through an artifact here (rather than scrubbing the live URL) is
    // what makes this test cover the whole documented drill end to end: seed →
    // backup → restore → scrub → assert → boot.
    run_autumn_ok(&project, &["db", "backup"], &envs);
    let run_dir = newest_run_dir(&project.join("backups").join("dev"));
    run_autumn_ok(
        &project,
        &["db", "scrub", "--artifact", run_dir.to_str().unwrap()],
        &envs,
    );
    assert_no_secrets_survive(&client).await;

    // ── `autumn migrate` reports a clean status against the scrubbed DB ─────
    let (_o, migrate_err) = run_autumn_ok(&project, &["migrate"], &envs);
    // Assert the POSITIVE signal `autumn migrate` actually prints. A negative
    // assertion on a string the command never emits would pass on empty output.
    assert!(
        migrate_err.contains("Migrations are already up to date.")
            || migrate_err.contains("Migrations applied successfully."),
        "migrations must report a clean status against the scrubbed database: {migrate_err}"
    );

    // ── The app boots against it and answers GET /health ────────────────────
    let build = Command::new("cargo")
        .args(["build"])
        .current_dir(&project)
        .output()
        .expect("cargo build");
    assert!(
        build.status.success(),
        "cargo build failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let app_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener.local_addr().unwrap().port()
    };
    let stdout_path = project.join("app-stdout.log");
    let stderr_path = project.join("app-stderr.log");
    let child = Command::new("cargo")
        .args(["run"])
        .current_dir(&project)
        .env("AUTUMN_SERVER__PORT", app_port.to_string())
        .env("AUTUMN_DATABASE__URL", &url)
        .stdout(Stdio::from(
            std::fs::File::create(&stdout_path).expect("create the app stdout log"),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("create the app stderr log"),
        ))
        .spawn()
        .expect("spawn the generated app");
    let mut guard = ServerGuard {
        child,
        stdout: stdout_path,
        stderr: stderr_path,
    };

    let base = format!("http://127.0.0.1:{app_port}");
    let client_http = reqwest::Client::new();
    let mut status = None;
    for _ in 0..60 {
        // A child that died at boot must fail fast with ITS OWN output, not
        // after 30 s of silence.
        if let Some(exit) = guard.child.try_wait().expect("poll the app process") {
            let output = guard.drain();
            panic!("the generated app exited before serving: {exit}\n{output}");
        }
        if let Ok(resp) = client_http.get(format!("{base}/health")).send().await {
            status = Some(resp.status().as_u16());
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let output = guard.drain();
    assert_eq!(
        status,
        Some(200),
        "the app must boot against the scrubbed database and answer GET /health \
         with 200\n{output}"
    );
}
