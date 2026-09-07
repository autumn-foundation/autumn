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
/// Every profile the tests drive gets a key, so a test that exercises the
/// production-profile refusal fails on THAT guard rather than on a missing key.
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

    // Every profile the tests actually drive, not the ones that sound right:
    // `AUTUMN_ENV=prod` is what the production-refusal test sets, and `dev` is
    // the CLI's default when no profile is given. Provisioning "production"
    // instead of "prod" left the `--force` leg of that test failing on a
    // missing key rather than exercising the guard it exists to check.
    for profile in ["dev", "test", "prod", "production"] {
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
    // One statement per call, deliberately: `batch_execute` uses the simple
    // query protocol, and Postgres wraps a MULTI-statement simple query in an
    // implicit transaction block — where `CREATE DATABASE` is rejected with
    // `25001 PreventInTransactionBlock`. Sent singly, each runs outside one.
    admin
        .batch_execute("CREATE DATABASE scrub_source")
        .await
        .unwrap();
    admin
        .batch_execute("CREATE DATABASE scrub_staging")
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
    let (stanza, refusal) = run_autumn_fail(
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
    // On STDOUT, not stderr: `run()` prints the stanza with `println!` so that
    // `autumn db scrub --check 2>/dev/null >> scrub.toml` appends a valid
    // fragment rather than a wall of interleaved prose. Asserting it on stderr
    // asserted against the one stream it is deliberately kept out of.
    assert!(
        stanza.contains("[tables.users.pii]"),
        "the refusal must print a paste-ready stanza on stdout: {stanza}"
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
        check_err.contains("Every column in `public` is classified"),
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
    // Schema-qualified on purpose — do not "simplify" this to a bare
    // `UPDATE "users"`. Every catalog read that built the plan is scoped to
    // `public`, so the writes are too: under a database- or role-level
    // `search_path` (which Autumn supports for tenant schemas) a bare
    // identifier would resolve to a DIFFERENT table than the one classified,
    // leaving the classified rows unscrubbed. This assertion pins that.
    assert!(
        dry_err.contains("UPDATE \"public\".\"users\" SET"),
        "dry run should print the schema-qualified statement: {dry_err}"
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

    // A sample is refused by the same guard, on the same terms — the guard runs
    // before anything reads the schema, so no flag can slip past it.
    let (_o, sample_refusal) =
        run_autumn_fail(dir, &["db", "scrub", "--sample", "users=1%"], &prod_envs);
    assert!(
        sample_refusal.contains("Refusing to scrub") && sample_refusal.contains("prod"),
        "a sampled scrub must be refused on prod too: {sample_refusal}"
    );
    let survivors: i64 = client
        .query_one("SELECT count(*) FROM users", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        survivors, 2,
        "a refused sample must not have deleted anything"
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
    /// `cargo run`, so the server is a grandchild that inherits these handles:
    /// killing `cargo` leaves the server holding the pipe open. Files sidestep
    /// the whole question — no EOF to wait for, no pipe buffer to fill, and the
    /// output survives however the process tree happens to die.
    fn drain(&self) -> String {
        let read = |path: &PathBuf| std::fs::read_to_string(path).unwrap_or_default();
        format!("{}{}", read(&self.stdout), read(&self.stderr))
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        // The server is a GRANDCHILD (spawned through `cargo run`), so killing
        // the child leaves it running — and, since #1636, still connected to
        // the database the next leg of this test compacts with a `VACUUM FULL`.
        // That takes an ACCESS EXCLUSIVE lock, so an orphan holding an open
        // transaction would stall it. The child is spawned into its own process
        // group, so the whole group goes down together.
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .arg("--")
                .arg(format!("-{}", self.child.id()))
                .status();
        }
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
/// Docker — it runs in the generator-conformance Postgres gate. It then repeats
/// the last two steps with `--sample` (issue #1636), which is the same
/// criterion for a SAMPLED database and reuses the one expensive compile.
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
    assert_app_serves_health(&project, &url).await;

    // ── #1636: the same drill, now with a sample ────────────────────────────
    //
    // Re-scrubbing the (already scrubbed) database with `--sample` is the one
    // AC the Docker sweep cannot cover either: a SAMPLED database must also
    // migrate clean and boot. The app is already compiled by this point, so
    // this second leg costs a boot rather than a build.
    run_autumn_ok(&project, &["db", "scrub", "--sample", "users=50%"], &envs);
    let sampled_users: i64 = client
        .query_one("SELECT count(*) FROM users", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(sampled_users, 1, "50% of two users is one row");
    let orphans: i64 = client
        .query_one(
            "SELECT count(*) FROM comments c \
             LEFT JOIN users u ON u.id = c.user_id WHERE u.id IS NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(orphans, 0, "every foreign key must resolve in the subset");
    let kept_comments: i64 = client
        .query_one("SELECT count(*) FROM comments", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        kept_comments, 1,
        "the kept user's comment must come with it \u{2014} a zero here would satisfy \
         the orphan check vacuously"
    );

    let (_o, sampled_migrate) = run_autumn_ok(&project, &["migrate"], &envs);
    assert!(
        sampled_migrate.contains("Migrations are already up to date.")
            || sampled_migrate.contains("Migrations applied successfully."),
        "migrations must report a clean status against the sampled database: \
         {sampled_migrate}"
    );
    assert_app_serves_health(&project, &url).await;
}

/// Boot the generated app in `project` against `url` and require `GET /health`
/// to answer 200, killing the server on the way out.
async fn assert_app_serves_health(project: &Path, url: &str) {
    let build = Command::new("cargo")
        .args(["build"])
        .current_dir(project)
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
    let stdout_path = project.join(format!("app-stdout-{app_port}.log"));
    let stderr_path = project.join(format!("app-stderr-{app_port}.log"));
    let mut command = Command::new("cargo");
    // Its own process group, so `ServerGuard` can take the whole tree down —
    // `cargo run`'s grandchild server outlives a plain kill of `cargo`.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let child = command
        .args(["run"])
        .current_dir(project)
        .env("AUTUMN_SERVER__PORT", app_port.to_string())
        .env("AUTUMN_DATABASE__URL", url)
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

// ─── `--sample`: laptop-sized, referentially-intact subsets (issue #1636) ────

/// The sampling fixture: a `countries` lookup every user points at, a
/// `comments` child hanging off `users`, and an `audit_logs` table connected to
/// nothing. Together they exercise all four sample roles — root, related,
/// always-include and never-include — in one schema.
const SAMPLE_SCHEMA: &str = "\
    CREATE TABLE countries ( \
        id BIGSERIAL PRIMARY KEY, \
        code TEXT NOT NULL UNIQUE, \
        name TEXT NOT NULL \
    ); \
    CREATE TABLE users ( \
        id BIGSERIAL PRIMARY KEY, \
        country_id BIGINT NOT NULL REFERENCES countries (id), \
        email TEXT NOT NULL UNIQUE, \
        full_name TEXT NOT NULL, \
        created_at TIMESTAMP NOT NULL DEFAULT NOW() \
    ); \
    CREATE TABLE comments ( \
        id BIGSERIAL PRIMARY KEY, \
        user_id BIGINT NOT NULL REFERENCES users (id), \
        body TEXT NOT NULL, \
        created_at TIMESTAMP NOT NULL DEFAULT NOW() \
    ); \
    CREATE TABLE audit_logs ( \
        id BIGSERIAL PRIMARY KEY, \
        actor_email TEXT NOT NULL, \
        action TEXT NOT NULL \
    ); \
    CREATE TABLE tags ( \
        id BIGSERIAL PRIMARY KEY, \
        label TEXT NOT NULL \
    ); \
    CREATE TABLE user_tags ( \
        user_id BIGINT NOT NULL REFERENCES users (id), \
        tag_id BIGINT NOT NULL REFERENCES tags (id), \
        PRIMARY KEY (user_id, tag_id) \
    ); \
    CREATE TABLE user_tag_notes ( \
        id BIGSERIAL PRIMARY KEY, \
        user_id BIGINT NOT NULL, \
        tag_id BIGINT NOT NULL, \
        note TEXT NOT NULL, \
        FOREIGN KEY (user_id, tag_id) REFERENCES user_tags (user_id, tag_id) \
    );";

/// 3 countries, 200 users, 400 comments, 500 audit rows — enough volume that a
/// 1% sample is a real subset rather than a rounding artefact. Generated from
/// `generate_series`, so two databases seeded this way hold identical ids and a
/// same-seed comparison is exact.
const SAMPLE_ROWS: &str = "\
    INSERT INTO countries (code, name) \
        SELECT 'C' || i, 'Country ' || i FROM generate_series(1, 3) AS i; \
    INSERT INTO users (country_id, email, full_name) \
        SELECT 1 + (i % 3), 'user' || i || '@real-corp.example', 'Real Person ' || i \
        FROM generate_series(1, 200) AS i; \
    INSERT INTO comments (user_id, body) \
        SELECT id, 'secret note ' || id FROM users \
        UNION ALL SELECT id, 'second secret note ' || id FROM users; \
    INSERT INTO audit_logs (actor_email, action) \
        SELECT 'admin' || i || '@real-corp.example', 'login' \
        FROM generate_series(1, 500) AS i; \
    INSERT INTO tags (label) SELECT 'tag ' || i FROM generate_series(1, 5) AS i; \
    INSERT INTO user_tags (user_id, tag_id) \
        SELECT id, 1 + (id % 5) FROM users; \
    INSERT INTO user_tag_notes (user_id, tag_id, note) \
        SELECT user_id, tag_id, 'secret note about ' || user_id FROM user_tags;";

/// The sampling fixture's declaration. `comments` is absent on purpose: it is
/// registered with the GDPR anonymize strategy by `write_project_sources`, so
/// the scrub classifies it with no declaration at all.
const SAMPLE_SCRUB_TOML: &str = r#"
[defaults]
safe_columns = ["id", "created_at"]

[tables.countries]
safe = ["code", "name"]

[tables.users]
safe = ["country_id"]

[tables.users.pii]
email = "email"
full_name = "name"

[tables.audit_logs]
safe = ["action"]

[tables.audit_logs.pii]
actor_email = "email"

[tables.tags]
safe = ["label"]

# A composite primary key, and a composite foreign key onto it: the row key and
# the walk's join both have to carry every component.
[tables.user_tags]
safe = ["user_id", "tag_id"]

[tables.user_tag_notes]
safe = ["user_id", "tag_id"]

[tables.user_tag_notes.pii]
note = "redact"

# Reference data is copied whole; the audit trail is not copied at all.
[sample]
always_include = ["countries"]
never_include = ["audit_logs"]
"#;

/// Create `name`, seed the sampling fixture into it and return a client.
async fn seed_sample_fixture(admin: &Client, base: &str, name: &str) -> Client {
    admin
        .batch_execute(&format!("CREATE DATABASE {name}"))
        .await
        .unwrap_or_else(|e| panic!("creating {name}: {e}"));
    let client = connect(&format!("{base}/{name}")).await;
    client.batch_execute(SAMPLE_SCHEMA).await.unwrap();
    client.batch_execute(SAMPLE_ROWS).await.unwrap();
    client
}

/// A project directory wired for the sampling fixture.
fn sample_project(dir: &Path) {
    write_project_sources(dir);
    std::fs::write(dir.join("scrub.toml"), SAMPLE_SCRUB_TOML).unwrap();
}

async fn count(client: &Client, sql: &str) -> i64 {
    client
        .query_one(sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get(0)
}

/// The ids the sample kept, ascending — the exact row set a seed selects.
async fn kept_user_ids(client: &Client) -> Vec<i64> {
    client
        .query("SELECT id FROM users ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// AC #1/#2/#3/#6/#8: one pass produces a scrubbed **and** sampled database
/// that is smaller than the source, keeps every foreign key resolvable, honours
/// the per-table rules, and carries none of the original values.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn sampled_scrub_is_smaller_referentially_intact_and_pii_free() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;
    let client = seed_sample_fixture(&admin, &base, "sample_target").await;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);
    let url = format!("{base}/sample_target");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_o, stderr) = run_autumn_ok(dir, &["db", "scrub", "--sample", "users=1%"], &envs);
    assert!(
        stderr.contains("Scrub complete"),
        "the sampled scrub should complete: {stderr}"
    );

    // ── AC #2: the root is sized as asked, related rows follow ──────────────
    assert_eq!(
        count(&client, "SELECT count(*) FROM users").await,
        2,
        "1% of 200 users is 2 rows"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM comments").await,
        4,
        "each kept user brings its two comments and nothing else"
    );

    // ── AC #3: per-table rules ──────────────────────────────────────────────
    assert_eq!(
        count(&client, "SELECT count(*) FROM countries").await,
        3,
        "an always-include lookup table is copied whole"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM audit_logs").await,
        0,
        "a never-include table is excluded entirely"
    );

    // ── AC #2/#6: every foreign key still resolves ──────────────────────────
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM comments c \
             LEFT JOIN users u ON u.id = c.user_id WHERE u.id IS NULL"
        )
        .await,
        0,
        "no comment may point at a user the sample dropped"
    );
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM users u \
             LEFT JOIN countries c ON c.id = u.country_id WHERE c.id IS NULL"
        )
        .await,
        0,
        "no user may point at a country the sample dropped"
    );

    // ── Composite keys: a two-column primary key, and a two-column foreign
    //    key onto it. Both the row key and the walk's join must carry every
    //    component, in key order.
    assert_eq!(
        count(&client, "SELECT count(*) FROM user_tags").await,
        2,
        "each kept user brings its one tag row, keyed on the composite key"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM user_tag_notes").await,
        2,
        "and the rows hanging off that composite key follow it"
    );
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM user_tag_notes n \
             LEFT JOIN user_tags t \
             ON t.user_id = n.user_id AND t.tag_id = n.tag_id \
             WHERE t.user_id IS NULL"
        )
        .await,
        0,
        "a composite reference must still resolve"
    );
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM user_tags t \
             LEFT JOIN tags g ON g.id = t.tag_id WHERE g.id IS NULL"
        )
        .await,
        0,
        "the tags a kept row points at must be kept"
    );

    // ── AC #1/#8: sampled AND scrubbed, in one pass ─────────────────────────
    //
    // Swept across every character column of every table rather than by a
    // hand-written column list, so a value that landed somewhere unexpected is
    // caught too.
    for secret in ["@real-corp.example", "Real Person", "secret note"] {
        let hits = occurrences(&client, secret).await;
        assert!(
            hits.is_empty(),
            "the sampled copy still contains {secret:?} in: {}",
            hits.join(", ")
        );
    }

    // ── AC #6: the run reports what it produced ─────────────────────────────
    assert!(
        stderr.contains("users: 200 \u{2192} 2 row(s)"),
        "the report must give per-table row counts: {stderr}"
    );
    assert!(
        stderr.contains("Total: ") && stderr.contains("of the source"),
        "the report must compare the subset to the source: {stderr}"
    );
    assert!(
        stderr.contains("foreign key(s) re-verified"),
        "the run must verify referential integrity itself: {stderr}"
    );
    assert!(
        stderr.contains("Table size: "),
        "the report must give the size versus the source: {stderr}"
    );
    assert!(
        !stderr.contains("postgres://"),
        "no message may print the connection URL: {stderr}"
    );
}

