//! `SQLite` integration test for `autumn schema pull` (issue #1975).
//!
//! Compiles and runs **only** under the non-default `sqlite` cargo feature (the
//! backend-flip). A default `cargo test -p autumn-cli` neither compiles nor requires
//! this file — it is `#[cfg(feature = "sqlite")]`-gated in `tests/integration/mod.rs`
//! and additionally guarded here.
//!
//! Unlike the Postgres pull suite this needs **no Docker**: it drives the real
//! binary end-to-end against a temp-FILE `SQLite` database (not `:memory:`, which the
//! runtime rejects for registered migrations):
//!
//!   `cargo test -p autumn-cli --no-default-features --features sqlite --test cli_tests schema_pull_sqlite`
#![cfg(feature = "sqlite")]

use std::path::{Path, PathBuf};
use std::process::Command;

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

fn fresh_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name], &[]);
    let project = tmp.path().join(name);
    (tmp, project)
}

/// Overwrite the project's single-file models, dropping any scaffolded
/// `src/models/` directory so the single-file layout resolves.
fn write_models(project: &Path, content: &str) {
    let src = project.join("src");
    let _ = std::fs::remove_dir_all(src.join("models"));
    std::fs::write(src.join("models.rs"), content).expect("write models.rs");
}

/// Two well-behaved models exercising the full clean-round-trip `SQLite` type surface:
/// a `BigSerial` id (→ `INTEGER PRIMARY KEY AUTOINCREMENT`), a unique column (→ a
/// standalone `CREATE UNIQUE INDEX`), a foreign key (`Post.author_id → authors`, →
/// an auto-index + `REFERENCES`), a nullable column, a `f64` (→ `REAL`), plain text
/// columns, and the synthesized `created_at` (→ `TEXT DEFAULT CURRENT_TIMESTAMP`,
/// recovered as a `Timestamp` with a `Now` default).
const MODELS: &str = r"
#[autumn_web::model(managed)]
pub struct Author {
    #[id]
    pub id: i64,
    #[unique]
    pub email: String,
    pub name: String,
}

#[autumn_web::model(managed)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub body: Option<String>,
    pub rating: f64,
    #[references(authors)]
    pub author_id: i64,
}
";

/// THE ACCEPTANCE: a schema built from `#[model]` structs, migrated into a live
/// `SQLite` file, then `schema pull`ed back, produces a snapshot a subsequent
/// `schema diff` reports as EMPTY — the `SQLite`-introspected snapshot is byte-faithful
/// to what the models produce, so the round-trip is clean. Also asserts `doctor`
/// agrees the database matches the pulled snapshot.
#[test]
fn schema_pull_round_trips_models_through_a_sqlite_file() {
    let (_tmp, project) = fresh_project("pull_sqlite_roundtrip");
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // 1. Declare the models + an empty baseline snapshot (Sqlite-tagged), generate
    //    the CREATE-TABLE migration, and apply it into the SQLite file.
    write_models(&project, MODELS);
    std::fs::write(project.join("empty_models.rs"), "").expect("empty models");
    run_autumn_ok(
        &project,
        &[
            "schema",
            "snapshot",
            "--from",
            "empty_models.rs",
            "--backend",
            "sqlite",
        ],
        &envs,
    );
    run_autumn_ok(
        &project,
        &["schema", "diff", "--write-migration", "--name", "init"],
        &envs,
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(db_path.is_file(), "sqlite db file created");

    // 2. Pull the live SQLite database back into the snapshot, OVERWRITING the
    //    model-derived baseline with the DB-introspected one.
    let (pull_out, _) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert!(
        pull_out.contains("pulled schema snapshot") && pull_out.contains("2 table(s)"),
        "pull reports the table count: {pull_out}"
    );

    // 3. The pulled snapshot faithfully carries the SQLite shapes.
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"backend\": \"Sqlite\""),
        "sqlite-tagged: {snap}"
    );
    assert!(
        snap.contains("\"name\": \"authors\""),
        "authors table: {snap}"
    );
    assert!(snap.contains("\"name\": \"posts\""), "posts table: {snap}");
    // The AUTOINCREMENT id is recovered as a BigSerial serial marker.
    assert!(
        snap.contains("\"serial\": \"BigSerial\""),
        "the INTEGER PRIMARY KEY AUTOINCREMENT id carries the serial marker: {snap}"
    );
    // The FK, the unique index, and the created_at Timestamp recovery all survive.
    assert!(
        snap.contains("\"table\": \"authors\""),
        "the FK references authors: {snap}"
    );
    assert!(
        snap.contains("idx_authors_email_unique"),
        "unique index preserved: {snap}"
    );
    assert!(
        snap.contains("\"Timestamp\"") && snap.contains("\"Now\""),
        "created_at recovered as a Timestamp with a Now default: {snap}"
    );
    // No framework/bookkeeping tables leaked.
    assert!(
        !snap.contains("__diesel_schema_migrations") && !snap.contains("sqlite_sequence"),
        "no framework/internal tables: {snap}"
    );

    // 4. THE ACCEPTANCE: `schema diff` (models vs the pulled snapshot) reports no
    //    changes — the SQLite round-trip is clean.
    let (diff_out, _) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        diff_out.contains("No schema changes"),
        "the SQLite round-trip must yield an empty diff:\n{diff_out}"
    );

    // 5. A re-pull is byte-for-byte stable.
    run_autumn_ok(&project, &["schema", "pull"], &envs);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("second pull");
    assert_eq!(snap, snap_after, "a re-pull must be byte-for-byte stable");

    // 6. doctor agrees the database matches the pulled snapshot baseline.
    let (doc_out, _) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the SQLite DB matches the snapshot:\n{doc_out}"
    );
}

