//! Scaffold support for threaded, polymorphic comments (issue #1367).
//!
//! `autumn generate scaffold post title:string comments:commentable` adds, on
//! top of the ordinary scaffold:
//!
//! 1. `comment_count BIGINT NOT NULL DEFAULT 0` on the scaffolded model's own
//!    table — handled by the ordinary field pipeline, because the DSL token
//!    *is* that column (see [`super::dsl::FieldKind::Commentable`]);
//! 2. `#[commentable(by = User, counter_cache = comment_count)]` on the
//!    generated `#[model]` — emitted by [`super::model`]; and
//! 3. the **shared** `comments` table migration this module owns.
//!
//! # The comments table is shared, so it is emitted at most once
//!
//! That is the whole point of the polymorphic kind: a second commentable model
//! attaches to the same table under a different `commentable_type`. So this
//! module first looks for an existing `*_create_comments` migration in the
//! project and emits nothing when it finds one — which is what makes "add
//! comments to a second model" the DSL token and nothing else.
//!
//! The `CREATE TABLE` is deliberately **not** `IF NOT EXISTS`. A project that
//! already has an unrelated `comments` table (a `Comment` resource scaffolded
//! the ordinary way, say) is a real conflict the author has to resolve, and
//! `IF NOT EXISTS` would turn it into a silent no-op whose only symptom is a
//! `column "commentable_type" does not exist` at request time. Failing the
//! migration says so at `migrate`, where it is fixable.

use std::path::Path;

use crate::generate::emit::Plan;

/// The shared comments table's name. Not configurable from the DSL token: the
/// `#[commentable(table = …)]` attribute is where a project renames it, and a
/// rename there is a migration the author writes anyway.
pub const COMMENTS_TABLE: &str = "comments";

/// The migration directory suffix, used both to name the directory and to
/// detect an existing one.
const MIGRATION_SUFFIX: &str = "_create_comments";

/// The backend-forked column spellings the shared comments table needs.
///
/// The scaffold's *own* migration is already backend-aware (issue #1614), so
/// this one has to be too, or a `SQLite` project would take `comments:commentable`
/// happily and then fail `diesel migration run` on `BIGSERIAL`. Mirrors
/// [`super::auth`]'s `AuthDdl`, which forks the same three spellings for the same
/// reason.
struct CommentsDdl {
    pk: &'static str,
    big_int: &'static str,
    ts: &'static str,
    ts_not_null_default_now: &'static str,
}

impl CommentsDdl {
    const fn for_backend(backend: autumn_web::config::DatabaseBackend) -> Self {
        match backend {
            autumn_web::config::DatabaseBackend::Postgres => Self {
                pk: "BIGSERIAL PRIMARY KEY",
                big_int: "BIGINT",
                ts: "TIMESTAMP",
                ts_not_null_default_now: "TIMESTAMP NOT NULL DEFAULT NOW()",
            },
            autumn_web::config::DatabaseBackend::Sqlite => Self {
                pk: "INTEGER PRIMARY KEY AUTOINCREMENT",
                big_int: "INTEGER",
                ts: "TEXT",
                ts_not_null_default_now: "TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP",
            },
        }
    }
}

/// `up.sql` creating the polymorphic, threaded comments table for `backend`.
///
/// `commentable_id` deliberately carries **no** `REFERENCES`: a single column
/// cannot reference two tables, which is the known trade-off of the polymorphic
/// pattern. The framework's write path is the referential check instead — it
/// probes and row-locks the parent before inserting — so an unknown parent is a
/// `404`, not a dangling row.
#[must_use]
pub fn up_sql(backend: autumn_web::config::DatabaseBackend) -> String {
    let CommentsDdl {
        pk,
        big_int,
        ts,
        ts_not_null_default_now,
    } = CommentsDdl::for_backend(backend);
    format!(
        "-- Threaded, polymorphic comments (issue #1367).\n\
         --\n\
         -- ONE table serves every `#[commentable]` model: `commentable_type` holds the\n\
         -- model's name and `commentable_id` its primary key. `commentable_id` carries no\n\
         -- REFERENCES because a single column cannot reference two tables -- the framework\n\
         -- probes and row-locks the parent before every insert instead.\n\
         --\n\
         -- `author_id` has no REFERENCES either, for a different reason: the author model\n\
         -- is named by `#[commentable(by = ...)]` and this migration does not know which\n\
         -- table that is. Add `REFERENCES users(id)` (or your own author table) once it\n\
         -- exists -- it is worth having.\n\
         CREATE TABLE {COMMENTS_TABLE} (\n\
         \x20   id {pk},\n\
         \x20   commentable_type TEXT NOT NULL,\n\
         \x20   commentable_id {big_int} NOT NULL,\n\
         \x20   parent_id {big_int} REFERENCES {COMMENTS_TABLE}(id) ON DELETE CASCADE,\n\
         \x20   author_id {big_int} NOT NULL,\n\
         \x20   body TEXT NOT NULL,\n\
         \x20   created_at {ts_not_null_default_now},\n\
         \x20   deleted_at {ts}\n\
         );\n\
         \n\
         -- Covers the thread read whole: its WHERE is the discriminator pair and its\n\
         -- ORDER BY is (created_at, id), so one index serves both halves.\n\
         CREATE INDEX IF NOT EXISTS idx_{COMMENTS_TABLE}_thread\n\
         \x20   ON {COMMENTS_TABLE} (commentable_type, commentable_id, created_at, id);\n\
         \n\
         -- The delete cascade walks children by parent_id.\n\
         CREATE INDEX IF NOT EXISTS idx_{COMMENTS_TABLE}_parent_id\n\
         \x20   ON {COMMENTS_TABLE} (parent_id);\n"
    )
}

