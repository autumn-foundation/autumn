//! `autumn generate migration` — emit a Diesel migration directory only.
//!
//! Inspects the migration name to decide whether to emit empty SQL files
//! (for hand-edited migrations), `ALTER TABLE … ADD COLUMN` (when the name
//! starts with `Add…To…`), or `ALTER TABLE … DROP COLUMN` (when it starts
//! with `Remove…From…`).

use std::path::Path;

use super::dsl::parse_fields;
use super::emit::Plan;
use super::naming::pascal_to_snake;
use super::schema_edit::{
    MigrationShape, add_columns_down_sql, add_columns_up_sql, add_search_down_sql,
    add_search_up_sql, detect_migration_shape, encrypt_columns_down_sql, encrypt_columns_up_sql,
    parse_model_search_config_for_table, remove_columns_down_sql, remove_columns_up_sql,
    singularize,
};
use super::{Flags, GenerateError, ensure_project_root, timestamp_now};

fn collect_rs_files_recursive(dir: &Path, candidates: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files_recursive(&path, candidates);
            } else if path.is_file()
                && path.extension().is_some_and(|ext| ext == "rs")
                && !candidates.contains(&path)
            {
                candidates.push(path);
            }
        }
    }
}

/// Compute the file actions for `autumn generate migration`.
///
/// # Errors
/// Project layout, name, and DSL errors surface here.
pub fn plan_migration(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    timestamp: &str,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    super::model::validate_resource_name(name)?;
    let fields = parse_fields(field_tokens)?;

    // The directory uses snake_case (`add_title_to_posts`) but the shape is
    // detected from the original PascalCase form because the keywords `To`
    // and `From` only have an unambiguous meaning at PascalCase chunk
    // boundaries.
    let dir_name = format!("{timestamp}_{}", snake_or_pascal_to_snake(name));
    let migration_dir = project_root.join("migrations").join(&dir_name);

    let mut plan = Plan::new(project_root);

    let shape = detect_migration_shape(&pascalish(name));
    let (up, down) = match shape {
        MigrationShape::AddColumns { ref table } if !fields.is_empty() => {
            // A `references` field here gets the same target-model warning /
            // UUID-PK error as `generate model`/`generate scaffold` (issue
            // #1026) — otherwise the same DSL token gives inconsistent
            // feedback depending only on which subcommand declared it.
            // `own_id_type` is `None`: this shape only `ALTER TABLE`s an
            // *existing* table, so its actual primary-key type isn't tracked
            // anywhere the generator can see — a self-reference here is left
            // unvalidated rather than guessed at.
            super::model::check_reference_targets(&mut plan, project_root, &fields, table, None)?;
            (
                add_columns_up_sql(table, &fields),
                add_columns_down_sql(table, &fields),
            )
        }
        MigrationShape::RemoveColumns { ref table } if !fields.is_empty() => (
            remove_columns_up_sql(table, &fields),
            remove_columns_down_sql(table, &fields),
        ),
        MigrationShape::EncryptColumns {
            ref table,
            ref columns,
        } => (
            encrypt_columns_up_sql(table, columns),
            encrypt_columns_down_sql(table, columns),
        ),
        MigrationShape::AddSearch { ref table } => {
            let singular = singularize(table);

            // Collect all potential model file candidates in order of preference
            let mut candidates = Vec::new();

            let first_cand = project_root
                .join("src/models")
                .join(format!("{singular}.rs"));
            if first_cand.exists() {
                candidates.push(first_cand);
            }

            let second_cand = project_root.join("src/models.rs");
            if second_cand.exists() {
                candidates.push(second_cand);
            }

            let models_dir = project_root.join("src/models");
            let mut other_candidates = Vec::new();
            collect_rs_files_recursive(&models_dir, &mut other_candidates);
            other_candidates.sort();

            for path in other_candidates {
                if !candidates.contains(&path) {
                    candidates.push(path);
                }
            }

            let mut found_config = None;
            let mut tried_files = Vec::new();

            for path in candidates {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Some((language, fts_fields)) =
                        parse_model_search_config_for_table(&content, table)
                    {
                        found_config = Some((path, language, fts_fields));
                        break;
                    }
                    tried_files.push(path);
                }
            }

            let Some((_path, language, fts_fields)) = found_config else {
                if tried_files.is_empty() {
                    return Err(GenerateError::Config(format!(
                        "Missing model files for table '{table}'. Expected src/models/{singular}.rs or src/models.rs."
                    )));
                }
                let files_str = tried_files
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(GenerateError::Config(format!(
                    "No #[searchable] fields configured for table '{table}' in any of the checked files: [{files_str}]"
                )));
            };

            (
                add_search_up_sql(table, &language, &fts_fields),
                add_search_down_sql(table),
            )
        }
        _ => (String::new(), String::new()),
    };

    plan.create(migration_dir.join("up.sql"), up);
    plan.create(migration_dir.join("down.sql"), down);
    Ok(plan)
}

