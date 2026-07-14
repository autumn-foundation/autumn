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
    MigrationShape, add_columns_down_sql, add_columns_up_sql_for, add_search_down_sql,
    add_search_up_sql, detect_migration_shape, encrypt_columns_down_sql, encrypt_columns_up_sql,
    parse_model_search_config_for_table, remove_columns_down_sql_for, remove_columns_up_sql,
    singularize,
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

    // Determine the target app's database backend so the emitted ALTER TABLE
    // DDL is backend-aware (SQLite foundation, issue #1614).
    let backend = detect_backend(project_root);

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
            // `src/schema.rs`'s current content, so a `unique` field being
            // added here can't pick an index name that coincidentally
            // collides with a plain index on some other, already-existing
            // column from an earlier migration this one has no other way to
            // see (issue #1032 review follow-up). Empty string (no
            // collision-check widening) if the file doesn't exist yet.
            let existing_schema =
                std::fs::read_to_string(project_root.join("src/schema.rs")).unwrap_or_default();
            (
                add_columns_up_sql_for(backend, table, &fields, &existing_schema),
                add_columns_down_sql(table, &fields),
            )
        }
        MigrationShape::RemoveColumns { ref table } if !fields.is_empty() => {
            // `remove_columns_down_sql`'s rollback restores the FK
            // constraint/index for a `references` field (issue #1026), so it
            // needs the same UUID-PK guard as `AddColumns` — otherwise a
            // target with a UUID primary key still produces a `down.sql`
            // that fails to apply on rollback.
            super::model::check_reference_targets(&mut plan, project_root, &fields, table, None)?;
            let existing_schema =
                std::fs::read_to_string(project_root.join("src/schema.rs")).unwrap_or_default();
            (
                remove_columns_up_sql(table, &fields),
                remove_columns_down_sql_for(backend, table, &fields, &existing_schema),
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
            // Postgres FTS (`tsvector` + GIN) has no SQLite equivalent in this
            // slice; reject at generate time citing #1910 (issue #1614 AC #4)
            // rather than emit Postgres-only DDL that breaks on SQLite.
            if backend == autumn_web::config::DatabaseBackend::Sqlite {
                return Err(super::sqlite_search_unsupported_error());
            }
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
    use crate::generate::Flags;
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
        // issue #1048 PR review: a developer may drop a README.md or an
        // auxiliary fixture alongside the generated up.sql/down.sql. Destroy
        // must treat that extra file as divergence rather than silently
        // sweeping it up with `remove_dir_all`.
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
        // The restored FK constraint in remove_columns_down_sql needs the
        // same UUID-PK guard as AddColumns — otherwise a target with a UUID
        // primary key still produces a down.sql that fails on rollback.
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

    /// `AddSearchTo…` on a `SQLite` app is rejected at generate time citing #1910
    /// (Postgres `tsvector` FTS has no `SQLite` equivalent in this slice, AC #4).
    #[test]
    fn add_search_migration_on_sqlite_is_rejected_citing_1910() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
            let err =
                plan_migration(tmp.path(), "AddSearchToPosts", &[], "20260427000000").unwrap_err();
            let msg = err.to_string();
            assert!(
                matches!(err, GenerateError::Config(_)),
                "expected Config error, got: {err:?}"
            );
            assert!(msg.contains("1910"), "must cite issue #1910: {msg}");
            assert!(
                msg.contains("SQLite") && msg.contains("full-text search"),
                "message must be actionable: {msg}"
            );
        });
    }

    /// `Add…To…` on a `SQLite` app emits `SQLite`-valid column types
    /// (`i64` -> `INTEGER`, not Postgres `BIGINT`).
    #[test]
    fn add_columns_migration_on_sqlite_emits_sqlite_types() {
        with_no_db_env(|| {
            let tmp = sqlite_project();
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
                up.contains("ALTER TABLE posts ADD COLUMN views INTEGER NOT NULL"),
                "up.sql: {up}"
            );
            assert!(!up.contains("BIGINT"), "SQLite up.sql leaked BIGINT: {up}");
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

    // ── `plan_migration_destroy_fallback` (issue #1048 PR review) ───────────

    #[test]
    fn destroy_add_search_migration_after_model_already_destroyed_still_removes_it() {
        // A common cleanup order — destroying the model before destroying
        // an `AddSearchTo<Table>` migration that depended on its
        // `#[searchable]` config — must not strand the migration directory
        // just because `plan_migration_with_options` can no longer read it.
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            || {
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
}
"#;
                fs::write(models_dir.join("post.rs"), model_src).unwrap();

                let plan =
                    plan_migration(tmp.path(), "AddSearchToPosts", &[], "20260427000000").unwrap();
                plan.execute(Flags::default()).unwrap();
                let dir = tmp
                    .path()
                    .join("migrations/20260427000000_add_search_to_posts");
                assert!(dir.exists());

                // Simulate `autumn destroy model Post` having already run.
                fs::remove_file(models_dir.join("post.rs")).unwrap();
                assert!(
                    plan_migration(tmp.path(), "AddSearchToPosts", &[], "99999999999999").is_err()
                );

                let fallback_plan = plan_migration_destroy_fallback(
                    tmp.path(),
                    "AddSearchToPosts",
                    "99999999999999",
                )
                .unwrap();
                // Without --force: content is unverifiable (the search
                // config is gone), so it's treated as diverged and left in
                // place.
                let err = fallback_plan
                    .revert(Flags {
                        dry_run: false,
                        force: false,
                    })
                    .unwrap_err();
                assert!(matches!(err, GenerateError::Diverged(_)));
                assert!(dir.exists());

                let fallback_plan = plan_migration_destroy_fallback(
                    tmp.path(),
                    "AddSearchToPosts",
                    "99999999999999",
                )
                .unwrap();
                fallback_plan
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

    #[test]
    fn add_columns_self_reference_errors_when_existing_model_has_uuid_pk() {
        // Unlike `generate model`, `generate migration Add…To…` alters an
        // EXISTING table — if that table's own model file is on disk and
        // declares a UUID primary key, the self-reference must still be
        // caught (it's not "unknown PK type, can't check", it's "known PK
        // type, from the file the caller didn't think to look at").
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
}