/// `down.sql` dropping it again.
#[must_use]
pub fn down_sql() -> String {
    format!("DROP TABLE IF EXISTS {COMMENTS_TABLE};\n")
}

/// The migration directory name, e.g.
/// `202604270000002_create_comments`.
///
/// Diesel takes the prefix as the migration **version**, and the scaffold has
/// already claimed `{timestamp}` for its own `{timestamp}_create_{table}` and
/// `{timestamp}1` for a `--counter-cache` column (see
/// [`super::counter_cache::migration_dir_name`]). Appending `2` keeps all three
/// distinct while never consuming a wall-clock version a scaffold run one
/// second later would take: `MigrationVersion` is `Ord` over the raw string, a
/// prefix sorts first, and the appended digit sits beyond the index at which
/// `{timestamp}` and `{timestamp + 1}` first differ.
#[must_use]
pub fn migration_dir_name(timestamp: &str) -> String {
    format!("{timestamp}2{MIGRATION_SUFFIX}")
}

/// Whether `project_root` already has a migration creating the **polymorphic**
/// comments table.
///
/// The table is shared, so the second (and third, and tenth) commentable model
/// must not recreate it. Detection is by `up.sql` **content**, not by the
/// directory name: `autumn generate scaffold Comment body:Text` produces a
/// directory called `{timestamp}_create_comments` too, and matching that name
/// would make a later `comments:commentable` skip the shared table while
/// cheerfully reporting it was reused — leaving every `add_comment` to fail at
/// runtime with `42703 column "commentable_type" does not exist`. The
/// discriminator column is the thing that actually distinguishes the two, so
/// that is what is matched.
#[must_use]
pub fn already_migrated(project_root: &Path) -> bool {
    // Replayed in version order, because a migration history is a sequence of
    // edits and not a bag of facts. Flags that only ever get SET would report a
    // table that a later `DROP TABLE comments` removed as still present -- the
    // generator would skip recreating it, and every helper would fail at
    // runtime on a table that is not there.
    // One running picture of the table: does it exist, and does it currently
    // have each discriminator column. Every event edits that picture, so a
    // column added by a CREATE body and one added by a later ALTER are the same
    // kind of fact -- which they are, and treating them differently is what let
    // a rename INTO the discriminator name go unrecognised.
    let mut creates = false;
    let (mut has_type, mut has_id) = (false, false);

    for sql in migration_up_sql(project_root) {
        for event in comments_table_events(&sql) {
            match event {
                CommentsEvent::Create {
                    has_type: created_type,
                    has_id: created_id,
                } => {
                    creates = true;
                    has_type = created_type;
                    has_id = created_id;
                }
                CommentsEvent::AddType => has_type = true,
                CommentsEvent::AddId => has_id = true,
                CommentsEvent::DropType => has_type = false,
                CommentsEvent::DropId => has_id = false,
                CommentsEvent::Drop => {
                    creates = false;
                    has_type = false;
                    has_id = false;
                }
            }
        }
    }

    creates && has_type && has_id
}

/// Whether `haystack` mentions `column` as a complete SQL identifier.
///
/// A bare `contains` would accept `legacy_commentable_type` as
/// `commentable_type`, classify a table that lacks the real columns as
/// polymorphic, and make the generator skip the shared migration while
/// reporting that it reused the table — with every helper then failing at
/// runtime on columns that are not there. The same identifier-boundary rule the
/// table name gets; columns had been left as substrings.
fn mentions_column(haystack: &str, column: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut base = 0usize;
    while let Some(at) = haystack[base..].find(column) {
        let start = base + at;
        let end = start + column.len();
        let before_ok = start == 0 || !haystack[..start].ends_with(is_ident);
        let after_ok = !haystack[end..].starts_with(is_ident);
        if before_ok && after_ok {
            return true;
        }
        base = end;
    }
    false
}