/// AC #4: the same seed selects the identical row set, and a different one does
/// not — the property that lets a teammate reproduce the exact subset that
/// exhibits a bug.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_same_seed_selects_the_identical_row_set() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);

    let mut selections = Vec::new();
    for (name, seed) in [("seed_a", "99"), ("seed_b", "99"), ("seed_c", "100")] {
        let client = seed_sample_fixture(&admin, &base, name).await;
        let url = format!("{base}/{name}");
        run_autumn_ok(
            dir,
            &["db", "scrub", "--sample", "users=10", "--seed", seed],
            &[("AUTUMN_DATABASE__URL", url.as_str())],
        );
        let ids = kept_user_ids(&client).await;
        assert_eq!(ids.len(), 10, "{name} should keep exactly 10 users");
        selections.push(ids);
    }

    assert_eq!(
        selections[0], selections[1],
        "the same seed against the same source data must select the identical rows"
    );
    assert_ne!(
        selections[0], selections[2],
        "a different seed must select a different subset"
    );
}

/// AC #5: a table the walk cannot reach aborts with a non-zero exit that names
/// it — sampling never empties a table without saying so.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sampling_refuses_a_table_no_root_can_reach() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;
    let client = seed_sample_fixture(&admin, &base, "sample_gap").await;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);
    // Drop the rule that accounted for `audit_logs`: nothing references it, so
    // the walk can no longer reach it.
    std::fs::write(
        dir.join("scrub.toml"),
        SAMPLE_SCRUB_TOML.replace("never_include = [\"audit_logs\"]", "never_include = []"),
    )
    .unwrap();
    let url = format!("{base}/sample_gap");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_o, stderr) = run_autumn_fail(dir, &["db", "scrub", "--sample", "users=1%"], &envs);
    assert!(
        stderr.contains("audit_logs"),
        "the refusal must name the uncovered table: {stderr}"
    );
    assert!(
        stderr.contains("cannot be reached"),
        "the refusal must say why: {stderr}"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM users").await,
        200,
        "a refused sample must not have deleted anything"
    );

    // The same gap is caught by `--check`, which is the CI gate.
    let (_o, check_err) = run_autumn_fail(
        dir,
        &["db", "scrub", "--check", "--sample", "users=1%"],
        &envs,
    );
    assert!(
        check_err.contains("audit_logs"),
        "--check must catch the gap before any restore: {check_err}"
    );
}

