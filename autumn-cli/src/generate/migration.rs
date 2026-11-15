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
    MigrationShape, add_columns_down_sql_for, add_columns_up_sql_for, add_search_down_sql_for,
    add_search_up_sql_for, detect_migration_shape, encrypt_columns_down_sql,
    encrypt_columns_up_sql, parse_model_search_config_for_table, remove_columns_down_sql_for,
    remove_columns_up_sql_for, singularize,
};
use super::{GenerateError, detect_backend, ensure_project_root};

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
#[allow(dead_code)]
pub fn plan_migration(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    timestamp: &str,
) -> Result<Plan, GenerateError> {
    plan_migration_with_options(project_root, name, field_tokens, timestamp, &[])
}

/// [`plan_migration`], plus `--unique FIELD` flags (issue #1032) — mirrors
/// the DSL's inline `:unique` modifier, which already works via
/// [`parse_fields`] alone (no options struct needed, unlike `generate
/// model`/`scaffold`'s `ModelOptions`).
///
/// # Errors
/// Project layout, name, DSL, and unknown-field errors surface here.
#[allow(
    clippy::too_many_lines,
    reason = "a linear match over the migration shapes (add/remove/encrypt/search \
              columns), each arm emitting its own up/down SQL; splitting an arm out \
              would not make any single shape clearer"
)]
pub fn plan_migration_with_options(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    timestamp: &str,
    uniques: &[String],
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    super::model::validate_resource_name(name)?;
    let mut fields = parse_fields(field_tokens)?;
    super::model::apply_unique_flags(&mut fields, uniques)?;
    // This command writes no model, so `{encrypted}` would silently add a plaintext column
    // (#1340).
    if let Some(field) = fields.iter().find(|f| f.is_encrypted()) {
        return Err(GenerateError::InvalidField {
            token: field.name.clone(),
            reason: format!(
                "the `encrypted` modifier is not supported by `generate migration`: this \
                 command emits SQL only, so the `#[encrypted]` attribute would never reach a \
                 model and the column would silently be plaintext. Declare the column with \
                 `autumn generate model`/`autumn generate scaffold` \
                 (`{}:String{{encrypted}}`), or convert an existing plaintext column with \
                 `autumn generate migration Encrypt{}On<Table>`, which emits the documented \
                 offline backfill.",
                field.name,
                super::naming::pascal(&field.name),
            ),
        });
    }
    // Likewise `{translatable}` (#1384): no model would read the JSON column as `Translated`.
    // This also refuses `--unique`, which `apply_unique_flags` folds in after `parse_field`'s
    // own check and which would index the whole JSON container.
    if let Some(field) = fields.iter().find(|f| f.is_translatable()) {
        return Err(GenerateError::InvalidField {
            token: field.name.clone(),
            reason: format!(
                "the `translatable` modifier is not supported by `generate migration`: this \
                 command emits SQL only, so the `#[translatable]` attribute and the \
                 `autumn_web::i18n::Translated` field type would never reach a model, and the \
                 app would read the per-locale JSON container as a plain string. Declare the \
                 column with `autumn generate model` (`{}:String{{translatable}}`), which emits \
                 the model, the schema entry, the migration and the `i18n` feature together.",
                field.name
            ),
        });
    }
    let backend = detect_backend(project_root);

    // The shape is detected from the PascalCase name: `To` and `From` are only unambiguous at
    // PascalCase chunk boundaries.
    let dir_name = format!("{timestamp}_{}", snake_or_pascal_to_snake(name));
    let migration_dir = project_root.join("migrations").join(&dir_name);

    let mut plan = Plan::new(project_root);

    let shape = detect_migration_shape(&pascalish(name));
    let (up, down) = match shape {
        MigrationShape::AddColumns { ref table } if !fields.is_empty() => {
            // Same `references` checks as `generate model` (#1026). `own_id_type` is `None`: the
            // altered table's PK type is unknown here, so a self-reference is not guessed at.
            super::model::check_reference_targets(&mut plan, project_root, &fields, table, None)?;
            // Validate `lock_version` as `generate model` does (#1318). ADD shape only: dropping a
            // legacy `lock_version` of another type is the escape hatch the errors point at.
            super::model::validate_lock_version_field(&fields, &[])?;
            // Same guard as `generate model`: a kind with no diesel SQLite conversion would not
            // compile. Every kind converts as of #1924.
            if backend == autumn_web::config::DatabaseBackend::Sqlite {
                super::reject_sqlite_unsupported_field_kinds(&fields)?;
            }
            // Lets a `unique` index name avoid colliding with an index from an earlier migration
            // (#1032). Empty when the file does not exist yet.
            let existing_schema =
                std::fs::read_to_string(project_root.join("src/schema.rs")).unwrap_or_default();
            (
                add_columns_up_sql_for(backend, table, &fields, &existing_schema)?,
                add_columns_down_sql_for(backend, table, &fields, &existing_schema),
            )
        }
        MigrationShape::RemoveColumns { ref table } if !fields.is_empty() => {
            // The rollback restores a `references` field's FK and index (#1026), so it needs the
            // same UUID-PK guard as `AddColumns`.
            super::model::check_reference_targets(&mut plan, project_root, &fields, table, None)?;
            // `down.sql` re-adds the columns, so the SQLite kind guard applies here too (#1924).
            if backend == autumn_web::config::DatabaseBackend::Sqlite {
                super::reject_sqlite_unsupported_field_kinds(&fields)?;
            }
            let existing_schema =
                std::fs::read_to_string(project_root.join("src/schema.rs")).unwrap_or_default();
            // SQLite refuses `DROP COLUMN` while an index names the column.
            // Recover the table's prior indexes so the up path drops them first
            // (#1906). Postgres cascades the drop and needs none of this.
            let prior_indexes = if backend == autumn_web::config::DatabaseBackend::Sqlite {
                let columns: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                super::prior_index::scan_prior_indexes(
                    &project_root.join("migrations"),
                    table,
                    &columns,
                )
                .map_err(GenerateError::Config)?
            } else {
                Vec::new()
            };
            (
                remove_columns_up_sql_for(
                    backend,
                    table,
                    &fields,
                    &existing_schema,
                    &prior_indexes,
                ),
                remove_columns_down_sql_for(
                    backend,
                    table,
                    &fields,
                    &existing_schema,
                    &prior_indexes,
                )?,
            )
        }
        MigrationShape::EncryptColumns {
            ref table,
            ref columns,
        } => (
            encrypt_columns_up_sql(table, columns),
            encrypt_columns_down_sql(table, columns),
        ),
        MigrationShape::AddSearch { ref table } => {
            // Postgres gets a `tsvector` column + GIN index, SQLite an FTS5 table + triggers
            // (#1910; see `add_search_up_sql_for`).
            let singular = singularize(table);

            // Model file candidates, in order of preference.
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
                add_search_up_sql_for(backend, table, &language, &fts_fields)?,
                add_search_down_sql_for(backend, table),
            )
        }
        _ => (String::new(), String::new()),
    };

    plan.create(migration_dir.join("up.sql"), up);
    plan.create(migration_dir.join("down.sql"), down);
    Ok(plan)
}

