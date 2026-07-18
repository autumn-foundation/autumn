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

/// A model exercising the AFFINITY-collapsing `SQLite` type surface (issue #1975): a
/// `bool`, `i32`, `i64`, `f32`, `f64`, and a plain (non-`CURRENT_TIMESTAMP`)
/// `Timestamp` field. The emitter renders `bool`/`i32`/`i64` → `INTEGER`,
/// `f32`/`f64` → `REAL`, and the plain `Timestamp` → `TEXT`, so a pull cannot
/// recover the original variant — WITHOUT affinity-aware diffing every one of these
/// would drift on every pull. The `schema diff` must be EMPTY.
const AFFINITY_MODEL: &str = r"
#[autumn_web::model(managed)]
pub struct Metric {
    #[id]
    pub id: i64,
    pub enabled: bool,
    pub small: i32,
    pub big: i64,
    pub ratio32: f32,
    pub ratio64: f64,
    pub scheduled_at: chrono::NaiveDateTime,
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

/// THE AFFINITY ACCEPTANCE (issue #1975): a model whose `bool`/`i32`/`i64`/`f32`/
/// `f64`/plain-`Timestamp` fields the `SQLite` emitter COLLAPSES onto shared declared
/// types (`INTEGER`/`REAL`/`TEXT`) migrates into a temp-file `SQLite` DB, is
/// `schema pull`ed back, and `schema diff` reports NO changes — the affinity-aware
/// type comparison treats the pulled `Int64`/`Float64`/`Text` as equivalent to the
/// model's `Bool`/`Int32`/`Float32`/`Timestamp`. Before the fix this drifted on
/// every pull.
#[test]
fn schema_pull_affinity_collapsed_types_round_trip_clean() {
    let (_tmp, project) = fresh_project("pull_sqlite_affinity");
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    write_models(&project, AFFINITY_MODEL);
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
    run_autumn_ok(&project, &["schema", "pull"], &envs);

    // THE ACCEPTANCE: the affinity-collapsed types round-trip to an EMPTY diff.
    let (diff_out, _) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        diff_out.contains("No schema changes"),
        "affinity-collapsed SQLite types must round-trip clean:\n{diff_out}"
    );
    // doctor agrees (bidirectional drift check) — no spurious type-change drift.
    let (doc_out, _) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor sees no drift for the affinity-collapsed types:\n{doc_out}"
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

    // The rebuild / DropTable-rollback path must NEVER emit a `CREATE … "sqlite_…"`
    // (SQLite refuses to create `sqlite_`-prefixed objects). Drop the model entirely
    // so the migration's up = DROP TABLE and down = CREATE TABLE accounts (restored
    // from the pulled baseline) — then assert neither leg emits any `sqlite_`-named
    // statement.
    write_models(&project, "");
    run_autumn_ok(
        &project,
        &[
            "schema",
            "diff",
            "--write-migration",
            "--name",
            "drop_accounts",
            "--allow-destructive",
        ],
        &envs,
    );
    let mig_dir = std::fs::read_dir(project.join("migrations"))
        .expect("migrations dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_drop_accounts"))
        })
        .expect("drop_accounts migration written");
    let up = std::fs::read_to_string(mig_dir.join("up.sql")).expect("up.sql");
    let down = std::fs::read_to_string(mig_dir.join("down.sql")).expect("down.sql");
    for (leg, sql) in [("up", &up), ("down", &down)] {
        assert!(
            !sql.to_lowercase().contains("sqlite_"),
            "the {leg} leg must emit no `sqlite_`-named statement (un-creatable):\n{sql}"
        );
    }
    assert!(
        down.contains("CREATE TABLE accounts"),
        "the down leg restores the accounts table: {down}"
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

/// CONTRACT (Codex review): a generated column (`GENERATED ALWAYS AS (...)
/// STORED`/`VIRTUAL`) must NOT be silently dropped by the pull — `pragma_table_info`
/// omits generated columns (a fail-closed violation), so the pull reads
/// `pragma_table_xinfo` and preserves them.
#[test]
fn schema_pull_preserves_generated_columns() {
    let (_tmp, project) = fresh_project("pull_sqlite_generated");
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_generated",
        "CREATE TABLE people (\
           id INTEGER PRIMARY KEY AUTOINCREMENT, \
           first TEXT NOT NULL, \
           last TEXT NOT NULL, \
           full_stored TEXT GENERATED ALWAYS AS (first || ' ' || last) STORED, \
           full_virtual TEXT GENERATED ALWAYS AS (first || ' ' || last) VIRTUAL);\n",
        "DROP TABLE people;\n",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);
    run_autumn_ok(&project, &["schema", "pull"], &envs);

    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    let people = table_json(&snap, "people");
    for gen_col in ["full_stored", "full_virtual"] {
        // `column_json` panics if the column is absent — asserting it is preserved.
        let _ = column_json(&people, gen_col);
    }

    // A re-pull is byte-for-byte stable, and doctor sees no drift.
    run_autumn_ok(&project, &["schema", "pull"], &envs);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("second pull");
    assert_eq!(snap, snap_after, "a re-pull is byte-stable");
    let (doc_out, _) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor is clean with generated columns preserved:\n{doc_out}"
    );
}