/// A trigger that writes PII into a purged table AFTER the rewrites must not
/// leave it there.
///
/// `[framework] purge` runs early so the sample's deletes are possible, but an
/// `UPDATE` trigger on a scrubbed table copies `OLD` values — the real PII —
/// into its audit table while the rewrites run, long after that early pass. The
/// purge after the rewrites is what makes the guarantee true; without it the run
/// reports `autumn_jobs (emptied)` while the original addresses sit in it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_trigger_cannot_refill_a_purged_table_with_pii() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;
    let client = seed_sample_fixture(&admin, &base, "purge_trigger").await;
    client
        .batch_execute(
            "CREATE TABLE autumn_jobs ( \
                id BIGSERIAL PRIMARY KEY, \
                args TEXT NOT NULL \
            ); \
            CREATE FUNCTION audit_user() RETURNS TRIGGER AS $$ \
            BEGIN \
                INSERT INTO autumn_jobs (args) VALUES (OLD.email); \
                RETURN NEW; \
            END; \
            $$ LANGUAGE plpgsql; \
            CREATE TRIGGER users_audit BEFORE UPDATE ON users \
                FOR EACH ROW EXECUTE FUNCTION audit_user();",
        )
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);
    std::fs::write(
        dir.join("scrub.toml"),
        format!("{SAMPLE_SCRUB_TOML}\n[framework]\npurge = [\"autumn_jobs\"]\n"),
    )
    .unwrap();
    let url = format!("{base}/purge_trigger");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    run_autumn_ok(dir, &["db", "scrub", "--sample", "users=50%"], &envs);

    assert_eq!(
        count(&client, "SELECT count(*) FROM autumn_jobs").await,
        0,
        "the trigger's rows must be purged after the rewrites, not before them"
    );
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM autumn_jobs WHERE args LIKE '%@example.com'",
        )
        .await,
        0,
        "no original address may survive in the purged table"
    );
}

