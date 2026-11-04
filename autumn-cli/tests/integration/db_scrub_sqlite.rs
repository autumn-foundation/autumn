//! `autumn db scrub` against a `SQLite` app, end to end through the real
//! binary (issue #2553).
//!
//! No Docker and no Postgres tools: a `SQLite` target is a file. Every run uses
//! an empty `PATH`, so a regression to `psql`/`pg_dump` fails here.
//!
//! Not gated on `--features sqlite`. diesel's `SQLite` backend is in the
//! default graph, as for `db_backup_sqlite`.

use std::path::{Path, PathBuf};
use std::process::Command;

use diesel::connection::SimpleConnection as _;
use diesel::{Connection as _, QueryableByName, RunQueryDsl as _, SqliteConnection, sql_query};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// Values that must not survive a scrub anywhere in the file.
const SECRETS: &[&str] = &[
    "alice@real-corp.example",
    "bob@real-corp.example",
    "Alice Realname",
    "+14155550101",
    "lives at 42 Real Street",
    "my real secret comment",
];

const SCHEMA: &str = "\
    CREATE TABLE users ( \
        id INTEGER PRIMARY KEY, \
        email TEXT NOT NULL UNIQUE, \
        full_name TEXT NOT NULL, \
        phone TEXT, \
        bio TEXT, \
        role TEXT NOT NULL, \
        created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP \
    ); \
    CREATE TABLE comments ( \
        id INTEGER PRIMARY KEY, \
        user_id INTEGER NOT NULL REFERENCES users(id), \
        body TEXT NOT NULL \
    ); \
    INSERT INTO users (email, full_name, phone, bio, role) VALUES \
        ('alice@real-corp.example', 'Alice Realname', '+14155550101', \
         'lives at 42 Real Street', 'admin'), \
        ('bob@real-corp.example', 'Bob Realname', NULL, NULL, 'member'); \
    INSERT INTO comments (user_id, body) \
        SELECT id, 'my real secret comment' FROM users;";

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

[tables.comments]
safe = ["user_id"]

[tables.comments.pii]
body = "redact"
"#;

#[derive(QueryableByName)]
struct Text {
    #[diesel(sql_type = diesel::sql_types::Text)]
    v: String,
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

fn open(db: &Path) -> SqliteConnection {
    SqliteConnection::establish(&db.to_string_lossy()).expect("open sqlite")
}

fn texts(db: &Path, sql: &str) -> Vec<String> {
    let rows: Vec<Text> = sql_query(sql).load(&mut open(db)).expect("query");
    rows.into_iter().map(|r| r.v).collect()
}

fn count(db: &Path, sql: &str) -> i64 {
    let rows: Vec<Count> = sql_query(sql).load(&mut open(db)).expect("count");
    rows.first().map_or(-1, |r| r.n)
}

/// A project with a seeded `app.db`, an empty `src/`, and `scrub_toml`.
fn project(scrub_toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("autumn.toml"),
        "[database]\nurl = \"sqlite://app.db\"\n",
    )
    .expect("autumn.toml");
    std::fs::create_dir_all(root.join("src")).expect("src");
    std::fs::write(root.join("scrub.toml"), scrub_toml).expect("scrub.toml");
    open(&root.join("app.db"))
        .batch_execute(SCHEMA)
        .expect("seed");
    dir
}

/// `(combined output, stdout, exit code)` from the real binary.
fn run(root: &Path, env: &str, args: &[&str]) -> (String, String, Option<i32>) {
    let empty = root.join("no-tools");
    std::fs::create_dir_all(&empty).expect("mkdir");
    let out = Command::new(autumn_bin())
        .args(args)
        .current_dir(root)
        .env("PATH", &empty)
        .env("AUTUMN_PG_BIN_DIR", &empty)
        .env("AUTUMN_ENV", env)
        .output()
        .expect("run autumn");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let both = format!("{stdout}{}", String::from_utf8_lossy(&out.stderr));
    (both, stdout, out.status.code())
}