/// Write a hand-authored raw-SQL migration under `migrations/<name>/{up,down}.sql`
/// so a brownfield DB shape (composite FKs, `DEFAULT NULL`, inline `UNIQUE`, FTS5)
/// the model DSL cannot generate can be applied via `schema migrate`.
fn write_raw_migration(project: &Path, name: &str, up_sql: &str, down_sql: &str) {
    let dir = project.join("migrations").join(name);
    std::fs::create_dir_all(&dir).expect("mkdir migration");
    std::fs::write(dir.join("up.sql"), up_sql).expect("up.sql");
    std::fs::write(dir.join("down.sql"), down_sql).expect("down.sql");
}

/// Parse the pulled snapshot and return the named table's JSON object.
fn table_json(snap: &str, table: &str) -> serde_json::Value {
    let root: serde_json::Value = serde_json::from_str(snap).expect("snapshot JSON");
    root["tables"]
        .as_array()
        .expect("tables array")
        .iter()
        .find(|t| t["name"] == table)
        .unwrap_or_else(|| panic!("table {table} not in snapshot: {snap}"))
        .clone()
}

/// The named column object within a table JSON value.
fn column_json<'a>(table: &'a serde_json::Value, column: &str) -> &'a serde_json::Value {
    table["columns"]
        .as_array()
        .expect("columns array")
        .iter()
        .find(|c| c["name"] == column)
        .unwrap_or_else(|| panic!("column {column} not found: {table}"))
}

/// CONTRACT (Gemini review): a composite (multi-column) foreign key is OMITTED
/// entirely from the pulled IR (the IR `ForeignKey` is single-column only, and the
/// fail-closed posture skips what it cannot faithfully represent), a single-column
/// FK is preserved, a `DEFAULT NULL` bareword maps to NO default, and a quoted
/// `'NULL'` string default is preserved verbatim.
#[test]
fn schema_pull_omits_composite_fk_and_ignores_default_null() {
    let (_tmp, project) = fresh_project("pull_sqlite_fk_defaults");
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_brownfield_fks",
        "CREATE TABLE orgs (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL);\n\
         CREATE TABLE parents (org_id INTEGER NOT NULL, slug TEXT NOT NULL, \
         PRIMARY KEY (org_id, slug));\n\
         CREATE TABLE children (\
           id INTEGER PRIMARY KEY AUTOINCREMENT, \
           org_id INTEGER NOT NULL, \
           parent_slug TEXT NOT NULL, \
           owner_id INTEGER REFERENCES orgs(id), \
           note TEXT DEFAULT NULL, \
           label TEXT DEFAULT 'NULL', \
           FOREIGN KEY (org_id, parent_slug) REFERENCES parents(org_id, slug));\n",
        "DROP TABLE children;\nDROP TABLE parents;\nDROP TABLE orgs;\n",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);

    run_autumn_ok(&project, &["schema", "pull"], &envs);
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    let children = table_json(&snap, "children");

    // The single-column FK survives.
    let owner = column_json(&children, "owner_id");
    assert_eq!(
        owner["references"]["table"].as_str(),
        Some("orgs"),
        "the single-column FK is preserved: {owner}"
    );
    // The composite FK is omitted — NEITHER of its columns carries a `references`.
    for col in ["org_id", "parent_slug"] {
        let c = column_json(&children, col);
        assert!(
            c.get("references").is_none() || c["references"].is_null(),
            "the composite FK column {col} must carry no single-column FK: {c}"
        );
    }
    // `DEFAULT NULL` bareword → no default; quoted `'NULL'` → preserved.
    let note = column_json(&children, "note");
    assert!(
        note.get("default").is_none() || note["default"].is_null(),
        "DEFAULT NULL maps to no default: {note}"
    );
    let label = column_json(&children, "label");
    assert_eq!(
        label["default"]["Sql"].as_str(),
        Some("'NULL'"),
        "a quoted 'NULL' string default is preserved: {label}"
    );

    // No spurious drift from the omitted composite FK / normalized defaults.
    let (doc_out, _) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor is clean after the pull:\n{doc_out}"
    );
}