/// A purge the sample's own emptied rows reference has to wait for the sample.
///
/// `[framework] purge` runs at the START of the transaction so a framework table
/// referencing a sampled one is already empty when the sample removes its
/// parents. That order is wrong for the mirror shape — an app table pointing INTO
/// a purged table — so the plan defers that one purge. Without the deferral the
/// `DELETE FROM autumn_jobs` hits `audit_logs`'s rows and the whole run rolls
/// back, which is exactly what this asserts does not happen.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_purge_an_emptied_table_references_runs_after_the_sample() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;
    let client = seed_sample_fixture(&admin, &base, "sample_purge_order").await;
    // A framework-owned table, and an excluded app table that references it.
    // `audit_logs` is `never_include`, so the sample empties it — which is what
    // makes the deferred purge possible at all.
    client
        .batch_execute(
            "CREATE TABLE autumn_jobs ( \
                id BIGSERIAL PRIMARY KEY, \
                args JSONB NOT NULL \
            ); \
            INSERT INTO autumn_jobs (args) VALUES ('{\"note\": \"payload\"}'); \
            ALTER TABLE audit_logs ADD COLUMN job_id BIGINT REFERENCES autumn_jobs (id); \
            UPDATE audit_logs SET job_id = (SELECT id FROM autumn_jobs LIMIT 1);",
        )
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);
    std::fs::write(
        dir.join("scrub.toml"),
        format!(
            "{}\n[framework]\npurge = [\"autumn_jobs\"]\n",
            SAMPLE_SCRUB_TOML.replace(
                "[tables.audit_logs]\nsafe = [\"action\"]",
                "[tables.audit_logs]\nsafe = [\"action\", \"job_id\"]",
            )
        ),
    )
    .unwrap();
    let url = format!("{base}/sample_purge_order");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    // The dry run advertises itself as the exact SQL in execution order, so it
    // has to hold the deferred purge back too — printing it first would show a
    // sequence that fails if a reader actually ran it.
    let (_o, dry_err) = run_autumn_ok(
        dir,
        &["db", "scrub", "--dry-run", "--sample", "users=1%"],
        &envs,
    );
    let purge_at = dry_err
        .find(r#"DELETE FROM "public"."autumn_jobs""#)
        .expect("the dry run must print the purge");
    let sample_at = dry_err
        .find(r#"DELETE FROM "public"."audit_logs""#)
        .expect("the dry run must print the sample's own delete");
    assert!(
        sample_at < purge_at,
        "the deferred purge must print AFTER the sample empties its child:\n{dry_err}"
    );

    run_autumn_ok(dir, &["db", "scrub", "--sample", "users=1%"], &envs);

    assert_eq!(
        count(&client, "SELECT count(*) FROM autumn_jobs").await,
        0,
        "the deferred purge must still empty its table"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM audit_logs").await,
        0,
        "and the excluded table it referenced is emptied by the sample"
    );
    assert!(
        count(&client, "SELECT count(*) FROM users").await < 200,
        "the sample itself must still have run"
    );
}

/// The shape no order satisfies: rows the sample KEEPS reference a purged table.
/// Purging before the sample hits them and purging after still hits them, so the
/// run refuses up front rather than failing mid-transaction.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sampling_refuses_a_retained_reference_into_a_purged_table() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;
    let client = seed_sample_fixture(&admin, &base, "sample_purge_conflict").await;
    // Same edge, but from `tags`, whose rows the sample keeps.
    client
        .batch_execute(
            "CREATE TABLE autumn_jobs ( \
                id BIGSERIAL PRIMARY KEY, \
                args JSONB NOT NULL \
            ); \
            ALTER TABLE tags ADD COLUMN job_id BIGINT REFERENCES autumn_jobs (id);",
        )
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);
    std::fs::write(
        dir.join("scrub.toml"),
        format!(
            "{}\n[framework]\npurge = [\"autumn_jobs\"]\n",
            SAMPLE_SCRUB_TOML.replace(
                "[tables.tags]\nsafe = [\"label\"]",
                "[tables.tags]\nsafe = [\"label\", \"job_id\"]",
            )
        ),
    )
    .unwrap();
    let url = format!("{base}/sample_purge_conflict");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_o, stderr) = run_autumn_fail(dir, &["db", "scrub", "--sample", "users=1%"], &envs);
    assert!(
        stderr.contains("autumn_jobs"),
        "the refusal must name the purged table: {stderr}"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM users").await,
        200,
        "a refused sample must not have deleted anything"
    );
}