/// What one statement does to the shared comments table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentsEvent {
    /// `CREATE TABLE comments (…)`, and which discriminator columns its body
    /// declares. A fresh table replaces whatever was known about the old one.
    Create { has_type: bool, has_id: bool },
    /// `ALTER TABLE comments … commentable_type`.
    AddType,
    /// `ALTER TABLE comments … commentable_id`.
    AddId,
    /// `DROP TABLE comments`.
    Drop,
    /// `ALTER TABLE comments DROP COLUMN commentable_type` (or a rename away).
    DropType,
    /// `ALTER TABLE comments DROP COLUMN commentable_id` (or a rename away).
    DropId,
}

/// Whether an `ALTER TABLE comments …` statement takes `column` away.
///
/// `DROP COLUMN <column>` removes it outright; `RENAME COLUMN <column> TO …`
/// removes it under that name, which is all this scan cares about. A
/// `RENAME COLUMN <other> TO <column>` is the opposite and must not count as a
/// removal, so the position of the name relative to `to` decides.
fn alter_removes_column(statement: &str, column: &str) -> bool {
    if statement.contains("drop column") || statement.contains("drop if exists") {
        // `ADD COLUMN a, DROP COLUMN b` is legal in one statement, so the
        // column has to be the one being dropped.
        if let Some(drop_at) = statement.find("drop column") {
            return mentions_column(&statement[drop_at..], column);
        }
        return mentions_column(statement, column);
    }
    if let Some(rename_at) = statement.find("rename column") {
        let rest = &statement[rename_at..];
        let (from, to) = rest.split_once(" to ").unwrap_or((rest, ""));
        return mentions_column(from, column) && !mentions_column(to, column);
    }
    false
}

/// Every statement in `sql` touching the shared comments table, **in source
/// order**, so a create and a later drop in one file are seen as a sequence.
fn comments_table_events(sql: &str) -> Vec<CommentsEvent> {
    let mut events: Vec<(usize, CommentsEvent)> = Vec::new();

    if let Some(start) = comments_table_statement_start(sql) {
        let body = comments_table_body(sql).unwrap_or("");
        events.push((
            start,
            CommentsEvent::Create {
                has_type: mentions_column(body, "commentable_type"),
                has_id: mentions_column(body, "commentable_id"),
            },
        ));
    }
    for (at, statement) in comments_statements(sql, "alter table") {
        // An ALTER naming the column may be adding it, dropping it, or renaming
        // it away. Treating every mention as an add would let
        // `DROP COLUMN commentable_type` read as proof the column is present.
        for (column, added, removed) in [
            (
                "commentable_type",
                CommentsEvent::AddType,
                CommentsEvent::DropType,
            ),
            (
                "commentable_id",
                CommentsEvent::AddId,
                CommentsEvent::DropId,
            ),
        ] {
            if !mentions_column(statement, column) {
                continue;
            }
            if alter_removes_column(statement, column) {
                events.push((at, removed));
            } else {
                events.push((at, added));
            }
        }
    }
    for (at, _) in comments_statements(sql, "drop table") {
        events.push((at, CommentsEvent::Drop));
    }

    events.sort_by_key(|(at, _)| *at);
    events.into_iter().map(|(_, event)| event).collect()
}

/// Statements of the form `<verb> [if exists] comments …`, as (offset, body).
///
/// Identifier-exact, so `comments_archive` is never mistaken for `comments`.
fn comments_statements<'a>(lowered: &'a str, verb: &str) -> Vec<(usize, &'a str)> {
    let mut found = Vec::new();
    for prefix in [
        format!("{verb} {COMMENTS_TABLE}"),
        format!("{verb} \"{COMMENTS_TABLE}\""),
        format!("{verb} if exists {COMMENTS_TABLE}"),
        format!("{verb} if exists \"{COMMENTS_TABLE}\""),
    ] {
        let mut base = 0usize;
        while let Some(at) = lowered[base..].find(&prefix) {
            let start = base + at;
            let after = &lowered[start + prefix.len()..];
            if prefix.ends_with('"')
                || after.is_empty()
                || after.starts_with(|c: char| c.is_whitespace() || c == ';')
            {
                found.push((start, after.split(';').next().unwrap_or(after)));
            }
            base = start + prefix.len();
        }
    }
    found
}

/// Every migration's `up.sql`, lowercased with SQL comments stripped.
fn migration_up_sql(project_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(project_root.join("migrations")) else {
        return Vec::new();
    };
    // Sorted by directory name, which carries the timestamp prefix diesel
    // orders by. Read order from the filesystem is unspecified, and this scan
    // now depends on sequence: a create followed by a drop is not the same
    // history as a drop followed by a create.
    let mut dirs: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .filter_map(|dir| std::fs::read_to_string(dir.join("up.sql")).ok())
        .map(|sql| strip_sql_comments(&sql.to_ascii_lowercase()))
        .collect()
}

