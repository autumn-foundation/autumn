//! Scaffold support for declarative counter caches (issue #1325).
//!
//! `autumn generate scaffold comment body:text post:references --belongs-to Post
//! --counter-cache` adds, on top of the ordinary `--belongs-to` scaffold:
//!
//! 1. `#[belongs_to(Post, counter_cache)]` on the generated child model, which
//!    is what makes `PgCommentRepository` maintain the column; and
//! 2. a migration adding `comment_count BIGINT NOT NULL DEFAULT 0` to the
//!    parent's table, with a `DROP COLUMN` down.
//!
//! `NOT NULL DEFAULT 0` is load-bearing rather than tidy: the maintenance is
//! `SET c = c + 1`, and `NULL + 1` is `NULL`.
//!
//! The parent's `src/schema.rs` block and model struct also need the column, but
//! both are files this invocation does not own and neither has a marker-delimited
//! revert. Rather than make `autumn destroy scaffold` unable to take them back
//! out, the plan surfaces the two exact lines to paste as a warning.

use std::path::Path;

use crate::generate::emit::Plan;

/// The parent column a counter-cached child maintains, matching `#[model]`'s
/// convention: `{snake(child)}_count` — deliberately singular, mirroring
/// `#[votable(aggregate = count)]`'s `{name}_count`.
#[must_use]
pub fn counter_column_for(child_snake: &str) -> String {
    format!("{child_snake}_count")
}

/// `up.sql` adding the counter column to the parent table.
#[must_use]
pub fn up_sql(parent_plural: &str, column: &str) -> String {
    format!("ALTER TABLE {parent_plural} ADD COLUMN {column} BIGINT NOT NULL DEFAULT 0;\n")
}

/// `down.sql` removing it again.
#[must_use]
pub fn down_sql(parent_plural: &str, column: &str) -> String {
    format!("ALTER TABLE {parent_plural} DROP COLUMN {column};\n")
}

/// The migration directory name, e.g.
/// `20260427000000_add_comment_count_to_posts`.
#[must_use]
pub fn migration_dir_name(timestamp: &str, column: &str, parent_plural: &str) -> String {
    format!("{timestamp}_add_{column}_to_{parent_plural}")
}

/// Push the counter-cache migration and the follow-up warning onto `plan`.
///
/// `plan` already carries the child's model/migration/schema actions; this adds
/// the parent-side column. The child model's `#[belongs_to(..., counter_cache)]`
/// attribute is added separately, by rewriting the model file the plan is about
/// to create (see [`add_counter_cache_to_model_source`]).
pub fn push_counter_cache_migration(
    plan: &mut Plan,
    project_root: &Path,
    timestamp: &str,
    child_snake: &str,
    parent_pascal: &str,
    parent_plural: &str,
) {
    let column = counter_column_for(child_snake);
    let dir = project_root
        .join("migrations")
        .join(migration_dir_name(timestamp, &column, parent_plural));
    plan.create(dir.join("up.sql"), up_sql(parent_plural, &column));
    plan.create(dir.join("down.sql"), down_sql(parent_plural, &column));

    plan.warn(format!(
        "--counter-cache: `{parent_plural}.{column}` is added by the generated \
         migration, but the parent's Rust side is a file this scaffold does not \
         own, so it is left to you (two lines, both required before the app \
         compiles):\n  \
         src/schema.rs, inside `diesel::table! {{ {parent_plural} (id) {{ … }} }}`:\n    \
         {column} -> Int8,\n  \
         the `{parent_pascal}` model struct:\n    \
         #[default]\n    pub {column}: i64,\n  \
         Then run `{child_snake}s.recompute_counter_caches()` once if \
         `{parent_plural}` already has rows."
    ));
}

/// Insert `counter_cache` into the child model source's `#[belongs_to(...)]`
/// attribute, adding the attribute when the scaffold did not emit one.
///
/// Returns `source` unchanged when the attribute is already counter-cached, so
/// re-running the scaffold over its own output is idempotent.
#[must_use]
pub fn add_counter_cache_to_model_source(
    source: &str,
    pascal_name: &str,
    parent_pascal: &str,
) -> String {
    let existing = format!("#[belongs_to({parent_pascal}");
    if let Some(start) = source.find(&existing) {
        // An attribute for this parent is already present. Leave it alone if it
        // already opts in; otherwise add the key before the closing paren.
        let Some(end_rel) = source[start..].find(")]") else {
            return source.to_owned();
        };
        let end = start + end_rel;
        if source[start..end].contains("counter_cache") {
            return source.to_owned();
        }
        let mut out = String::with_capacity(source.len() + 16);
        out.push_str(&source[..end]);
        out.push_str(", counter_cache");
        out.push_str(&source[end..]);
        return out;
    }

    // No `belongs_to` on the generated model (the flat scaffold does not emit
    // one): add the whole attribute directly above the struct declaration, which
    // is where `#[model]`'s attributes have to sit.
    let needle = format!("pub struct {pascal_name} ");
    let Some(struct_at) = source.find(&needle) else {
        return source.to_owned();
    };
    // Walk back over the attribute block above the struct to the line after the
    // last blank line, so the new attribute joins the existing attribute stack
    // rather than splitting it.
    let insert_at = source[..struct_at].rfind("\n#[").map_or(struct_at, |i| i + 1);
    let mut out = String::with_capacity(source.len() + 48);
    out.push_str(&source[..insert_at]);
    out.push_str(&format!("#[belongs_to({parent_pascal}, counter_cache)]\n"));
    out.push_str(&source[insert_at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_migration_is_not_null_with_a_zero_default() {
        let up = up_sql("posts", "comment_count");
        assert!(up.contains("ALTER TABLE posts ADD COLUMN comment_count BIGINT"));
        assert!(up.contains("NOT NULL"), "up.sql: {up}");
        assert!(up.contains("DEFAULT 0"), "up.sql: {up}");
        assert_eq!(
            down_sql("posts", "comment_count"),
            "ALTER TABLE posts DROP COLUMN comment_count;\n"
        );
    }

    #[test]
    fn the_column_name_follows_the_singular_convention() {
        assert_eq!(counter_column_for("comment"), "comment_count");
        assert_eq!(counter_column_for("blog_post"), "blog_post_count");
    }

    #[test]
    fn the_migration_dir_names_both_sides() {
        assert_eq!(
            migration_dir_name("20260427000000", "comment_count", "posts"),
            "20260427000000_add_comment_count_to_posts"
        );
    }

    #[test]
    fn the_attribute_is_added_above_the_struct_when_absent() {
        let src = "#[autumn_web::model]\npub struct Comment {\n    pub id: i64,\n}\n";
        let out = add_counter_cache_to_model_source(src, "Comment", "Post");
        assert!(
            out.contains("#[autumn_web::model]\n#[belongs_to(Post, counter_cache)]\npub struct Comment"),
            "{out}"
        );
    }

    #[test]
    fn an_existing_attribute_gains_the_key_and_is_idempotent() {
        let src = "#[autumn_web::model]\n#[belongs_to(Post)]\npub struct Comment {\n}\n";
        let once = add_counter_cache_to_model_source(src, "Comment", "Post");
        assert!(once.contains("#[belongs_to(Post, counter_cache)]"), "{once}");
        let twice = add_counter_cache_to_model_source(&once, "Comment", "Post");
        assert_eq!(once, twice, "re-running the scaffold must not stack keys");
    }
}