/// Every byte of the database file and its sidecars.
fn raw_bytes(db: &Path) -> Vec<u8> {
    let mut all = std::fs::read(db).expect("db");
    for suffix in ["-wal", "-journal"] {
        let mut side = db.as_os_str().to_owned();
        side.push(suffix);
        if let Ok(bytes) = std::fs::read(PathBuf::from(side)) {
            all.extend(bytes);
        }
    }
    all
}

fn leaked(db: &Path) -> Vec<&'static str> {
    let bytes = raw_bytes(db);
    SECRETS
        .iter()
        .copied()
        .filter(|s| bytes.windows(s.len()).any(|w| w == s.as_bytes()))
        .collect()
}

#[test]
fn scrub_rewrites_pii_and_keeps_structure() {
    let project = project(SCRUB_TOML);
    let root = project.path();
    let db = root.join("app.db");

    let (out, _, code) = run(root, "dev", &["db", "scrub"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("Scrub complete"), "{out}");

    let emails = texts(&db, "SELECT email AS v FROM users ORDER BY id");
    assert_eq!(emails.len(), 2);
    for email in &emails {
        assert!(
            email.starts_with("scrubbed+") && email.ends_with("@example.invalid"),
            "{email}"
        );
    }
    assert_ne!(emails[0], emails[1], "unique column stays unique");
    assert_eq!(
        texts(&db, "SELECT role AS v FROM users ORDER BY id"),
        ["admin", "member"],
        "safe column untouched"
    );
    let phones = texts(
        &db,
        "SELECT COALESCE(phone, 'NULL') AS v FROM users ORDER BY id",
    );
    assert!(phones[0].starts_with("+1555"), "{phones:?}");
    assert_eq!(phones[1], "NULL", "NULL stays NULL");
    assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM comments"), 2);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS n FROM comments c JOIN users u ON u.id = c.user_id"
        ),
        2,
        "foreign keys intact"
    );
}

#[test]
fn scrub_is_deterministic_per_row() {
    let a = project(SCRUB_TOML);
    let b = project(SCRUB_TOML);
    for p in [&a, &b] {
        let (out, _, code) = run(p.path(), "dev", &["db", "scrub"]);
        assert_eq!(code, Some(0), "{out}");
    }
    let q = "SELECT email AS v FROM users ORDER BY id";
    assert_eq!(
        texts(&a.path().join("app.db"), q),
        texts(&b.path().join("app.db"), q)
    );
}

#[test]
fn no_original_value_survives_in_the_file_bytes() {
    let project = project(SCRUB_TOML);
    let db = project.path().join("app.db");
    assert!(
        !leaked(&db).is_empty(),
        "fixture must start with the secrets"
    );

    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(
        leaked(&db),
        Vec::<&str>::new(),
        "free pages and WAL included"
    );
}

#[test]
fn an_unclassified_column_aborts_and_writes_nothing() {
    let toml = SCRUB_TOML.replace("bio = \"redact\"\n", "");
    let project = project(&toml);
    let db = project.path().join("app.db");

    let (out, stdout, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("users.bio"), "names the column:\n{out}");
    assert!(
        stdout.contains("bio"),
        "suggests a stanza on stdout:\n{stdout}"
    );
    assert_eq!(leaked(&db).len(), SECRETS.len(), "nothing was written");
}

#[test]
fn a_column_added_after_the_config_was_written_aborts() {
    let project = project(SCRUB_TOML);
    open(&project.path().join("app.db"))
        .batch_execute("ALTER TABLE users ADD COLUMN ssn TEXT;")
        .expect("alter");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("users.ssn"), "{out}");
}

#[test]
fn a_stale_declaration_aborts() {
    let toml = SCRUB_TOML.replace("email = \"email\"", "nope = \"redact\"\nemail = \"email\"");
    let project = project(&toml);
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("nope"), "{out}");
}