/// `sql` with `--` line comments and `/* … */` blocks removed.
///
/// Matching runs over raw text, so a commented-out example — `-- CREATE TABLE
/// comments (commentable_type …)` in a migration's header, which is exactly the
/// kind of thing a header explaining the shared table would contain — would
/// otherwise read as the real table and make the generator emit nothing.
fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    loop {
        let line = rest.find("--");
        let block = rest.find("/*");
        match (line, block) {
            (None, None) => {
                out.push_str(rest);
                return out;
            }
            // Whichever opens first wins; a `--` inside a block comment (and a
            // `/*` inside a line comment) is just text.
            (Some(l), b) if b.is_none_or(|b| l < b) => {
                out.push_str(&rest[..l]);
                rest = rest[l..].find('\n').map_or("", |nl| &rest[l + nl..]);
            }
            (_, Some(b)) => {
                out.push_str(&rest[..b]);
                rest = rest[b + 2..]
                    .find("*/")
                    .map_or("", |end| &rest[b + 2 + end + 2..]);
            }
            (Some(_), None) => unreachable!("the guard above covers a line comment first"),
        }
    }
}

/// The column list of a `CREATE TABLE` naming **exactly** the shared comments
/// table, or `None` when `lowered` creates no such table.
///
/// Returns the text between the statement's outer parentheses, so callers can
/// ask what *this* table declares rather than what the file mentions anywhere.
fn comments_table_body(lowered: &str) -> Option<&str> {
    let start = comments_table_statement_start(lowered)?;
    let open = lowered[start..].find('(')? + start;
    // Balance parens: a column can carry its own, e.g. `NUMERIC(10, 2)`.
    let mut depth = 0usize;
    for (offset, ch) in lowered[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&lowered[open + 1..open + offset]);
                }
            }
            _ => {}
        }
    }
    // Unbalanced SQL: treat the rest of the file as the body rather than
    // silently reporting "no such table" for a migration that does create one.
    Some(&lowered[open + 1..])
}

/// Where a `CREATE TABLE` naming exactly the shared comments table begins.
///
/// A prefix match is not enough: `CREATE TABLE comments_archive (...)` carrying
/// the discriminator columns would satisfy every other check, and the generator
/// would then skip the real table while reporting that it was reused. The name
/// has to end where the identifier ends -- at whitespace, an opening paren, or
/// the closing quote of a quoted identifier.
fn comments_table_statement_start(lowered: &str) -> Option<usize> {
    // `create` is part of the match, not assumed: `DROP TABLE comments;`
    // followed by an archive table carrying the discriminator columns would
    // otherwise read as "the shared table is already here", and the generator
    // would emit nothing while the table it reported is gone.
    //
    // The name must also end where the identifier ends -- at whitespace, an
    // opening paren, or a closing quote -- so `comments_archive` is not
    // mistaken for `comments`.
    let mut best: Option<usize> = None;
    for prefix in [
        format!("create table if not exists {COMMENTS_TABLE}"),
        format!("create table if not exists \"{COMMENTS_TABLE}\""),
        format!("create table {COMMENTS_TABLE}"),
        format!("create table \"{COMMENTS_TABLE}\""),
    ] {
        let mut base = 0usize;
        while let Some(at) = lowered[base..].find(&prefix) {
            let start = base + at;
            let after = &lowered[start + prefix.len()..];
            if prefix.ends_with('"')
                || after.is_empty()
                || after.starts_with(|c: char| c.is_whitespace() || c == '(' || c == ';')
            {
                // Earliest match wins, so the body belongs to the first such
                // statement in the file.
                best = Some(best.map_or(start, |b: usize| b.min(start)));
                break;
            }
            base = start + prefix.len();
        }
    }
    best
}

/// Push the shared comments migration onto `plan`, unless the project already
/// has one.
///
/// Pushed on a **revert** plan too, and that is load-bearing: `Plan::revert`
/// discovers the migration directories it may remove from the plan's `Create`
/// actions under `migrations/`, so omitting it would leave the directory behind
/// after `autumn destroy scaffold`.
///
/// Returns whether the migration was emitted, so the caller can surface the
/// "already there, reusing it" case as a warning rather than silence.
pub fn push_commentable_migration(
    plan: &mut Plan,
    project_root: &Path,
    timestamp: &str,
    backend: autumn_web::config::DatabaseBackend,
    for_revert: bool,
) -> bool {
    // On a revert plan the directory is (by construction) already on disk from
    // the generate run being undone, so `already_migrated` would always say
    // "skip" and the revert would never take it back out.
    if !for_revert && already_migrated(project_root) {
        return false;
    }
    let dir = project_root
        .join("migrations")
        .join(migration_dir_name(timestamp));
    plan.create(dir.join("up.sql"), up_sql(backend));
    plan.create(dir.join("down.sql"), down_sql());
    true
}