/// Convert a name like `AddTitleToPosts` → `add_title_to_posts`, while
/// leaving an already-snake-case name untouched.
fn snake_or_pascal_to_snake(name: &str) -> String {
    if name.contains('_') || !name.chars().any(char::is_uppercase) {
        name.to_ascii_lowercase()
    } else {
        pascal_to_snake(name)
    }
}

/// Re-shape a possibly-snake-case name back to `PascalCase` so
/// [`detect_migration_shape`] sees the chunk boundaries it expects.
fn pascalish(name: &str) -> String {
    if name.contains('_') {
        super::naming::snake_to_pascal(name)
    } else {
        name.to_owned()
    }
}

/// CLI entry point.
pub fn run(name: &str, field_tokens: &[String], flags: Flags) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: cannot determine current directory: {e}");
            std::process::exit(1);
        }
    };
    let timestamp = timestamp_now();
    match plan_migration(&cwd, name, field_tokens, &timestamp).and_then(|p| p.execute(flags)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        tmp
    }

    #[test]
    fn empty_migration_when_no_keyword_match() {
        let tmp = project();
        let plan = plan_migration(tmp.path(), "BackfillSomething", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let dir = tmp
            .path()
            .join("migrations/20260427000000_backfill_something");
        let up = fs::read_to_string(dir.join("up.sql")).unwrap();
        let down = fs::read_to_string(dir.join("down.sql")).unwrap();
        assert!(up.is_empty());
        assert!(down.is_empty());
    }

    #[test]
    fn add_columns_migration_emits_alter() {
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "AddTitleToPosts",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_add_title_to_posts/up.sql"),
        )
        .unwrap();
        let down = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_add_title_to_posts/down.sql"),
        )
        .unwrap();
        assert!(up.contains("ALTER TABLE posts ADD COLUMN title TEXT NOT NULL"));
        assert!(down.contains("ALTER TABLE posts DROP COLUMN title"));
    }

    #[test]
    fn remove_columns_migration_emits_drop() {
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "RemoveBodyFromPosts",
            &["body:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_remove_body_from_posts/up.sql"),
        )
        .unwrap();
        assert!(up.contains("ALTER TABLE posts DROP COLUMN body"));
    }

    #[test]
    fn add_pattern_with_no_fields_is_empty() {
        let tmp = project();
        let plan = plan_migration(tmp.path(), "AddTitleToPosts", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_add_title_to_posts/up.sql"),
        )
        .unwrap();
        assert!(up.is_empty());
    }

    #[test]
    fn snake_case_name_is_accepted() {
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "add_title_to_posts",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_add_title_to_posts/up.sql"),
        )
        .unwrap();
        assert!(up.contains("ALTER TABLE posts ADD COLUMN title TEXT NOT NULL"));
    }

    #[test]
    fn add_search_migration_emits_fts_columns_and_indices() {
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        let model_src = r#"
#[autumn_web::model(table = "posts")]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B")]
    pub body: String,
}
"#;
        fs::write(models_dir.join("post.rs"), model_src).unwrap();

        let plan = plan_migration(tmp.path(), "AddSearchToPosts", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_add_search_to_posts/up.sql"),
        )
        .unwrap();
        let down = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_add_search_to_posts/down.sql"),
        )
        .unwrap();

        assert!(up.contains("ALTER TABLE posts ADD COLUMN search_vector tsvector GENERATED ALWAYS AS (setweight(to_tsvector('english'::regconfig, coalesce(\"title\"::text, '')), 'A') || setweight(to_tsvector('english'::regconfig, coalesce(\"body\"::text, '')), 'B')) STORED;"));
        assert!(
            up.contains("CREATE INDEX idx_posts_search_vector ON posts USING gin(search_vector);")
        );
        assert!(down.contains("DROP INDEX IF EXISTS idx_posts_search_vector;"));
        assert!(down.contains("ALTER TABLE posts DROP COLUMN IF EXISTS search_vector;"));
    }

    // ── references field: parity with `generate model` (issue #1026) ───────

    #[test]
    fn add_columns_with_references_field_emits_fk_and_index() {
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "AddPostToComments",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_add_post_to_comments/up.sql"),
        )
        .unwrap();
        assert!(up.contains("post_id BIGINT NOT NULL REFERENCES posts(id)"));
        assert!(up.contains("CREATE INDEX idx_comments_post_id ON comments (post_id);"));
    }

    #[test]
    fn add_columns_with_references_field_warns_when_target_model_missing() {
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "AddPostToComments",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
        assert!(plan.warnings[0].contains("posts"));
    }

    #[test]
    fn add_columns_with_references_field_errors_on_uuid_target() {
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: uuid::Uuid,\n}\n",
        )
        .unwrap();

        let err = plan_migration(
            tmp.path(),
            "AddPostToComments",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn add_columns_self_reference_to_table_being_altered_has_no_warning() {
        // `AddCategoryToCategories category:references` targets the very
        // table it's altering — a filesystem lookup for a "Category" model
        // is irrelevant here (the table obviously already exists, that's
        // the point of ALTER TABLE), so no "model not found" warning should
        // fire for the self-reference.
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "AddCategoryToCategories",
            &["category:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(plan.warnings.is_empty(), "warnings: {:?}", plan.warnings);
    }
}