#[test]
fn check_classifies_and_writes_nothing() {
    let project = project(SCRUB_TOML);
    let db = project.path().join("app.db");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub", "--check"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("users.email"), "{out}");
    assert_eq!(leaked(&db).len(), SECRETS.len());
}

#[test]
fn dry_run_prints_sql_and_writes_nothing() {
    let project = project(SCRUB_TOML);
    let db = project.path().join("app.db");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub", "--dry-run"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("UPDATE \"users\" SET"), "{out}");
    assert!(out.contains("BEGIN") && out.contains("COMMIT"), "{out}");
    assert!(out.contains("Dry run only"), "{out}");
    assert!(
        !out.contains("psql") && !out.contains("\\connect"),
        "no psql script for SQLite:\n{out}"
    );
    assert_eq!(leaked(&db).len(), SECRETS.len());
}

#[test]
fn production_profile_is_refused_without_force() {
    for args in [vec!["db", "scrub"], vec!["db", "scrub", "--dry-run"]] {
        let project = project(SCRUB_TOML);
        let db = project.path().join("app.db");
        let (out, _, code) = run(project.path(), "prod", &args);
        assert_eq!(code, Some(1), "{args:?}\n{out}");
        assert!(out.contains("prod"), "{out}");
        assert_eq!(
            leaked(&db).len(),
            SECRETS.len(),
            "{args:?}: nothing written"
        );
    }
}

#[test]
fn force_lets_a_production_profile_scrub() {
    let project = project(SCRUB_TOML);
    let (out, _, code) = run(project.path(), "prod", &["db", "scrub", "--force"]);
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(leaked(&project.path().join("app.db")), Vec::<&str>::new());
}

#[test]
fn a_missing_file_is_refused_and_not_created() {
    let project = project(SCRUB_TOML);
    std::fs::remove_file(project.path().join("app.db")).expect("rm");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        !project.path().join("app.db").exists(),
        "a typo must not create an empty database"
    );
}

#[test]
fn pii_on_a_key_column_is_refused() {
    let toml = SCRUB_TOML.replace(
        "safe = [\"user_id\"]",
        "safe = []\n[tables.comments.pii]\nuser_id = \"zero\"",
    );
    let toml = toml.replacen("[tables.comments.pii]\nbody", "body", 1);
    let project = project(&toml);
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("comments.user_id"), "{out}");
}

#[test]
fn a_check_constrained_column_is_refused() {
    let project = project(SCRUB_TOML);
    open(&project.path().join("app.db"))
        .batch_execute(
            "ALTER TABLE users ADD COLUMN tier TEXT NOT NULL DEFAULT 'free' \
             CHECK (tier IN ('free', 'pro'));",
        )
        .expect("alter");
    let toml = SCRUB_TOML.replace("bio = \"redact\"", "bio = \"redact\"\ntier = \"redact\"");
    std::fs::write(project.path().join("scrub.toml"), toml).expect("toml");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("users.tier"), "{out}");
}

#[test]
fn null_on_a_not_null_column_is_refused() {
    let toml = SCRUB_TOML.replace("full_name = \"name\"", "full_name = \"null\"");
    let project = project(&toml);
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("users.full_name"), "{out}");
}

#[test]
fn framework_tables_are_emptied_when_opted_in() {
    let toml = format!("{SCRUB_TOML}\n[framework]\npurge = [\"autumn_sync_rows\"]\n");
    let project = project(&toml);
    let db = project.path().join("app.db");
    open(&db)
        .batch_execute(
            "CREATE TABLE autumn_sync_rows (id INTEGER PRIMARY KEY, payload TEXT NOT NULL); \
             INSERT INTO autumn_sync_rows (payload) VALUES ('alice@real-corp.example');",
        )
        .expect("framework");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM autumn_sync_rows"), 0);
    assert_eq!(leaked(&db), Vec::<&str>::new());
}