/// Fallback destroy-only plan for `autumn destroy migration <name>` when
/// [`plan_migration_with_options`] can't be recomputed — e.g. an
/// `AddSearchTo<Table>` migration whose model file (or its `#[searchable]`
/// config) is already gone by the time destroy runs, a common cleanup order
/// like deleting/destroying the model first (issue #1048 PR review).
/// `plan_migration_with_options` needs that config to render the right SQL,
/// which is meaningless (and an error) once it's gone.
///
/// Locates the migration directory by suffix only — the same
/// `{timestamp}_{suffix}` naming [`plan_migration_with_options`] uses, which
/// is all [`Plan::revert`] actually needs to find it (destroy always
/// recomputes a fresh timestamp anyway, so exact SQL content was never
/// load-bearing for *locating* the directory). Its expected content is
/// unknowable without re-deriving the search config, so `up.sql`/`down.sql`
/// are recorded with an empty placeholder — real content (never empty)
/// always counts as diverged, so they're only removed with `--force`,
/// exactly like [`super::admin::plan_admin_destroy_fallback`].
///
/// # Errors
/// Returns [`GenerateError`] when `project_root` isn't a valid project, or
/// `name` fails validation.
pub fn plan_migration_destroy_fallback(
    project_root: &Path,
    name: &str,
    timestamp: &str,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    super::model::validate_resource_name(name)?;

    let dir_name = format!("{timestamp}_{}", snake_or_pascal_to_snake(name));
    let migration_dir = project_root.join("migrations").join(&dir_name);
    let mut plan = Plan::new(project_root);
    plan.create(migration_dir.join("up.sql"), String::new());
    plan.create(migration_dir.join("down.sql"), String::new());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #1384 (Codex round 4): `generate migration` emits SQL only, so a
    /// `{translatable}` column would arrive without the `#[translatable]`
    /// attribute or the `Translated` field type — the app would read the JSON
    /// container as a plain string. Same silent failure `{encrypted}` refuses.
    #[test]
    fn migration_rejects_a_translatable_field() {
        let tmp = project();
        let err = plan_migration(
            tmp.path(),
            "AddTitleToPosts",
            &["title:String{translatable}".into()],
            "20260427000000",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("translatable"), "{err}");
        assert!(err.contains("generate model"), "{err}");
    }

    /// The refusal also closes the `--unique` hole: the flag is applied after
    /// `parse_field`'s own `:unique` cross-check has run, so without it
    /// `--unique` would emit a UNIQUE index over the whole JSON container.
    #[test]
    fn migration_rejects_a_translatable_field_flagged_unique() {
        let tmp = project();
        let err = plan_migration_with_options(
            tmp.path(),
            "AddTitleToPosts",
            &["title:String{translatable}".into()],
            "20260427000000",
            &["title".to_owned()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("translatable"), "{err}");
    }
    use crate::generate::Flags;
    use crate::generate::emit::Action;
    use std::fs;
    use std::path::PathBuf;
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
    fn generate_then_destroy_migration_round_trips_to_original_project_state() {
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                let tmp = project();
                let plan = plan_migration(
                    tmp.path(),
                    "AddTitleToPosts",
                    &["title:String".into()],
                    "20260427000000",
                )
                .unwrap();
                plan.execute(Flags::default()).unwrap();
                let dir = tmp
                    .path()
                    .join("migrations/20260427000000_add_title_to_posts");
                assert!(dir.exists());

                // Destroy recomputes the plan with a FRESH timestamp; the
                // real on-disk directory must still be found by suffix.
                let destroy_plan = plan_migration(
                    tmp.path(),
                    "AddTitleToPosts",
                    &["title:String".into()],
                    "99999999999999",
                )
                .unwrap();
                destroy_plan.revert(Flags::default()).unwrap();

                assert!(!dir.exists());
                assert!(
                    fs::read_dir(tmp.path().join("migrations"))
                        .map_or(true, |mut d| d.next().is_none())
                );
            },
        );
    }

    #[test]
    fn destroy_migration_refuses_directory_with_unplanned_file_unless_forced() {
        // An extra file beside up.sql/down.sql (a README, a fixture) is divergence, not
        // something `remove_dir_all` may sweep up (#1048).
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                let tmp = project();
                let plan = plan_migration(
                    tmp.path(),
                    "AddTitleToPosts",
                    &["title:String".into()],
                    "20260427000000",
                )
                .unwrap();
                plan.execute(Flags::default()).unwrap();
                let dir = tmp
                    .path()
                    .join("migrations/20260427000000_add_title_to_posts");
                assert!(dir.exists());
                fs::write(dir.join("README.md"), "hand-authored notes\n").unwrap();

                let destroy_plan = plan_migration(
                    tmp.path(),
                    "AddTitleToPosts",
                    &["title:String".into()],
                    "99999999999999",
                )
                .unwrap();
                let err = destroy_plan
                    .revert(Flags {
                        dry_run: false,
                        force: false,
                    })
                    .unwrap_err();
                assert!(matches!(err, GenerateError::Diverged(_)));
                assert!(
                    dir.join("README.md").exists(),
                    "hand-authored file must survive without --force"
                );

                let destroy_plan = plan_migration(
                    tmp.path(),
                    "AddTitleToPosts",
                    &["title:String".into()],
                    "99999999999999",
                )
                .unwrap();
                destroy_plan
                    .revert(Flags {
                        dry_run: false,
                        force: true,
                    })
                    .unwrap();
                assert!(!dir.exists(), "--force must still remove the directory");
            },
        );
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
    fn remove_columns_migration_with_references_field_restores_fk_on_rollback() {
        // `RemovePostFromComments post:references` — down.sql must restore
        // the FK constraint and index, not just a bare column (issue #1026).
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "RemovePostFromComments",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let down = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_remove_post_from_comments/down.sql"),
        )
        .unwrap();
        assert!(
            down.contains(
                "ALTER TABLE comments ADD COLUMN post_id BIGINT NOT NULL REFERENCES posts(id);"
            ),
            "down.sql: {down}"
        );
        assert!(
            down.contains("CREATE INDEX idx_comments_post_id ON comments (post_id);"),
            "down.sql: {down}"
        );
    }

    #[test]
    fn remove_columns_with_references_field_errors_on_uuid_target() {
        // Otherwise a UUID-keyed target yields a down.sql that fails on rollback.
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
            "RemovePostFromComments",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn remove_columns_with_references_field_warns_when_target_model_missing() {
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "RemovePostFromComments",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
        assert!(plan.warnings[0].contains("posts"));
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

    // ── SQLite backend awareness (issue #1614) ──────────────────────────────

    /// Null the DB-URL environment variables so backend detection reads the
    /// temp project's `autumn.toml` deterministically.
    fn with_no_db_env<R>(f: impl FnOnce() -> R) -> R {
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            f,
        )
    }

    fn sqlite_project() -> TempDir {
        let tmp = project();
        fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nprimary_url = \"sqlite://app.db\"\n",
        )
        .unwrap();
        tmp
    }

    /// `AddSearchTo…` on a `SQLite` app now emits an FTS5 external-content virtual
    /// table + maintenance triggers (issue #1910) instead of being rejected, and
    /// no Postgres-only `tsvector`/GIN DDL leaks into the `SQLite` migration.
    #[test]
    fn add_search_migration_on_sqlite_emits_fts5() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            // The AddSearch shape reads the model's #[searchable] config from a
            // model file to know which columns to index.
            fs::create_dir_all(tmp.path().join("src/models")).unwrap();
            fs::write(
                tmp.path().join("src/models/post.rs"),
                "#[autumn_web::model(table = \"posts\")]\n\
                 #[searchable(language = \"english\")]\n\
                 pub struct Post {\n\
                 \x20   #[id]\n\
                 \x20   pub id: i64,\n\
                 \x20   #[searchable(weight = \"A\")]\n\
                 \x20   pub title: String,\n\
                 \x20   #[searchable(weight = \"B\")]\n\
                 \x20   pub body: String,\n\
                 }\n",
            )
            .unwrap();

            let plan =
                plan_migration(tmp.path(), "AddSearchToPosts", &[], "20260427000000").unwrap();
            plan.execute(Flags::default()).unwrap();

            let dir = tmp
                .path()
                .join("migrations/20260427000000_add_search_to_posts");
            let up = fs::read_to_string(dir.join("up.sql")).unwrap();
            let down = fs::read_to_string(dir.join("down.sql")).unwrap();

            assert!(
                up.contains(
                    "CREATE VIRTUAL TABLE \"posts__fts\" USING fts5(\"title\", \"body\", \
                     content='posts', content_rowid='id', tokenize='unicode61');"
                ),
                "up.sql must create the FTS5 vtable: {up}"
            );
            assert!(
                up.contains("INSERT INTO \"posts__fts\"(\"posts__fts\") VALUES('rebuild');"),
                "up.sql must backfill via 'rebuild': {up}"
            );
            for trig in ["posts__fts_ai", "posts__fts_ad", "posts__fts_au"] {
                assert!(up.contains(trig), "up.sql must create trigger {trig}: {up}");
                assert!(
                    down.contains(&format!("DROP TRIGGER IF EXISTS \"{trig}\";")),
                    "down.sql must drop trigger {trig}: {down}"
                );
            }
            assert!(
                down.contains("DROP TABLE IF EXISTS \"posts__fts\";"),
                "down.sql must drop the FTS table: {down}"
            );
            for leak in ["tsvector", "to_tsvector", "USING gin", "search_vector"] {
                assert!(!up.contains(leak), "SQLite up.sql leaked `{leak}`: {up}");
            }
        });
    }

    /// `Add…To…` on a `SQLite` app emits `SQLite`-valid column types
    /// (`i64` -> `INTEGER`, not Postgres `BIGINT`). Uses a *nullable* column
    /// because `SQLite` rejects `ADD COLUMN … NOT NULL` without a default (see
    /// `add_columns_migration_on_sqlite_rejects_not_null_without_default`).
    #[test]
    fn add_columns_migration_on_sqlite_emits_sqlite_types() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let plan = plan_migration(
                tmp.path(),
                "AddViewsToPosts",
                &["views:Option<i64>".into()],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();
            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_add_views_to_posts/up.sql"),
            )
            .unwrap();
            assert!(
                up.contains("ALTER TABLE posts ADD COLUMN views INTEGER NULL"),
                "up.sql: {up}"
            );
            assert!(!up.contains("BIGINT"), "SQLite up.sql leaked BIGINT: {up}");
        });
    }

    /// `Add…To…` on a `SQLite` app with a `NOT NULL` column and no default is
    /// rejected at generate time (issue #1614 AC #4): `SQLite` rejects
    /// `ALTER TABLE … ADD COLUMN … NOT NULL` without a `DEFAULT` once the table
    /// has rows, and this command has no way to attach a column default, so the
    /// generated migration would fail to apply. Mirrors the `--search`
    /// reject-at-generate contract.
    #[test]
    fn add_columns_migration_on_sqlite_rejects_not_null_without_default() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let err = plan_migration(
                tmp.path(),
                "AddViewsToPosts",
                &["views:i64".into()],
                "20260427000000",
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                matches!(err, GenerateError::Config(_)),
                "expected Config error, got: {err:?}"
            );
            assert!(
                msg.contains("NOT NULL") && msg.contains("views") && msg.contains("posts"),
                "message must name the column/table and the constraint: {msg}"
            );
            assert!(
                msg.contains("nullable") || msg.contains("default"),
                "message must be actionable (nullable / default): {msg}"
            );
        });
    }

    /// A *nullable* `NOT NULL`-free column is added without a default on
    /// `SQLite`, while the reject above guards the `NOT NULL` case — together
    /// they cover both branches of the `SQLite` `ADD COLUMN` gate.
    #[test]
    fn add_columns_migration_on_sqlite_allows_nullable_without_default() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let plan = plan_migration(
                tmp.path(),
                "AddNoteToPosts",
                &["note:Option<String>".into()],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();
            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_add_note_to_posts/up.sql"),
            )
            .unwrap();
            assert!(
                up.contains("ALTER TABLE posts ADD COLUMN note TEXT NULL"),
                "up.sql: {up}"
            );
        });
    }

    /// A `SQLite` `Add…To…` migration that adds a nullable `references` field
    /// creates an index in `up.sql`, and its `down.sql` must `DROP INDEX` before
    /// `DROP COLUMN` — `SQLite` refuses to drop a column still used by an index
    /// (issue #1614).
    #[test]
    fn add_columns_migration_on_sqlite_drops_index_before_column_on_rollback() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let plan = plan_migration(
                tmp.path(),
                "AddAuthorToPosts",
                &["author:references?".into()],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();
            let dir = tmp
                .path()
                .join("migrations/20260427000000_add_author_to_posts");
            let up = fs::read_to_string(dir.join("up.sql")).unwrap();
            assert!(
                up.contains("CREATE INDEX idx_posts_author_id ON posts (author_id);"),
                "up.sql: {up}"
            );
            let down = fs::read_to_string(dir.join("down.sql")).unwrap();
            let drop_idx = down
                .find("DROP INDEX idx_posts_author_id;")
                .expect("drop index");
            let drop_col = down
                .find("ALTER TABLE posts DROP COLUMN author_id;")
                .expect("drop column");
            assert!(
                drop_idx < drop_col,
                "down.sql must DROP INDEX before DROP COLUMN: {down}"
            );
        });
    }

    /// Issue #1906: a `Remove…From…` on `SQLite` must drop a PRE-EXISTING index
    /// that names the removed column, not just the conventional
    /// `idx_<table>_<col>` the generator itself would have created. `SQLite`
    /// refuses `DROP COLUMN` while any index still references the column, so a
    /// composite index from an earlier migration would otherwise break the
    /// migration at apply time.
    #[test]
    fn remove_columns_migration_on_sqlite_drops_a_pre_existing_composite_index() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let earlier = tmp.path().join("migrations/20260101000000_create_posts");
            fs::create_dir_all(&earlier).unwrap();
            fs::write(
                earlier.join("up.sql"),
                "CREATE TABLE posts (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 author_id INTEGER, title TEXT);\n\
                 CREATE INDEX idx_posts_author_title ON posts (author_id, title);\n",
            )
            .unwrap();

            let plan = plan_migration(
                tmp.path(),
                "RemoveTitleFromPosts",
                &["title:Option<String>".into()],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();
            let dir = tmp
                .path()
                .join("migrations/20260427000000_remove_title_from_posts");

            let up = fs::read_to_string(dir.join("up.sql")).unwrap();
            let drop_idx = up
                .find("DROP INDEX IF EXISTS idx_posts_author_title;")
                .unwrap_or_else(|| panic!("composite index not dropped: {up}"));
            let drop_col = up
                .find("ALTER TABLE posts DROP COLUMN title;")
                .unwrap_or_else(|| panic!("column not dropped: {up}"));
            assert!(drop_idx < drop_col, "index drop must come first: {up}");

            // The rollback restores it, after the column is back.
            let down = fs::read_to_string(dir.join("down.sql")).unwrap();
            let add_col = down
                .find("ALTER TABLE posts ADD COLUMN title")
                .unwrap_or_else(|| panic!("column not re-added: {down}"));
            let recreate = down
                .find("CREATE INDEX idx_posts_author_title ON posts (author_id, title);")
                .unwrap_or_else(|| panic!("index not re-created: {down}"));
            assert!(add_col < recreate, "column must precede its index: {down}");
        });
    }

    /// Issue #2579: a column behind an inline `UNIQUE` cannot be dropped on
    /// `SQLite`. The generator says so now, not at apply time.
    #[test]
    fn remove_columns_migration_on_sqlite_rejects_an_inline_unique_column() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let earlier = tmp.path().join("migrations/20260101000000_create_posts");
            fs::create_dir_all(&earlier).unwrap();
            fs::write(
                earlier.join("up.sql"),
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, slug TEXT UNIQUE);\n",
            )
            .unwrap();

            let err = plan_migration(
                tmp.path(),
                "RemoveSlugFromPosts",
                &["slug:Option<String>".into()],
                "20260427000000",
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("posts.slug"), "got: {err}");
        });
    }

    /// Issue #2579: an earlier migration that does not replay stops the
    /// generator with the file name, instead of a guess.
    #[test]
    fn remove_columns_migration_on_sqlite_names_a_migration_that_does_not_replay() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let earlier = tmp.path().join("migrations/20260101000000_broken");
            fs::create_dir_all(&earlier).unwrap();
            fs::write(earlier.join("up.sql"), "CREATE TABEL posts (id INTEGER);\n").unwrap();

            let err = plan_migration(
                tmp.path(),
                "RemoveTitleFromPosts",
                &["title:Option<String>".into()],
                "20260427000000",
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("20260101000000_broken"), "got: {err}");
        });
    }

    /// The same project shape on Postgres emits no explicit `DROP INDEX` — it
    /// cascades the drop with the column, so the output is unchanged by #1906.
    #[test]
    fn remove_columns_migration_on_postgres_ignores_pre_existing_indexes() {
        with_no_db_env(|| {
            let tmp = project();
            let earlier = tmp.path().join("migrations/20260101000000_create_posts");
            fs::create_dir_all(&earlier).unwrap();
            fs::write(
                earlier.join("up.sql"),
                "CREATE INDEX idx_posts_author_title ON posts (author_id, title);\n",
            )
            .unwrap();

            let plan = plan_migration(
                tmp.path(),
                "RemoveTitleFromPosts",
                &["title:Option<String>".into()],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();
            let dir = tmp
                .path()
                .join("migrations/20260427000000_remove_title_from_posts");
            let up = fs::read_to_string(dir.join("up.sql")).unwrap();
            let down = fs::read_to_string(dir.join("down.sql")).unwrap();
            assert!(!up.contains("DROP INDEX"), "Postgres up.sql: {up}");
            assert!(!down.contains("idx_posts_author_title"), "down.sql: {down}");
        });
    }

    /// Regression guard: the same `Add…To…` on a Postgres app emits no explicit
    /// `DROP INDEX` in `down.sql` — Postgres cascades the index drop with the
    /// column, so the rollback stays byte-for-byte the historical output.
    #[test]
    fn add_columns_migration_on_postgres_has_no_explicit_drop_index_on_rollback() {
        with_no_db_env(|| {
            let tmp = project();
            let plan = plan_migration(
                tmp.path(),
                "AddAuthorToPosts",
                &["author:references?".into()],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();
            let down = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_add_author_to_posts/down.sql"),
            )
            .unwrap();
            assert!(!down.contains("DROP INDEX"), "Postgres down.sql: {down}");
            assert!(
                down.contains("ALTER TABLE posts DROP COLUMN author_id;"),
                "down.sql: {down}"
            );
        });
    }

    /// A `SQLite` `Add…To…` / `Remove…From…` migration now accepts every field
    /// kind: #1924 gave `Uuid`, `Decimal` and `Enum` working `SQLite`
    /// conversions, so the generate-time rejection no longer fires. All three
    /// store `TEXT`.
    ///
    /// Nullable columns here, deliberately: `SQLite`'s own `ALTER TABLE ADD
    /// COLUMN` rule still refuses a `NOT NULL` column with no default, which is
    /// a separate gate (#1918) this test must not trip over.
    #[test]
    fn column_migrations_on_sqlite_accept_uuid_decimal_and_enum_after_1924() {
        with_no_db_env(|| {
            for token in [
                "token:Option<Uuid>",
                "price:Option<decimal{10,2}>",
                "status:Option<enum{draft,published}>",
            ] {
                for name in ["AddTokenToPosts", "RemoveTokenFromPosts"] {
                    let tmp = sqlite_project();
                    let plan = plan_migration(tmp.path(), name, &[token.into()], "20260427000000")
                        .unwrap_or_else(|e| {
                            panic!("{name} with `{token}` must plan on SQLite (#1924): {e}")
                        });
                    let up = sql_action(&plan, "up.sql");
                    let down = sql_action(&plan, "down.sql");
                    for sql in [&up, &down] {
                        for leak in ["UUID", "NUMERIC"] {
                            assert!(
                                !sql.contains(leak),
                                "{name}/{token}: SQLite SQL leaked `{leak}`: {sql}"
                            );
                        }
                    }
                }
            }
        });
    }

    /// Regression guard: with no `autumn.toml`, the backend defaults to Postgres
    /// and `Add…To…` emits the historical Postgres column type.
    #[test]
    fn add_columns_migration_defaults_to_postgres_types() {
        with_no_db_env(|| {
            let tmp = project();
            let plan = plan_migration(
                tmp.path(),
                "AddViewsToPosts",
                &["views:i64".into()],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();
            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_add_views_to_posts/up.sql"),
            )
            .unwrap();
            assert!(
                up.contains("ALTER TABLE posts ADD COLUMN views BIGINT NOT NULL"),
                "up.sql: {up}"
            );
        });
    }

    /// `Remove…From…` on a `SQLite` app is rejected at generate time when the
    /// rollback would re-add a `NOT NULL` column with no default (issue #1614
    /// AC #4). The `down.sql` of a "remove columns" migration regenerates
    /// `ALTER TABLE … ADD COLUMN …` to restore the dropped column, and `SQLite`
    /// rejects that DDL for a `NOT NULL` column without a `DEFAULT` — the same
    /// limit the forward path guards
    /// (`add_columns_migration_on_sqlite_rejects_not_null_without_default`), so
    /// the reverse path must be consistent.
    #[test]
    fn remove_columns_migration_on_sqlite_rejects_not_null_re_add_on_rollback() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let err = plan_migration(
                tmp.path(),
                "RemoveViewsFromPosts",
                &["views:i64".into()],
                "20260427000000",
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                matches!(err, GenerateError::Config(_)),
                "expected Config error, got: {err:?}"
            );
            assert!(
                msg.contains("NOT NULL") && msg.contains("views") && msg.contains("posts"),
                "message must name the column/table and the constraint: {msg}"
            );
            assert!(
                msg.contains("nullable") || msg.contains("default"),
                "message must be actionable (nullable / default): {msg}"
            );
        });
    }

    /// A *nullable* re-added column is restored without a default on `SQLite`,
    /// while the reject above guards the `NOT NULL` case — together they cover
    /// both branches of the `SQLite` rollback `ADD COLUMN` gate.
    #[test]
    fn remove_columns_migration_on_sqlite_allows_nullable_re_add_on_rollback() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let plan = plan_migration(
                tmp.path(),
                "RemoveNoteFromPosts",
                &["note:Option<String>".into()],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();
            let down = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_remove_note_from_posts/down.sql"),
            )
            .unwrap();
            assert!(
                down.contains("ALTER TABLE posts ADD COLUMN note TEXT NULL"),
                "down.sql: {down}"
            );
        });
    }

    /// Regression guard: with no `autumn.toml`, the backend defaults to Postgres
    /// and `Remove…From…`'s rollback re-adds the `NOT NULL` column with the
    /// historical Postgres column type — byte-for-byte unchanged by the
    /// `SQLite` reject above.
    #[test]
    fn remove_columns_migration_defaults_to_postgres_types_on_rollback() {
        with_no_db_env(|| {
            let tmp = project();
            let plan = plan_migration(
                tmp.path(),
                "RemoveViewsFromPosts",
                &["views:i64".into()],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();
            let down = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_remove_views_from_posts/down.sql"),
            )
            .unwrap();
            assert!(
                down.contains("ALTER TABLE posts ADD COLUMN views BIGINT NOT NULL"),
                "down.sql: {down}"
            );
        });
    }

    // ── `plan_migration_destroy_fallback` (issue #1048) ─────────────────────

    /// A project holding a `#[searchable]` `Post` model and the
    /// `AddSearchToPosts` migration generated from it, returned with the
    /// migration's directory. Shared by the three `destroy` fallback tests.
    fn searchable_post_with_migration() -> (TempDir, PathBuf) {
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            r#"
#[autumn_web::model(table = "posts")]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
}
"#,
        )
        .unwrap();

        let plan = plan_migration(tmp.path(), "AddSearchToPosts", &[], "20260427000000").unwrap();
        plan.execute(Flags::default()).unwrap();
        let dir = tmp
            .path()
            .join("migrations/20260427000000_add_search_to_posts");
        assert!(dir.exists());
        (tmp, dir)
    }

    #[test]
    fn destroy_add_search_migration_after_model_already_destroyed_still_removes_it() {
        // Destroying the model first must not strand an `AddSearchTo<Table>` migration whose
        // `#[searchable]` config is now gone.
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                let (tmp, dir) = searchable_post_with_migration();
                // Simulate `autumn destroy model Post` having already run.
                fs::remove_file(tmp.path().join("src/models/post.rs")).unwrap();
                assert!(
                    plan_migration(tmp.path(), "AddSearchToPosts", &[], "99999999999999").is_err()
                );

                let fallback_plan = plan_migration_destroy_fallback(
                    tmp.path(),
                    "AddSearchToPosts",
                    "99999999999999",
                )
                .unwrap();
                // The SQL cannot be reproduced, but the digest `generate` recorded proves the
                // files are untouched, so no --force is needed (#1835).
                fallback_plan.revert(Flags::default()).unwrap();
                assert!(!dir.exists());
            },
        );
    }

    #[test]
    fn destroy_add_search_migration_still_refuses_hand_edited_sql() {
        // With a recorded digest (#1835), an edited file still blocks the fallback (#1048).
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                let (tmp, dir) = searchable_post_with_migration();
                fs::remove_file(tmp.path().join("src/models/post.rs")).unwrap();
                fs::write(dir.join("up.sql"), "-- my own SQL\n").unwrap();

                let err = plan_migration_destroy_fallback(
                    tmp.path(),
                    "AddSearchToPosts",
                    "99999999999999",
                )
                .unwrap()
                .revert(Flags::default())
                .unwrap_err();

                assert!(matches!(err, GenerateError::Diverged(_)));
                assert!(dir.exists());
            },
        );
    }

    #[test]
    fn destroy_add_search_migration_without_provenance_still_needs_force() {
        // A project generated before the manifest (#1835) recorded no digest, so with the
        // search config gone the SQL is unverifiable.
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
                let (tmp, dir) = searchable_post_with_migration();
                fs::remove_file(tmp.path().join("src/models/post.rs")).unwrap();
                fs::remove_file(tmp.path().join(crate::generate::provenance::MANIFEST_PATH))
                    .unwrap();

                let err = plan_migration_destroy_fallback(
                    tmp.path(),
                    "AddSearchToPosts",
                    "99999999999999",
                )
                .unwrap()
                .revert(Flags::default())
                .unwrap_err();
                assert!(matches!(err, GenerateError::Diverged(_)));
                assert!(dir.exists());

                plan_migration_destroy_fallback(tmp.path(), "AddSearchToPosts", "99999999999999")
                    .unwrap()
                    .revert(Flags {
                        dry_run: false,
                        force: true,
                    })
                    .unwrap();
                assert!(!dir.exists());
            },
        );
    }

    #[test]
    fn plan_migration_destroy_fallback_fails_outside_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_migration_destroy_fallback(tmp.path(), "AddSearchToPosts", "20260427000000")
            .unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
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
        // A self-reference targets the table being altered, which exists, so no "model not
        // found" warning.
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

    #[test]
    fn add_columns_self_reference_errors_when_existing_model_has_uuid_pk() {
        // The altered table's model file on disk declares a UUID PK, so the self-reference is
        // still caught.
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("category.rs"),
            "#[autumn_web::model]\npub struct Category {\n    #[id]\n    pub id: uuid::Uuid,\n}\n",
        )
        .unwrap();

        let err = plan_migration(
            tmp.path(),
            "AddCategoryToCategories",
            &["category:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
        assert!(err.to_string().contains("self-referential"));
    }

    // ── optimistic locking (issue #1318) ────────────────────────────────────

    /// The contents of the planned `up.sql`/`down.sql` action.
    fn sql_action(plan: &Plan, file: &str) -> String {
        plan.actions
            .iter()
            .find(|a| a.path().file_name().is_some_and(|n| n == file))
            .map_or_else(
                || panic!("no {file} action"),
                |a| match a {
                    Action::Create { contents, .. } | Action::Modify { contents, .. } => {
                        contents.clone()
                    }
                    _ => String::new(),
                },
            )
    }

    #[test]
    fn adding_a_lock_version_column_carries_the_default_that_makes_it_usable() {
        // `#[lock_version]` keeps the column out of `New{Model}`, so a bare NOT NULL add would
        // fail every later insert. The DEFAULT also backfills existing rows.
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "AddLockVersionToPosts",
            &["lock_version:i32".into()],
            "20260427000000",
        )
        .unwrap();
        let up = sql_action(&plan, "up.sql");
        assert!(
            up.contains("ADD COLUMN lock_version INTEGER NOT NULL DEFAULT 0;"),
            "up.sql:\n{up}"
        );
    }

    #[test]
    fn adding_a_lock_version_column_of_the_wrong_type_is_rejected() {
        // Same feedback as `generate model` for the same token.
        let tmp = project();
        let err = plan_migration(
            tmp.path(),
            "AddLockVersionToPosts",
            &["lock_version:String".into()],
            "20260427000000",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("lock_version") && msg.contains("i32"),
            "got: {msg}"
        );
    }

    #[test]
    fn removing_a_legacy_lock_version_column_is_never_type_checked() {
        // Dropping a legacy `lock_version` is the escape hatch the other errors point at. The
        // rollback re-adds it with the caller's type, so a type check would block it.
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "RemoveLockVersionFromPosts",
            &["lock_version:String".into()],
            "20260427000000",
        )
        .expect("a removal migration must not be type-checked as a lock version");
        let down = sql_action(&plan, "down.sql");
        assert!(
            down.contains("ADD COLUMN lock_version TEXT"),
            "the rollback must restore the caller's original type:\n{down}"
        );
    }

    // ── `{encrypted}` is not a `generate migration` token (issue #1340) ─────

    /// R8: `generate migration` emits SQL only — it never touches a model
    /// file — so accepting `{encrypted}` here would add a plaintext column
    /// while the developer believes they declared encryption. That is exactly
    /// the silent failure issue #1340 exists to close, so refuse and name the
    /// two commands that really do wire the attribute.
    #[test]
    fn add_columns_migration_rejects_the_encrypted_modifier() {
        let tmp = project();
        let err = plan_migration(
            tmp.path(),
            "AddApiTokenToAccounts",
            &["api_token:String{encrypted}".into()],
            "20260427000000",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("encrypted"), "must name the modifier: {msg}");
        assert!(
            msg.contains("generate model") || msg.contains("generate scaffold"),
            "must point at the commands that emit the attribute: {msg}"
        );
        assert!(
            msg.contains("EncryptApiTokenOnAccounts") || msg.contains("Encrypt"),
            "must point at the existing encrypt-columns migration shape: {msg}"
        );
    }

    /// The pre-existing `Encrypt<Column>On<Table>` shape is unaffected — it
    /// takes no field tokens and stays the supported way to convert an
    /// existing plaintext column.
    #[test]
    fn encrypt_columns_migration_shape_still_works() {
        let tmp = project();
        let plan = plan_migration(
            tmp.path(),
            "EncryptApiTokenOnAccounts",
            &[],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_encrypt_api_token_on_accounts/up.sql"),
        )
        .unwrap();
        assert!(up.contains("api_token"), "up.sql: {up}");
    }
}
