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
//! comments to a second model" the DSL token and nothing else. The SQL is
//! additionally written with `IF NOT EXISTS`, so even a project that hand-rolled
//! its own `comments` table earlier survives running the migration.

use std::path::Path;

use crate::generate::emit::Plan;

/// The shared comments table's name. Not configurable from the DSL token: the
/// `#[commentable(table = …)]` attribute is where a project renames it, and a
/// rename there is a migration the author writes anyway.
pub const COMMENTS_TABLE: &str = "comments";

/// The migration directory suffix, used both to name the directory and to
/// detect an existing one.
const MIGRATION_SUFFIX: &str = "_create_comments";

/// `up.sql` creating the polymorphic, threaded comments table.
///
/// `commentable_id` deliberately carries **no** `REFERENCES`: a single column
/// cannot reference two tables, which is the known trade-off of the polymorphic
/// pattern. The framework's write path is the referential check instead — it
/// probes and row-locks the parent before inserting — so an unknown parent is a
/// `404`, not a dangling row.
#[must_use]
pub fn up_sql() -> String {
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
         CREATE TABLE IF NOT EXISTS {COMMENTS_TABLE} (\n\
         \x20   id BIGSERIAL PRIMARY KEY,\n\
         \x20   commentable_type TEXT NOT NULL,\n\
         \x20   commentable_id BIGINT NOT NULL,\n\
         \x20   parent_id BIGINT REFERENCES {COMMENTS_TABLE}(id) ON DELETE CASCADE,\n\
         \x20   author_id BIGINT NOT NULL,\n\
         \x20   body TEXT NOT NULL,\n\
         \x20   created_at TIMESTAMP NOT NULL DEFAULT NOW(),\n\
         \x20   deleted_at TIMESTAMP\n\
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

/// Whether `project_root` already has a `*_create_comments` migration.
///
/// The table is shared, so the second (and third, and tenth) commentable model
/// must not recreate it. A directory scan rather than a marker file because the
/// migration is an ordinary directory the author may have renamed, moved, or
/// hand-written — matching on the suffix finds all of those.
#[must_use]
pub fn already_migrated(project_root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(project_root.join("migrations")) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(MIGRATION_SUFFIX))
    })
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
    plan.create(dir.join("up.sql"), up_sql());
    plan.create(dir.join("down.sql"), down_sql());
    true
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

    #[test]
    fn up_sql_declares_the_polymorphic_key_and_the_threading_column() {
        let sql = up_sql();
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS comments"));
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

    #[test]
    fn already_migrated_finds_a_renamed_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let migrations = tmp.path().join("migrations");
        std::fs::create_dir_all(migrations.join("00000000000000_create_comments")).expect("mkdir");
        assert!(already_migrated(tmp.path()));

        let empty = tempfile::tempdir().expect("tempdir");
        assert!(!already_migrated(empty.path()));
    }
}