#[test]
fn a_delete_trigger_on_a_purged_table_is_refused() {
    let toml = format!("{SCRUB_TOML}\n[framework]\npurge = [\"autumn_sync_rows\"]\n");
    let project = project(&toml);
    let db = project.path().join("app.db");
    open(&db)
        .batch_execute(
            "CREATE TABLE autumn_sync_rows (id INTEGER PRIMARY KEY, payload TEXT NOT NULL); \
             CREATE TABLE archive (payload TEXT); \
             CREATE TRIGGER keep AFTER DELETE ON autumn_sync_rows \
             BEGIN INSERT INTO archive VALUES (OLD.payload); END;",
        )
        .expect("framework");
    let toml = toml.replace(
        "[framework]",
        "[tables.archive]\nsafe = [\"payload\"]\n[framework]",
    );
    std::fs::write(project.path().join("scrub.toml"), toml).expect("toml");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("autumn_sync_rows"), "{out}");
}

#[test]
fn sample_is_refused_not_ignored() {
    let project = project(SCRUB_TOML);
    let (out, _, code) = run(
        project.path(),
        "dev",
        &["db", "scrub", "--sample", "users=1"],
    );
    assert_eq!(code, Some(1), "{out}");
    assert!(out.to_lowercase().contains("sample"), "{out}");
    assert_eq!(leaked(&project.path().join("app.db")).len(), SECRETS.len());
}

#[test]
fn a_sqlite_backup_artifact_is_restored_then_scrubbed() {
    let project = project(SCRUB_TOML);
    let root = project.path();
    let db = root.join("app.db");

    let (out, _, code) = run(root, "dev", &["db", "backup"]);
    assert_eq!(code, Some(0), "{out}");
    let run_dir = std::fs::read_dir(root.join("backups").join("dev"))
        .expect("backups")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("run dir");

    // Lose the live data, then scrub straight from the artifact.
    open(&db)
        .batch_execute("DELETE FROM comments; DELETE FROM users;")
        .expect("wipe");
    let (out, _, code) = run(
        root,
        "dev",
        &[
            "db",
            "scrub",
            "--artifact",
            run_dir.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM users"), 2, "restored");
    assert_eq!(leaked(&db), Vec::<&str>::new());
}

#[test]
fn output_writes_a_scrubbed_artifact() {
    let project = project(SCRUB_TOML);
    let root = project.path();
    let out_dir = root.join("handoff");
    let (out, _, code) = run(
        root,
        "dev",
        &["db", "scrub", "--output", out_dir.to_str().expect("utf-8")],
    );
    assert_eq!(code, Some(0), "{out}");
    let artifact = walk(&out_dir)
        .into_iter()
        .find(|p| p.extension().is_some_and(|e| e == "sqlite"))
        .expect("a .sqlite artifact");
    assert_eq!(leaked(&artifact), Vec::<&str>::new());
    assert_eq!(count(&artifact, "SELECT COUNT(*) AS n FROM users"), 2);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

// Unix only: the shared credentials fixture provisions keys with a shell script
// as the editor, which Windows cannot launch.
#[cfg(unix)]
#[test]
fn an_encrypted_column_is_resealed() {
    let project = project(SCRUB_TOML);
    let root = project.path();
    let db = root.join("app.db");
    open(&db)
        .batch_execute(
            "ALTER TABLE users ADD COLUMN api_token TEXT NOT NULL DEFAULT 'tok_REALSECRET'; \
             UPDATE users SET api_token = 'tok_REALSECRET_' || id;",
        )
        .expect("alter");
    std::fs::create_dir_all(root.join("src").join("models")).expect("models");
    std::fs::write(
        root.join("src").join("models").join("user.rs"),
        "use autumn_web::model;\n\n#[model]\npub struct User {\n    #[id]\n    pub id: i64,\n    \
         #[encrypted]\n    pub api_token: String,\n}\n",
    )
    .expect("model");
    super::db_scrub::write_encryption_credentials(root);

    let (out, _, code) = run(root, "dev", &["db", "scrub"]);
    assert_eq!(code, Some(0), "{out}");
    let tokens = texts(&db, "SELECT api_token AS v FROM users ORDER BY id");
    assert_eq!(tokens.len(), 2);
    for token in &tokens {
        assert!(!token.contains("REALSECRET"), "{token}");
        assert!(token.len() > 20, "a sealed envelope, not a blank: {token}");
    }
    assert_ne!(tokens[0], tokens[1]);
}

#[test]
fn a_virtual_table_is_refused() {
    let project = project(SCRUB_TOML);
    open(&project.path().join("app.db"))
        .batch_execute("CREATE VIRTUAL TABLE docs USING fts5(body);")
        .expect("fts5");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains("virtual tables") && out.contains("docs"),
        "{out}"
    );
    assert_eq!(leaked(&project.path().join("app.db")).len(), SECRETS.len());
}

#[test]
fn a_purge_that_breaks_a_foreign_key_rolls_everything_back() {
    let toml = format!(
        "{SCRUB_TOML}\n[tables.refs]\nsafe = [\"id\", \"sync_id\"]\n\n[framework]\npurge = [\"autumn_sync_rows\"]\n"
    );
    let project = project(&toml);
    let db = project.path().join("app.db");
    open(&db)
        .batch_execute(
            "CREATE TABLE autumn_sync_rows (id INTEGER PRIMARY KEY, payload TEXT NOT NULL); \
             INSERT INTO autumn_sync_rows (payload) VALUES ('x'); \
             CREATE TABLE refs (id INTEGER PRIMARY KEY, \
                 sync_id INTEGER REFERENCES autumn_sync_rows(id)); \
             INSERT INTO refs (sync_id) VALUES (1);",
        )
        .expect("schema");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM autumn_sync_rows"), 1);
    assert_eq!(
        leaked(&db).len(),
        SECRETS.len(),
        "the rewrite rolled back with the purge"
    );
}

#[test]
fn rows_with_a_null_non_integer_key_get_distinct_tokens() {
    let toml = format!(
        "{SCRUB_TOML}\n[tables.legacy]\nsafe = [\"code\"]\n\n[tables.legacy.pii]\nemail = \"email\"\n"
    );
    let project = project(&toml);
    let db = project.path().join("app.db");
    // SQLite allows several NULL keys in a rowid table with a TEXT primary key.
    open(&db)
        .batch_execute(
            "CREATE TABLE legacy (code TEXT PRIMARY KEY, email TEXT UNIQUE); \
             INSERT INTO legacy VALUES (NULL, 'a@real-corp.example'), (NULL, 'b@real-corp.example');",
        )
        .expect("legacy");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(0), "unique column stays unique:\n{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM legacy"),
        2
    );
}