/// CONTRACT (Codex review): a retained `SQLite` partial index references its
/// WHERE-predicate column only in its verbatim `definition`. Dropping that column
/// must PRUNE the index (not leave an orphan that references a missing column), so
/// the generated table-rebuild migration APPLIES cleanly — `SQLite` would otherwise
/// reject the rebuild that recreates `... WHERE deleted_at IS NULL` on a table with
/// no `deleted_at`.
#[test]
fn schema_pull_dropping_partial_index_predicate_column_prunes_the_index() {
    let (_tmp, project) = fresh_project("pull_sqlite_partial_idx");
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_partial",
        "CREATE TABLE notes (\
           id INTEGER PRIMARY KEY AUTOINCREMENT, \
           email TEXT NOT NULL, \
           deleted_at TEXT, \
           created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);\n\
         CREATE INDEX notes_email_active ON notes (email) WHERE deleted_at IS NULL;\n",
        "DROP TABLE notes;\n",
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);
    run_autumn_ok(&project, &["schema", "pull"], &envs);

    // A model that DROPS `deleted_at` (the partial index's predicate column). The
    // model cannot express the partial index, so it is retained — and the column drop
    // must prune it during the rebuild.
    write_models(
        &project,
        "#[autumn_web::model(managed)]\npub struct Note {\n    #[id]\n    pub id: i64,\n    \
         pub email: String,\n}\n",
    );
    run_autumn_ok(
        &project,
        &[
            "schema",
            "diff",
            "--write-migration",
            "--name",
            "drop_deleted_at",
            "--allow-destructive",
        ],
        &envs,
    );

    // The generated up.sql must NOT recreate the orphaned partial index.
    let mig_dir = std::fs::read_dir(project.join("migrations"))
        .expect("migrations dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_drop_deleted_at"))
        })
        .expect("drop migration written");
    let up = std::fs::read_to_string(mig_dir.join("up.sql")).expect("up.sql");
    // The orphaned partial index is DROPPED (before the column drop), never left
    // referencing the missing column nor recreated with its `WHERE deleted_at` clause.
    assert!(
        up.contains("DROP INDEX") && up.contains("notes_email_active"),
        "the partial index depending on the dropped column is dropped first:\n{up}"
    );
    assert!(
        !up.contains("CREATE INDEX \"notes_email_active\"")
            && !up.contains("CREATE INDEX notes_email_active"),
        "the orphaned partial index is NOT recreated:\n{up}"
    );
    // `DROP INDEX` must precede the `DROP COLUMN` (SQLite rejects DROP COLUMN while a
    // live index references the column).
    if let (Some(di), Some(dc)) = (up.find("DROP INDEX"), up.find("DROP COLUMN")) {
        assert!(di < dc, "DROP INDEX must come before DROP COLUMN:\n{up}");
    }

    // THE PROOF: the rebuild migration APPLIES cleanly (an orphaned partial index
    // referencing the missing `deleted_at` would make SQLite reject the rebuild).
    run_autumn_ok(&project, &["schema", "migrate"], &envs);
    let (doc_out, _) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor is clean after the column drop + index prune:\n{doc_out}"
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

/// CONTRACT (Codex review): the create-on-open guard must also cover `file:` URIs —
/// a `file:` URI with a MISSING path must error (not create-on-open + clobber the
/// snapshot), while a `file:` URI pointing at a REAL DB pulls normally.
#[test]
fn schema_pull_file_uri_missing_path_errors_but_real_path_pulls() {
    let (_tmp, project) = fresh_project("pull_sqlite_file_uri");
    let db_path = project.join("app.db");
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // First build a real DB via a bare-path migration.
    let bare_url = format!("sqlite://{}", db_path.display());
    write_models(&project, "");
    write_raw_migration(
        &project,
        "20260101000000_one",
        "CREATE TABLE widgets (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL);\n",
        "DROP TABLE widgets;\n",
    );
    run_autumn_ok(
        &project,
        &["schema", "migrate"],
        &[("AUTUMN_DATABASE__URL", &bare_url)],
    );
    assert!(db_path.is_file(), "real DB created");

    // (a) A `file:` URI with a MISSING path must ERROR and not create the file.
    let missing = project.join("nope.db");
    let missing_uri = format!("file:{}", missing.display());
    std::fs::create_dir_all(project.join(".autumn")).expect("mkdir .autumn");
    let sentinel = "{\"backend\":\"Sqlite\",\"tables\":[{\"name\":\"sentinel\",\"managed\":true,\"columns\":[],\"indexes\":[],\"checks\":[],\"primary_key\":[]}]}";
    std::fs::write(&snapshot_path, sentinel).expect("seed snapshot");
    let (out, err, code) = run_autumn(
        &project,
        &["schema", "pull"],
        &[("AUTUMN_DATABASE__URL", &missing_uri)],
    );
    let combined = format!("{out}{err}");
    assert_ne!(
        code,
        Some(0),
        "a file: URI with a missing path must fail:\n{combined}"
    );
    assert!(
        combined.contains("could not open the SQLite database") && combined.contains("nope.db"),
        "the error names the missing file: {combined}"
    );
    assert!(
        !missing.exists(),
        "the missing file: URI DB must NOT be created"
    );
    assert_eq!(
        std::fs::read_to_string(&snapshot_path).expect("snapshot"),
        sentinel,
        "the snapshot must NOT be overwritten by the failed file: pull"
    );

    // (b) A `file:` URI pointing at the REAL DB pulls normally.
    let real_uri = format!("file:{}", db_path.display());
    let (pull_out, _) = run_autumn_ok(
        &project,
        &["schema", "pull"],
        &[("AUTUMN_DATABASE__URL", &real_uri)],
    );
    assert!(
        pull_out.contains("pulled schema snapshot") && pull_out.contains("1 table(s)"),
        "a file: URI at a real DB pulls: {pull_out}"
    );
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"name\": \"widgets\""),
        "widgets pulled: {snap}"
    );
}