/// CONTRACT (Codex review): a brownfield inline `col TEXT UNIQUE` constraint pulls
/// as a retained (non-droppable) `sqlite_autoindex_*` unique index, so a matching
/// `#[unique]` model round-trips to an EMPTY diff — the undroppable auto-index is
/// never `DROP INDEX`ed (which `SQLite` rejects).
#[test]
fn schema_pull_brownfield_inline_unique_round_trips_clean() {
    let (_tmp, project) = fresh_project("pull_sqlite_inline_unique");
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_inline_unique",
        "CREATE TABLE accounts (\
           id INTEGER PRIMARY KEY AUTOINCREMENT, \
           email TEXT NOT NULL UNIQUE, \
           created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);\n",
        "DROP TABLE accounts;\n",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);
    run_autumn_ok(&project, &["schema", "pull"], &envs);

    // A natural model matching the brownfield table (the inline UNIQUE becomes
    // `#[unique]`, which the parser emits as `idx_accounts_email_unique`).
    write_models(
        &project,
        "#[autumn_web::model(managed)]\npub struct Account {\n    #[id]\n    pub id: i64,\n    \
         #[unique]\n    pub email: String,\n}\n",
    );
    let (diff_out, diff_err, code) = run_autumn(&project, &["schema", "diff"], &envs);
    let combined = format!("{diff_out}{diff_err}");
    assert_eq!(code, Some(0), "the diff must succeed:\n{combined}");
    assert!(
        combined.contains("No schema changes"),
        "a brownfield inline-UNIQUE table round-trips clean against the matching model:\n{combined}"
    );
    assert!(
        !combined.contains("DROP INDEX") && !combined.to_uppercase().contains("SQLITE_AUTOINDEX"),
        "the undroppable UNIQUE-constraint auto-index is never dropped:\n{combined}"
    );
}

/// CONTRACT (Codex review): FTS5 search-index tables — the `CREATE VIRTUAL TABLE
/// "<table>__fts"` a `--searchable` model generates plus its `_data`/`_idx`/
/// `_docsize`/`_config` shadow tables — are excluded from the pull, so a searchable
/// app pulls clean (no `__fts*` tables, no spurious drift).
#[test]
fn schema_pull_excludes_fts5_search_tables() {
    let (_tmp, project) = fresh_project("pull_sqlite_fts5");
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_fts5",
        "CREATE TABLE posts (\
           id INTEGER PRIMARY KEY AUTOINCREMENT, \
           title TEXT NOT NULL, \
           created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);\n\
         CREATE VIRTUAL TABLE \"posts__fts\" USING fts5(\"title\", content='posts', content_rowid='id');\n",
        "DROP TABLE posts;\n",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);
    run_autumn_ok(&project, &["schema", "pull"], &envs);

    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"name\": \"posts\""),
        "the base table is pulled: {snap}"
    );
    assert!(
        !snap.contains("__fts"),
        "no FTS5 virtual/shadow tables leak into the snapshot: {snap}"
    );

    // A matching model round-trips clean — the search index never shows as drift.
    write_models(
        &project,
        "#[autumn_web::model(managed)]\npub struct Post {\n    #[id]\n    pub id: i64,\n    \
         pub title: String,\n}\n",
    );
    let (diff_out, diff_err, code) = run_autumn(&project, &["schema", "diff"], &envs);
    let combined = format!("{diff_out}{diff_err}");
    assert_eq!(code, Some(0), "the diff must succeed:\n{combined}");
    assert!(
        combined.contains("No schema changes"),
        "a searchable app pulls clean (FTS tables excluded):\n{combined}"
    );
}

/// CONTRACT (Codex review): a `schema pull` against a NONEXISTENT sqlite file must
/// error clearly (`SQLite` would otherwise create-on-open an empty DB and a non-dry-run
/// pull would overwrite the checked-in snapshot with zero tables). It must NOT create
/// the file NOR overwrite an existing snapshot.
#[test]
fn schema_pull_errors_on_missing_sqlite_file() {
    let (_tmp, project) = fresh_project("pull_sqlite_missing");
    let missing = project.join("does-not-exist.db");
    let url = format!("sqlite://{}", missing.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // Seed a non-empty, sqlite-tagged snapshot we can prove is not clobbered.
    std::fs::create_dir_all(project.join(".autumn")).expect("mkdir .autumn");
    let sentinel = "{\"backend\":\"Sqlite\",\"tables\":[{\"name\":\"sentinel\",\"managed\":true,\"columns\":[],\"indexes\":[],\"checks\":[],\"primary_key\":[]}]}";
    std::fs::write(&snapshot_path, sentinel).expect("seed snapshot");

    let (out, err, code) = run_autumn(&project, &["schema", "pull"], &envs);
    let combined = format!("{out}{err}");
    assert_ne!(
        code,
        Some(0),
        "pull against a missing file must fail:\n{combined}"
    );
    assert!(
        combined.contains("could not open the SQLite database")
            && combined.contains("does-not-exist.db"),
        "the error names the missing file:\n{combined}"
    );
    assert!(!missing.exists(), "the missing DB file must NOT be created");
    assert_eq!(
        std::fs::read_to_string(&snapshot_path).expect("snapshot"),
        sentinel,
        "the existing snapshot must NOT be overwritten"
    );
}