/// Scrub `ddl` plus a `legacy` table config. Returns `(db, output, exit code)`.
fn scrub_legacy(ddl: &str, safe: &str) -> (PathBuf, tempfile::TempDir, String, Option<i32>) {
    let toml = format!(
        "{SCRUB_TOML}\n[tables.legacy]\nsafe = [{safe}]\n\n[tables.legacy.pii]\nemail = \"email\"\n"
    );
    let project = project(&toml);
    let db = project.path().join("app.db");
    open(&db).batch_execute(ddl).expect("legacy");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    (db, project, out, code)
}

#[test]
fn text_keys_that_differ_after_a_nul_get_distinct_tokens() {
    let (db, _p, out, code) = scrub_legacy(
        "CREATE TABLE legacy (code TEXT PRIMARY KEY, email TEXT UNIQUE); \
         INSERT INTO legacy VALUES (char(97, 0, 120), 'a@real-corp.example'), \
                                   (char(97, 0, 121), 'b@real-corp.example');",
        "\"code\"",
    );
    assert_eq!(code, Some(0), "unique column stays unique:\n{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM legacy"),
        2
    );
}

#[test]
fn real_keys_that_differ_past_15_digits_get_distinct_tokens() {
    let (db, _p, out, code) = scrub_legacy(
        "CREATE TABLE legacy (code REAL PRIMARY KEY, email TEXT UNIQUE); \
         INSERT INTO legacy VALUES (0.1 + 0.2, 'a@real-corp.example'), \
                                   (0.3, 'b@real-corp.example'), \
                                   (1.000000000000001, 'c@real-corp.example'), \
                                   (1.000000000000002, 'd@real-corp.example');",
        "\"code\"",
    );
    assert_eq!(code, Some(0), "unique column stays unique:\n{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM legacy"),
        4
    );
}