/// Whether **another** model in `project_root` still declares
/// `#[commentable]`, ignoring `destroying_model`.
///
/// The comments table is shared, so `autumn destroy scaffold Post` in a project
/// where `Photo` is also commentable must NOT take the migration with it —
/// `Plan::revert` finds migration directories to remove from the plan's own
/// `Create` actions, so without this guard destroying one model silently breaks
/// every other one. Mirrors the `mail_unsubscribes_migration_still_needed_elsewhere`
/// guard in [`super::emit`], which exists for the same reason.
#[must_use]
pub fn another_model_is_still_commentable(project_root: &Path, destroying_model: &str) -> bool {
    let models_dir = project_root.join("src").join("models");
    let destroying_file = format!("{destroying_model}.rs");

    // Per-file layout (`src/models/<snake>.rs`), the one the generators emit.
    if let Ok(entries) = std::fs::read_dir(&models_dir) {
        for entry in entries.filter_map(Result::ok) {
            if entry.file_name().to_str() == Some(destroying_file.as_str()) {
                continue;
            }
            if std::fs::read_to_string(entry.path()).is_ok_and(|src| src.contains("#[commentable"))
            {
                return true;
            }
        }
    }

    // Single-file layout (`src/models.rs`), which hand-written apps use. The
    // file being destroyed from is the same file, so count declarations: more
    // than one means somebody else still needs the table.
    std::fs::read_to_string(project_root.join("src").join("models.rs"))
        .is_ok_and(|src| src.matches("#[commentable").count() > 1)
}

/// Whether the generated model for `snake_name` declares `#[commentable]`.
///
/// `destroy scaffold Post` is typed without the field tokens the generate run
/// carried, so the revert plan cannot learn from its arguments that this model
/// brought the shared comments table. It can read the model file, which is
/// still on disk when the plan is computed — the same "recover it from what was
/// written" move the nested-resource revert makes.
#[must_use]
pub fn model_declares_commentable(project_root: &Path, snake_name: &str) -> bool {
    let src = project_root.join("src");
    if let Ok(per_file) =
        std::fs::read_to_string(src.join("models").join(format!("{snake_name}.rs")))
    {
        return per_file.contains("#[commentable");
    }
    false
}

/// The app's author model, for `#[commentable(by = <Model>)]`.
///
/// `by` is what lets the generated code resolve an author display name, and a
/// typo'd (or absent) model name would be a compile error in a file the author
/// did not write — so it is emitted **only** when the model actually exists.
/// `autumn generate auth` produces `src/models/user.rs`; a project that keeps
/// its models in one `src/models.rs` is matched by the struct declaration.
/// Anything else gets a bare `#[commentable]`, which compiles, plus a warning
/// naming the one word to add.
#[must_use]
pub fn detect_author_model(project_root: &Path) -> Option<&'static str> {
    let src = project_root.join("src");
    if src.join("models").join("user.rs").is_file() {
        return Some("User");
    }
    let single_file = std::fs::read_to_string(src.join("models.rs")).ok()?;
    single_file.contains("pub struct User ").then_some("User")
}

#[cfg(test)]
mod tests {
    use super::*;

    use autumn_web::config::DatabaseBackend;

    #[test]
    fn up_sql_declares_the_polymorphic_key_and_the_threading_column() {
        let sql = up_sql(DatabaseBackend::Postgres);
        assert!(sql.contains("CREATE TABLE comments"));
        assert!(
            !sql.contains("CREATE TABLE IF NOT EXISTS"),
            "a colliding table is a conflict to resolve, not a silent no-op"
        );
        assert!(sql.contains("commentable_type TEXT NOT NULL"));
        assert!(sql.contains("commentable_id BIGINT NOT NULL"));
        assert!(sql.contains("parent_id BIGINT REFERENCES comments(id) ON DELETE CASCADE"));
        assert!(sql.contains("deleted_at TIMESTAMP"));
        assert!(sql.contains("(commentable_type, commentable_id, created_at, id)"));
        // The polymorphic column must NOT gain a foreign key — a single column
        // cannot reference two tables, and pretending otherwise would break the
        // second commentable model.
        assert!(!sql.contains("commentable_id BIGINT NOT NULL REFERENCES"));
    }

    /// A `SQLite` project takes the same token, so the shared table has to be
    /// spelled for it too — `BIGSERIAL`/`NOW()` would fail `diesel migration
    /// run` on the very first migrate.
    #[test]
    fn up_sql_is_spelled_for_sqlite_too() {
        let sql = up_sql(DatabaseBackend::Sqlite);
        assert!(!sql.contains("BIGSERIAL"), "{sql}");
        assert!(!sql.contains("NOW()"), "{sql}");
        assert!(sql.contains("INTEGER PRIMARY KEY AUTOINCREMENT"), "{sql}");
        assert!(sql.contains("DEFAULT CURRENT_TIMESTAMP"), "{sql}");
        // The polymorphic key and the threading self-FK are backend-independent.
        assert!(sql.contains("commentable_type TEXT NOT NULL"), "{sql}");
        assert!(sql.contains("commentable_id INTEGER NOT NULL"), "{sql}");
        assert!(
            sql.contains("parent_id INTEGER REFERENCES comments(id) ON DELETE CASCADE"),
            "{sql}"
        );
    }