/// AC #5's other refusal shape, end to end: a reference INTO an excluded table
/// would dangle, so the run aborts non-zero naming the edge.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sampling_refuses_a_reference_into_an_excluded_table() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;
    let client = seed_sample_fixture(&admin, &base, "sample_dangling").await;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);
    // `users` references `countries`, so excluding the lookup table entirely
    // would leave every kept user pointing at nothing.
    std::fs::write(
        dir.join("scrub.toml"),
        SAMPLE_SCRUB_TOML
            .replace("always_include = [\"countries\"]", "always_include = []")
            .replace(
                "never_include = [\"audit_logs\"]",
                "never_include = [\"audit_logs\", \"countries\"]",
            ),
    )
    .unwrap();
    let url = format!("{base}/sample_dangling");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_o, stderr) = run_autumn_fail(dir, &["db", "scrub", "--sample", "users=1%"], &envs);
    assert!(
        stderr.contains("users -> countries"),
        "the refusal must name the offending reference: {stderr}"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM users").await,
        200,
        "a refused sample must not have deleted anything"
    );
}

/// AC #1's other half: the sample's row removals are part of the scrub's
/// transaction, so a failure AFTER they run takes them with it.
///
/// A trigger that raises on `UPDATE users` fails the rewrite that follows the
/// removals — the only way to reach that window from outside the command.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_failure_after_the_sample_rolls_its_removals_back() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;
    let client = seed_sample_fixture(&admin, &base, "sample_rollback").await;
    client
        .batch_execute(
            "CREATE FUNCTION refuse_update() RETURNS trigger AS $$ \
             BEGIN RAISE EXCEPTION 'no rewrites here'; END; $$ LANGUAGE plpgsql; \
             CREATE TRIGGER users_refuse_update BEFORE UPDATE ON users \
             FOR EACH ROW EXECUTE FUNCTION refuse_update();",
        )
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);
    let url = format!("{base}/sample_rollback");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_o, stderr) = run_autumn_fail(dir, &["db", "scrub", "--sample", "users=1%"], &envs);
    assert!(
        stderr.contains("no rewrites here"),
        "the rewrite must be what failed: {stderr}"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM users").await,
        200,
        "the sample's removals must roll back with the rewrite that failed"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM audit_logs").await,
        500,
        "and so must the excluded table's removal"
    );
}