#[test]
fn composite_keys_with_a_nul_and_mixed_types_get_distinct_tokens() {
    let (db, _p, out, code) = scrub_legacy(
        "CREATE TABLE legacy (a, b, email TEXT UNIQUE, PRIMARY KEY (a, b)); \
         INSERT INTO legacy VALUES (char(97, 0, 120), 1, 'a@real-corp.example'), \
                                   (char(97, 0, 121), 1, 'b@real-corp.example'), \
                                   ('6162', 1, 'c@real-corp.example'), \
                                   (6162, 1, 'd@real-corp.example'), \
                                   (x'6162', 1, 'e@real-corp.example'), \
                                   ('1', 1, 'f@real-corp.example'), \
                                   (1, 1, 'g@real-corp.example');",
        "\"a\", \"b\"",
    );
    assert_eq!(code, Some(0), "unique column stays unique:\n{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM legacy"),
        7
    );
}

#[test]
fn a_user_column_named_rowid_does_not_collapse_tokens() {
    let (db, _p, out, code) = scrub_legacy(
        "CREATE TABLE legacy (RowID INTEGER, _rowid_ INTEGER, email TEXT UNIQUE); \
         INSERT INTO legacy VALUES (1, 1, 'a@real-corp.example'), (1, 1, 'b@real-corp.example'), \
                                   (NULL, NULL, 'c@real-corp.example'), (NULL, NULL, 'd@real-corp.example');",
        "\"RowID\", \"_rowid_\"",
    );
    assert_eq!(code, Some(0), "unique column stays unique:\n{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM legacy"),
        4
    );
}

#[test]
fn a_null_key_fallback_skips_a_shadowed_rowid() {
    let (db, _p, out, code) = scrub_legacy(
        "CREATE TABLE legacy (code TEXT PRIMARY KEY, rowid INTEGER, email TEXT UNIQUE); \
         INSERT INTO legacy VALUES (NULL, 1, 'a@real-corp.example'), \
                                   (NULL, 1, 'b@real-corp.example');",
        "\"code\", \"rowid\"",
    );
    assert_eq!(code, Some(0), "unique column stays unique:\n{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM legacy"),
        2
    );
}

#[test]
fn a_key_that_cannot_be_null_needs_no_rowid_name() {
    let (db, _p, out, code) = scrub_legacy(
        "CREATE TABLE legacy (id INTEGER PRIMARY KEY, rowid, _rowid_, oid, \
                              email TEXT UNIQUE); \
         INSERT INTO legacy (email) VALUES ('a@real-corp.example'), ('b@real-corp.example');",
        "\"id\", \"rowid\", \"_rowid_\", \"oid\"",
    );
    assert_eq!(code, Some(0), "id identifies each row:\n{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM legacy"),
        2
    );
}

#[test]
fn a_not_null_text_key_needs_no_rowid_name() {
    let (db, _p, out, code) = scrub_legacy(
        "CREATE TABLE legacy (code TEXT NOT NULL PRIMARY KEY, rowid, _rowid_, oid, \
                              email TEXT UNIQUE); \
         INSERT INTO legacy (code, email) VALUES ('a', 'a@real-corp.example'), \
                                                 ('b', 'b@real-corp.example');",
        "\"code\", \"rowid\", \"_rowid_\", \"oid\"",
    );
    assert_eq!(code, Some(0), "code identifies each row:\n{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM legacy"),
        2
    );
}