    #[test]
    fn down_sql_is_idempotent() {
        assert!(down_sql().contains("DROP TABLE IF EXISTS comments"));
    }

    /// The version must sort after the scaffold's own and after a
    /// `--counter-cache` migration taken in the same run, and before the next
    /// second's scaffold.
    #[test]
    fn migration_version_sorts_between_this_second_and_the_next() {
        let this = "20260621000000";
        let next = "20260621000001";
        let ours = migration_dir_name(this);
        assert!(ours.starts_with(this));
        assert!(this < ours.as_str());
        assert!(format!("{this}1_add_comment_count_to_posts").as_str() < ours.as_str());
        assert!(ours.as_str() < next);
    }

    #[test]
    fn detect_author_model_finds_both_model_layouts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(detect_author_model(tmp.path()), None);

        let per_file = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(per_file.path().join("src/models")).expect("mkdir");
        std::fs::write(per_file.path().join("src/models/user.rs"), "").expect("write");
        assert_eq!(detect_author_model(per_file.path()), Some("User"));

        let single = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(single.path().join("src")).expect("mkdir");
        std::fs::write(
            single.path().join("src/models.rs"),
            "#[autumn_web::model]\npub struct User {}\n",
        )
        .expect("write");
        assert_eq!(detect_author_model(single.path()), Some("User"));
    }

    /// Detection is by content, so a renamed directory still counts…
    #[test]
    fn already_migrated_finds_a_renamed_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_comments_v2");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("up.sql"), up_sql(DatabaseBackend::Postgres)).expect("write");
        assert!(already_migrated(tmp.path()));

        let empty = tempfile::tempdir().expect("tempdir");
        assert!(!already_migrated(empty.path()));
    }

    /// …and a `Comment` resource scaffolded the ordinary way does NOT, even
    /// though it produces a directory with the very same name. Matching on the
    /// name alone would skip the shared table and then fail at runtime on the
    /// missing discriminator columns.
    #[test]
    fn a_scaffolded_comment_model_does_not_look_like_the_shared_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp
            .path()
            .join("migrations")
            .join("20260820000000_create_comments");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             body TEXT NOT NULL,\n    post_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "a post_id-keyed comments table is not the polymorphic one"
        );
    }

    /// An unrelated *idempotent* `comments` table is not this migration either.
    /// The discriminator pair is what identifies it, whichever spelling of
    /// `CREATE TABLE` the file uses.
    #[test]
    fn an_unrelated_idempotent_comments_table_is_not_the_shared_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_create_comments");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE IF NOT EXISTS comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             body TEXT NOT NULL,\n    post_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "IF NOT EXISTS does not make an unrelated table polymorphic"
        );
    }

    /// A table whose name merely *starts* with `comments` is a different
    /// table, even when it carries the discriminator columns.
    #[test]
    fn a_comments_prefixed_table_is_not_the_shared_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_archive");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments_archive (\n    id BIGSERIAL PRIMARY KEY,\n    \
             commentable_type TEXT NOT NULL,\n    commentable_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "`comments_archive` is not `comments`"
        );

        // …while the real thing, quoted or not, still is.
        for sql in [
            "CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT);",
            "CREATE TABLE \"comments\" (commentable_type TEXT, commentable_id BIGINT);",
            "CREATE TABLE IF NOT EXISTS comments(commentable_type TEXT, commentable_id BIGINT);",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let dir = tmp.path().join("migrations").join("0001_real");
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("up.sql"), sql).expect("write");
            assert!(already_migrated(tmp.path()), "{sql}");
        }
    }

    /// A migration that *drops* the comments table has not created it, however
    /// many discriminator columns appear later in the file.
    #[test]
    fn dropping_the_comments_table_is_not_creating_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_retire");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "DROP TABLE comments;\nCREATE TABLE comments_archive (\n    \
             commentable_type TEXT NOT NULL,\n    commentable_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "a dropped table is not a reusable one"
        );
    }

    /// A commented-out example is not a table. A migration header explaining
    /// the shared table is a realistic place to find exactly this.
    #[test]
    fn a_commented_out_create_table_is_not_the_shared_one() {
        for sql in [
            "-- CREATE TABLE comments (\n--   commentable_type TEXT NOT NULL,\n\
             --   commentable_id BIGINT NOT NULL\n-- );\nCREATE TABLE notes (id BIGSERIAL);\n",
            "/* CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT); */\n\
             CREATE TABLE notes (id BIGSERIAL);\n",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let dir = tmp.path().join("migrations").join("0001_notes");
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("up.sql"), sql).expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "a commented-out CREATE is not the shared table:\n{sql}"
            );
        }

        // The real migration still registers, comments in its header and all.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_real");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("up.sql"), up_sql(DatabaseBackend::Postgres)).expect("write");
        assert!(already_migrated(tmp.path()));
    }

    /// The discriminator columns have to belong to the `comments` statement
    /// itself. A file that creates an ordinary `comments` table next to an
    /// unrelated table carrying those columns is not the shared migration.
    #[test]
    fn discriminator_columns_on_another_table_do_not_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_mixed");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             body TEXT NOT NULL,\n    post_id BIGINT NOT NULL\n);\n\
             CREATE TABLE audit_log (\n    id BIGSERIAL PRIMARY KEY,\n    \
             commentable_type TEXT NOT NULL,\n    commentable_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "the discriminator columns belong to audit_log, not to comments"
        );
    }

    /// …and a column list with its own parentheses still parses.
    #[test]
    fn a_nested_paren_in_the_column_list_does_not_truncate_the_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_nested");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             score NUMERIC(10, 2) NOT NULL DEFAULT 0,\n    \
             commentable_type TEXT NOT NULL,\n    commentable_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "NUMERIC(10, 2) must not end the column list early"
        );
    }

    /// A project that CONVERTED an existing `comments` table to polymorphic
    /// storage did it across two migrations: one created the table, a later one
    /// added the discriminator columns by `ALTER TABLE`. `examples/reddit-clone`
    /// is exactly this shape. Requiring both in one file would emit a second
    /// `CREATE TABLE comments` and fail the next `migrate`.
    #[test]
    fn a_table_made_polymorphic_by_a_later_migration_counts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("20260419000000_create_app");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(
            created.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             body TEXT NOT NULL,\n    post_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");

        // The CREATE alone is not the shared table…
        assert!(!already_migrated(tmp.path()));

        let converted = migrations.join("20260820000000_polymorphic_comments");
        std::fs::create_dir_all(&converted).expect("mkdir");
        std::fs::write(
            converted.join("up.sql"),
            "ALTER TABLE comments ADD COLUMN commentable_type TEXT;\n\
             ALTER TABLE comments ADD COLUMN commentable_id BIGINT;\n",
        )
        .expect("write");

        // …but the accumulated history is.
        assert!(
            already_migrated(tmp.path()),
            "a comments table converted by a later ALTER is still the shared table"
        );
    }

    /// …and an `ALTER` on a *different* table does not convert `comments`.
    #[test]
    fn altering_another_table_does_not_make_comments_polymorphic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let dir = migrations.join("0001_app");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL);\n\
             ALTER TABLE comments_archive ADD COLUMN commentable_type TEXT;\n\
             ALTER TABLE comments_archive ADD COLUMN commentable_id BIGINT;\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "comments_archive is not comments"
        );
    }

    /// Nothing requires a conversion to add both discriminator columns in the
    /// same migration, so the two ALTERs have to accumulate across the history
    /// as well.
    #[test]
    fn discriminator_columns_altered_in_separate_migrations_still_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        for (dir, sql) in [
            (
                "0001_create",
                "CREATE TABLE comments (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL);\n",
            ),
            (
                "0002_add_type",
                "ALTER TABLE comments ADD COLUMN commentable_type TEXT;\n",
            ),
        ] {
            let path = migrations.join(dir);
            std::fs::create_dir_all(&path).expect("mkdir");
            std::fs::write(path.join("up.sql"), sql).expect("write");
        }

        // Only one of the two columns so far.
        assert!(!already_migrated(tmp.path()));

        let path = migrations.join("0003_add_id");
        std::fs::create_dir_all(&path).expect("mkdir");
        std::fs::write(
            path.join("up.sql"),
            "ALTER TABLE comments ADD COLUMN commentable_id BIGINT;\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "the columns may arrive in separate migrations"
        );
    }

    /// A history is a sequence, not a bag of facts: a later `DROP TABLE
    /// comments` undoes an earlier polymorphic create, and the generator must
    /// emit the table again rather than skip it.
    #[test]
    fn a_later_drop_undoes_an_earlier_polymorphic_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("20260101000000_create");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(created.join("up.sql"), up_sql(DatabaseBackend::Postgres)).expect("write");
        assert!(
            already_migrated(tmp.path()),
            "the table exists at this point"
        );

        let dropped = migrations.join("20260202000000_retire");
        std::fs::create_dir_all(&dropped).expect("mkdir");
        std::fs::write(dropped.join("up.sql"), "DROP TABLE comments;\n").expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "a dropped table is not available to reuse"
        );

        // …and recreating it later brings it back.
        let again = migrations.join("20260303000000_recreate");
        std::fs::create_dir_all(&again).expect("mkdir");
        std::fs::write(again.join("up.sql"), up_sql(DatabaseBackend::Postgres)).expect("write");
        assert!(already_migrated(tmp.path()));
    }

    /// Order matters *within* a file too, not only between them.
    #[test]
    fn a_drop_after_a_create_in_one_file_leaves_no_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_churn");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT);\n\
             DROP TABLE comments;\n",
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()));

        // The reverse order does leave one.
        std::fs::write(
            dir.join("up.sql"),
            "DROP TABLE IF EXISTS comments;\n\
             CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT);\n",
        )
        .expect("write");
        assert!(already_migrated(tmp.path()));
    }

    /// Column names need the same identifier-boundary rule the table name
    /// gets: `legacy_commentable_type` is not `commentable_type`.
    #[test]
    fn similarly_named_columns_do_not_look_like_the_discriminator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("migrations").join("0001_legacy");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n    \
             legacy_commentable_type TEXT NOT NULL,\n    \
             legacy_commentable_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");
        assert!(
            !already_migrated(tmp.path()),
            "legacy_* columns are not the discriminator pair"
        );

        // A trailing suffix is no better than a leading one.
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    commentable_type_old TEXT,\n    \
             commentable_id_old BIGINT\n);\n",
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()));

        // …and the real columns still register, quoted or not.
        std::fs::write(
            dir.join("up.sql"),
            "CREATE TABLE comments (\n    \"commentable_type\" TEXT NOT NULL,\n    \
             commentable_id BIGINT NOT NULL\n);\n",
        )
        .expect("write");
        assert!(already_migrated(tmp.path()));
    }

    /// An ALTER naming a discriminator column may be REMOVING it. Treating
    /// every mention as an add would let a dropped column read as present.
    #[test]
    fn dropping_or_renaming_a_discriminator_column_undoes_it() {
        let base = "CREATE TABLE comments (commentable_type TEXT, commentable_id BIGINT);\n";
        for removal in [
            "ALTER TABLE comments DROP COLUMN commentable_type;\n",
            "ALTER TABLE comments RENAME COLUMN commentable_type TO legacy_kind;\n",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let migrations = tmp.path().join("migrations");
            let created = migrations.join("0001_create");
            std::fs::create_dir_all(&created).expect("mkdir");
            std::fs::write(created.join("up.sql"), base).expect("write");
            assert!(already_migrated(tmp.path()));

            let changed = migrations.join("0002_change");
            std::fs::create_dir_all(&changed).expect("mkdir");
            std::fs::write(changed.join("up.sql"), removal).expect("write");
            assert!(
                !already_migrated(tmp.path()),
                "the table is no longer polymorphic after:\n{removal}"
            );
        }

        // A rename that CREATES the column is the opposite, and must count.
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        let created = migrations.join("0001_create");
        std::fs::create_dir_all(&created).expect("mkdir");
        std::fs::write(
            created.join("up.sql"),
            "CREATE TABLE comments (kind TEXT, commentable_id BIGINT);\n",
        )
        .expect("write");
        assert!(!already_migrated(tmp.path()));

        let renamed = migrations.join("0002_rename");
        std::fs::create_dir_all(&renamed).expect("mkdir");
        std::fs::write(
            renamed.join("up.sql"),
            "ALTER TABLE comments RENAME COLUMN kind TO commentable_type;\n",
        )
        .expect("write");
        assert!(
            already_migrated(tmp.path()),
            "a rename INTO the discriminator name adds it"
        );
    }

    /// The shared migration must survive `destroy scaffold` while any other
    /// model still declares `#[commentable]`.
    #[test]
    fn another_commentable_model_keeps_the_shared_migration_alive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let models = tmp.path().join("src").join("models");
        std::fs::create_dir_all(&models).expect("mkdir");
        std::fs::write(models.join("post.rs"), "#[commentable(by = User)]\n").expect("write");
        assert!(
            !another_model_is_still_commentable(tmp.path(), "post"),
            "the model being destroyed does not count as another one"
        );

        std::fs::write(models.join("photo.rs"), "#[commentable(by = User)]\n").expect("write");
        assert!(another_model_is_still_commentable(tmp.path(), "post"));

        // Single-file layout: two declarations in one file.
        let single = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(single.path().join("src")).expect("mkdir");
        std::fs::write(
            single.path().join("src").join("models.rs"),
            "#[commentable(by = User)]\npub struct Post {}\n",
        )
        .expect("write");
        assert!(!another_model_is_still_commentable(single.path(), "post"));
        std::fs::write(
            single.path().join("src").join("models.rs"),
            "#[commentable(by = User)]\npub struct Post {}\n\
             #[commentable(by = User)]\npub struct Photo {}\n",
        )
        .expect("write");
        assert!(another_model_is_still_commentable(single.path(), "post"));
    }
}