/// AC #1: there is no path that emits sampled-but-unscrubbed rows. A
/// classification failure refuses before the sample deletes a single row,
/// because both are phases of one transaction.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_unclassified_column_refuses_before_the_sample_deletes_anything() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;
    let client = seed_sample_fixture(&admin, &base, "sample_unclassified").await;
    client
        .batch_execute("ALTER TABLE users ADD COLUMN ssn TEXT;")
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);
    let url = format!("{base}/sample_unclassified");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_o, stderr) = run_autumn_fail(dir, &["db", "scrub", "--sample", "users=1%"], &envs);
    assert!(
        stderr.contains("users.ssn"),
        "the undeclared column must still be refused: {stderr}"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM users").await,
        200,
        "no row may be sampled away by a run that refuses to scrub"
    );
}

/// `--check` and `--dry-run` write nothing, sample included.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sample_check_and_dry_run_write_nothing() {
    let (_pg, host, port) = start_postgres().await;
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let admin = connect(&format!("{base}/postgres")).await;
    let client = seed_sample_fixture(&admin, &base, "sample_no_write").await;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    sample_project(dir);
    let url = format!("{base}/sample_no_write");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_o, check_err) = run_autumn_ok(
        dir,
        &["db", "scrub", "--check", "--sample", "users=1%"],
        &envs,
    );
    assert!(
        check_err.contains("Every table is covered by the sample"),
        "--check must confirm the sample plan is complete: {check_err}"
    );

    let (_o, dry_err) = run_autumn_ok(
        dir,
        &["db", "scrub", "--dry-run", "--sample", "users=1%"],
        &envs,
    );
    assert!(
        dry_err.contains("DELETE FROM \"public\".\"audit_logs\""),
        "the dry run must print the sample's own statements: {dry_err}"
    );

    assert_eq!(
        count(&client, "SELECT count(*) FROM users").await,
        200,
        "neither mode may write"
    );
    assert_eq!(
        count(&client, "SELECT count(*) FROM audit_logs").await,
        500,
        "neither mode may empty an excluded table"
    );
}