#[test]
fn a_descending_integer_key_keeps_the_rowid_fallback() {
    let (db, _p, out, code) = scrub_legacy(
        "CREATE TABLE legacy (id INTEGER PRIMARY KEY DESC, email TEXT UNIQUE); \
         INSERT INTO legacy (email) VALUES ('a@real-corp.example'), ('b@real-corp.example');",
        "\"id\"",
    );
    assert_eq!(code, Some(0), "NULL keys fall back to rowid:\n{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM legacy"),
        2
    );
}

#[test]
fn a_table_that_shadows_every_rowid_alias_is_refused() {
    let (db, _p, out, code) = scrub_legacy(
        "CREATE TABLE legacy (rowid INTEGER, _rowid_ INTEGER, oid INTEGER, email TEXT UNIQUE); \
         INSERT INTO legacy VALUES (1, 1, 1, 'a@real-corp.example');",
        "\"rowid\", \"_rowid_\", \"oid\"",
    );
    assert_ne!(code, Some(0), "must refuse:\n{out}");
    assert!(out.contains("legacy") && out.contains("rowid"), "{out}");
    assert!(out.contains("Nothing was written"), "{out}");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS n FROM legacy WHERE email LIKE '%real-corp%'"
        ),
        1
    );
}

#[test]
fn a_purge_that_cascades_into_an_app_table_is_refused() {
    let toml = format!(
        "{SCRUB_TOML}\n[tables.refs]\nsafe = [\"id\", \"sync_id\"]\n\n[framework]\npurge = [\"autumn_sync_rows\"]\n"
    );
    let project = project(&toml);
    let db = project.path().join("app.db");
    open(&db)
        .batch_execute(
            "CREATE TABLE autumn_sync_rows (id INTEGER PRIMARY KEY, payload TEXT NOT NULL); \
             INSERT INTO autumn_sync_rows (payload) VALUES ('x'); \
             CREATE TABLE refs (id INTEGER PRIMARY KEY, \
                 sync_id INTEGER REFERENCES autumn_sync_rows(id) ON DELETE CASCADE); \
             INSERT INTO refs (sync_id) VALUES (1);",
        )
        .expect("schema");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("refs -> autumn_sync_rows"), "{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(*) AS n FROM refs"),
        1,
        "nothing deleted"
    );
    assert_eq!(leaked(&db).len(), SECRETS.len(), "nothing scrubbed");
}

#[test]
fn a_blob_primary_key_with_invalid_utf8_scrubs() {
    let toml = format!(
        "{SCRUB_TOML}\n[tables.blobs]\nsafe = [\"id\"]\n\n[tables.blobs.pii]\nemail = \"email\"\n"
    );
    let project = project(&toml);
    let db = project.path().join("app.db");
    open(&db)
        .batch_execute(
            "CREATE TABLE blobs (id BLOB PRIMARY KEY, email TEXT UNIQUE); \
             INSERT INTO blobs VALUES (x'80ff', 'a@real-corp.example'), (x'ff80', 'b@real-corp.example');",
        )
        .expect("blobs");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM blobs"),
        2
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS n FROM blobs WHERE email LIKE 'scrubbed+%'"
        ),
        2
    );
}

#[test]
fn keys_of_different_storage_classes_get_distinct_tokens() {
    let toml = format!(
        "{SCRUB_TOML}\n[tables.mixed]\nsafe = [\"id\"]\n\n[tables.mixed.pii]\nemail = \"email\"\n"
    );
    let project = project(&toml);
    let db = project.path().join("app.db");
    // A typeless key column keeps each value's own storage class.
    open(&db)
        .batch_execute(
            "CREATE TABLE mixed (id PRIMARY KEY, email TEXT UNIQUE); \
             INSERT INTO mixed VALUES (x'6162', 'a@real-corp.example'), ('6162', 'b@real-corp.example');",
        )
        .expect("mixed");
    let (out, _, code) = run(project.path(), "dev", &["db", "scrub"]);
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(
        count(&db, "SELECT COUNT(DISTINCT email) AS n FROM mixed"),
        2
    );
}
