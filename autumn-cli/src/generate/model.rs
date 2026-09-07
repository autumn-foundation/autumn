//! `autumn generate model` — emit a `#[model]` struct, its migration, and a
//! `schema.rs` table block.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::dsl::{
    EncryptedMode, Field, FieldConstraints, FieldKind, IdType, parse_fields,
    randomized_equality_lookup_reason,
};
use super::emit::{Action, Plan, Revert};
use super::naming::{pascal, pluralize, snake};
use super::schema_edit::{
    add_mod_declaration, add_search_down_sql_for, add_search_up_sql_for,
    append_schema_table_with_id_for, create_table_sql_with_metadata_and_id_for, drop_table_sql,
    ensure_autumn_web_feature, link_models_into_seed_bin, position_triggers_down_sql_for,
    position_triggers_up_sql_for,
};
use super::{GenerateError, detect_backend, ensure_project_root, read_or_empty};
use autumn_web::config::DatabaseBackend;

/// Optional metadata applied to generated model fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelOptions {
    /// Field names that should receive `#[indexed]` and SQL indexes.
    pub indexes: Vec<String>,
    /// Field names that should receive a `CREATE UNIQUE INDEX` (issue
    /// #1032), mirroring `--index`'s ergonomics. Equivalent to marking the
    /// field with the DSL's inline `:unique` modifier — both converge on
    /// [`Field::unique`].
    pub uniques: Vec<String>,
    /// Validation specs in `field=rule` form.
    pub validations: Vec<String>,
    /// Default specs in `field=value` form.
    pub defaults: Vec<String>,
    /// Emit a `deleted_at: Option<NaiveDateTime>` field and a nullable
    /// `deleted_at TIMESTAMP NULL` column for soft-delete support.
    pub soft_delete: bool,
    /// Generate shard-aware handlers (`ShardedDb` instead of `Db`).
    pub sharded: bool,
    /// The field used as the sharding key (validated against model fields).
    /// Defaults to `tenant_id` if present, otherwise `id`.
    pub shard_key: Option<String>,
    /// Primary-key type emitted for the `id` column. Defaults to `BigSerial`
    /// (`BIGSERIAL`/`i64`); set to `Uuid` for non-enumerable identifiers.
    pub id_type: IdType,
    /// Text field names (`String`/`Text`) to make full-text searchable
    /// (issue #1319). Emits a struct-level `#[searchable(language = "english")]`
    /// plus per-field `#[searchable(weight = "…")]`, and a `search_vector`
    /// generated column + GIN index in the migration. Empty by default (the
    /// flag is purely additive; when unset, output is byte-for-byte identical).
    pub searchable: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMetadata {
    indexes: BTreeSet<String>,
    validations: BTreeMap<String, Vec<String>>,
    defaults: BTreeMap<String, String>,
    /// Full-text search config (issue #1319): the FTS dictionary language and
    /// the ordered `(field, weight)` list. `search_language` is `Some` iff any
    /// field is searchable.
    search_language: Option<String>,
    searchable: Vec<(String, char)>,
    /// The author model `#[commentable(by = ...)]` should name (issue #1367),
    /// detected from the project by
    /// [`super::commentable::detect_author_model`]. `None` emits a bare
    /// `#[commentable]`, which compiles — naming a model that does not exist
    /// would not.
    commentable_author: Option<String>,
}

impl ModelMetadata {
    /// Record the author model `#[commentable(by = ...)]` will name.
    pub fn set_commentable_author(&mut self, author: Option<&str>) {
        self.commentable_author = author.map(str::to_owned);
    }

    #[must_use]
    pub fn has_validator_rules(&self) -> bool {
        !self.validations.is_empty()
    }

    /// The FTS dictionary language when the model has searchable fields.
    #[must_use]
    pub fn search_language(&self) -> Option<&str> {
        self.search_language.as_deref()
    }

    /// The ordered `(field, weight)` pairs the `search_vector` column is built
    /// from (issue #1319). Empty when the model is not searchable.
    #[must_use]
    pub fn searchable(&self) -> &[(String, char)] {
        &self.searchable
    }

    #[must_use]
    pub const fn indexes(&self) -> &BTreeSet<String> {
        &self.indexes
    }

    #[must_use]
    pub const fn defaults(&self) -> &BTreeMap<String, String> {
        &self.defaults
    }

    #[must_use]
    pub const fn validations(&self) -> &BTreeMap<String, Vec<String>> {
        &self.validations
    }
}

/// Compute every action a `generate model` invocation would perform.
///
/// Planning-only step — no file is *written* here (that's [`Plan::execute`]).
/// It does read a few existing files (`mod.rs`/`schema.rs` to merge into, and
/// any `references` target's model file to validate it — see
/// [`check_reference_targets`]), so tests can inspect the emitted file list
/// and contents without any writes reaching disk.
///
/// # Errors
/// Surfaces project-layout, DSL, and naming errors before any file is written.
#[allow(dead_code)]
pub fn plan_model(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    timestamp: &str,
) -> Result<Plan, GenerateError> {
    plan_model_with_options(
        project_root,
        name,
        field_tokens,
        timestamp,
        &ModelOptions::default(),
    )
}

/// Compute every action a `generate model` invocation would perform, using
/// optional metadata flags supplied by higher-level generators.
///
/// # Errors
/// Surfaces project-layout, DSL, naming, and metadata errors before any file is written.
pub fn plan_model_with_options(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    timestamp: &str,
    options: &ModelOptions,
) -> Result<Plan, GenerateError> {
    plan_model_with_options_impl(project_root, name, field_tokens, timestamp, options, false)
}

/// [`plan_model_with_options`], but for `autumn destroy model` (and the
/// scaffold's own destroy path), which recomputes the plan it is about to
/// revert.
///
/// Skips the *generation-only* semantic checks: a model created before those
/// checks existed — a `lock_version:String` column, say, or a lock-only model —
/// must still be removable. Refusing during the recompute happens before
/// [`Plan::revert`] ever sees `--force`, so it would strand exactly the files
/// the user is asking to delete. Same posture as the scaffold's shared-layout
/// preflight (issue #1834) and the migration destroy fallback (issue #1048).
///
/// Structural errors (project layout, bad field syntax, name collisions) still
/// apply: without them there is no plan to revert at all.
///
/// # Errors
/// Surfaces project-layout, DSL, and naming errors before any file is touched.
pub fn plan_model_with_options_for_revert(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    timestamp: &str,
    options: &ModelOptions,
) -> Result<Plan, GenerateError> {
    plan_model_with_options_impl(project_root, name, field_tokens, timestamp, options, true)
}

/// Shared implementation of [`plan_model_with_options`]. `for_revert` skips the
/// generation-only compatibility checks — see
/// [`plan_model_with_options_for_revert`].
#[allow(
    clippy::too_many_lines,
    reason = "linear sequence of independent file/revert steps mirroring the files this \
              generator emits; splitting it up would not make any single step clearer"
)]
fn plan_model_with_options_impl(
    project_root: &Path,
    name: &str,
    field_tokens: &[String],
    timestamp: &str,
    options: &ModelOptions,
    for_revert: bool,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    validate_resource_name(name)?;
    let mut fields = parse_fields(field_tokens)?;
    apply_unique_flags(&mut fields, &options.uniques)?;
    validate_field_names(&fields)?;
    // Issue #1340: the flag spellings of "make this encrypted column
    // equality-queryable"/"give it a default" only become visible once
    // `--unique`/`--default` have been folded in, so this runs after
    // `apply_unique_flags` and before anything is emitted. Generation-only —
    // see the function's contract for why `destroy` must skip it.
    if !for_revert {
        validate_encrypted_fields(&fields, options)?;
        // Issue #1384: same reasoning as `validate_encrypted_fields` above —
        // `--unique`/`--index`/`--searchable`/`--shard-key` are flag spellings
        // of constraints the `{translatable}` parse-time cross-checks cannot
        // see, because they are folded in after `parse_fields`.
        validate_translatable_fields(&fields, options)?;
    }
    let pascal_name = pascal(name);
    validate_enum_field_collisions(&pascal_name, &fields)?;
    // Determine the target app's database backend so the emitted DDL / diesel
    // schema is backend-aware (SQLite foundation, issue #1614). Full-text search
    // (`--searchable`) is now supported on both backends — Postgres emits a
    // `tsvector` column + GIN index, SQLite emits an FTS5 virtual table +
    // triggers (issue #1910) — so it is no longer rejected here.
    //
    // Read before the metadata: a `--default` literal is rendered per backend
    // (issue #1924).
    let backend = detect_backend(project_root);
    let mut metadata = parse_model_metadata_for(backend, &fields, options)?;
    // Issue #1367: `#[commentable(by = ...)]` may only name a model that
    // actually exists in this project — naming a missing one would be a
    // compile error in a file the author did not write.
    metadata.set_commentable_author(super::commentable::detect_author_model(project_root));

    // A UUID primary key needs app-side id generation on SQLite (no
    // `gen_random_uuid()`), which is part of the deferred runtime slice #1905;
    // reject `--id uuid` on a SQLite app at generate time rather than emit a
    // `TEXT PRIMARY KEY` column that would accept NULL/omitted ids (AC #4).
    if backend == autumn_web::config::DatabaseBackend::Sqlite && options.id_type == IdType::Uuid {
        return Err(super::sqlite_uuid_pk_unsupported_error());
    }
    // `comments:commentable` on a UUID-keyed model would plan every file and
    // then hand back a project that does not compile: the shared table stores
    // `commentable_id BIGINT` and the generated helpers take `parent_id: i64`.
    // Refused here, before anything is written, exactly as the SQLite/UUID
    // combination above is.
    if options.id_type == IdType::Uuid && fields.iter().any(|f| f.kind.is_commentable()) {
        return Err(super::uuid_pk_commentable_unsupported_error());
    }
    // Every DSL field kind converts on SQLite as of #1924, so this is a standing
    // guard rather than an active gate: a NEW kind with no working conversion is
    // reported here rather than emitted as code that cannot compile (AC #4).
    if backend == autumn_web::config::DatabaseBackend::Sqlite {
        super::reject_sqlite_unsupported_field_kinds(&fields)?;
    }
    // Sharding (`--sharded`) emits a `#[shard_key]` model plus (via scaffold)
    // `ShardedDb` routes/migrations that require a `[[database.shards]]`
    // topology, which `DatabaseConfig::validate_backend_consistency` rejects for
    // a SQLite primary — so no valid SQLite config could use the generated
    // resource. SQLite is single-host/single-writer, so this is a PERMANENT
    // constraint (Postgres-only), not a deferred slice. Reject at generate time
    // rather than emit a resource no SQLite app can boot (AC #4). `generate
    // scaffold --sharded` routes through here too, so this one gate covers both.
    if backend == autumn_web::config::DatabaseBackend::Sqlite && options.sharded {
        return Err(super::sqlite_sharded_unsupported_error());
    }

    let snake_name = snake(name);
    let table = pluralize(&snake_name);

    // When soft_delete is enabled, append a virtual `deleted_at` field so
    // the SQL migration and schema.rs block include the nullable column.
    let schema_fields = augment_fields_for_soft_delete(&fields, options.soft_delete)?;

    // #1318: `lock_version` is managed by the database, so it contributes nothing to
    // `New{Model}`, and neither does any `--default` column. A model whose columns are all
    // database-managed therefore emits an empty `New{Model}`, whose Diesel `Insertable`
    // derive does not compile, leaving the generated project dead on arrival.
    //
    // The check is on the effective set rather than the declared token count: `Post
    // title:String lock_version:i32 --default title=x` declares two columns and leaves
    // zero. `metadata.defaults` carries both the explicit `--default` columns and, from
    // `parse_model_metadata`, the lock column.
    //
    // Scoped to lock-version models deliberately. A model whose every column is
    // `--default`ed, or one declared with no fields, has always emitted this same
    // uncompilable struct; widening the refusal to those pre-existing cases is a separate
    // change from wiring #1318, and would move a fieldless `generate scaffold Post` from a
    // compile error to a planning error an existing test pins.
    if !for_revert {
        validate_lock_version_field(&fields, &options.defaults)?;
    }
    if !for_revert
        && lock_version_field(&fields).is_some()
        && fields
            .iter()
            .all(|f| metadata.defaults().contains_key(&f.name))
    {
        return Err(GenerateError::Config(format!(
            "this model has no insertable columns: `{LOCK_VERSION_COLUMN}` is managed by the \
             database (and so is every `--default` column), so the generated \
             New{pascal_name} struct would have no fields at all and the project would not \
             compile. Declare at least one ordinary column alongside `{LOCK_VERSION_COLUMN}`."
        )));
    }

    let mut plan = Plan::new(project_root);
    // Issue #1318: `lock_version` is a magic column name — declaring it changes
    // the model's semantics (DB-managed, kept out of `New{Model}`, carried on
    // `Update{Model}` as the expected version, conflict-checked by the
    // repository, and hidden from a scaffold's form). That is the whole point
    // for someone who wanted optimistic locking, and a nasty surprise for
    // someone who just wanted a counter with that name — so say so out loud
    // rather than letting the reinterpretation happen silently.
    if lock_version_field(&fields).is_some() {
        plan.warn(format!(
            "`{LOCK_VERSION_COLUMN}` opts this model into optimistic locking: the column is \
             managed by the database (excluded from New{pascal_name}, carried on \
             Update{pascal_name} as the expected version, defaulted to 0 in the migration) and \
             a scaffolded form carries it in a hidden field rather than an editable control. \
             Rename the column if you wanted an ordinary integer you set yourself."
        ));
    }
    // Issue #1340: an encrypted column is inert without key material — the app
    // boots, but the first read or write of the column fails. Say so with the
    // exact command and credential paths rather than leaving it to a runtime
    // error. (Emitted for the `generate scaffold` path too, which delegates
    // its model plan here.)
    if let Some(warning) = encryption_key_material_warning(&fields) {
        plan.warn(warning);
    }
    check_reference_targets(
        &mut plan,
        project_root,
        &fields,
        &table,
        Some(options.id_type),
    )?;

    // ── Polymorphic comments (issue #1367) ─────────────────────────────────
    // The `comments:commentable` token also has to bring the shared comments
    // table, or this model's `#[commentable]` compiles and then fails at
    // runtime with `relation "comments" does not exist`. `generate scaffold`
    // routes through its own copy of this because it owns the warnings; this is
    // the `generate model` path, which the scaffold does not reach.
    // On the destroy path the field tokens are not repeated, so the
    // declaration is recovered from the model file instead.
    if fields.iter().any(|f| f.kind.is_commentable())
        || (for_revert && super::commentable::model_declares_commentable(project_root, &snake_name))
    {
        // On a revert the shared table stays as long as ANY other model still
        // declares `#[commentable]`: it is one table for all of them.
        let revert_would_orphan_another_model = for_revert
            && super::commentable::another_model_is_still_commentable(project_root, &snake_name);
        let emitted = !revert_would_orphan_another_model
            && super::commentable::push_commentable_migration(
                &mut plan,
                project_root,
                timestamp,
                backend,
                for_revert,
            );
        if !for_revert {
            if emitted && super::commentable::conflicting_comments_table(project_root) {
                plan.warn(format!(
                    "This project already has a `{table}` table that is NOT the \
                     polymorphic one — a `Comment` model scaffolded the ordinary way \
                     creates exactly that, and the shared table takes the same name. \
                     Both `CREATE TABLE {table}` statements will be applied and \
                     `migrate` will stop on \"already exists\". Rename or drop the \
                     existing table, or add `commentable_type TEXT NOT NULL` and \
                     `commentable_id BIGINT NOT NULL` to it and delete the migration \
                     just written.",
                    table = super::commentable::COMMENTS_TABLE,
                ));
            }
            plan.warn(if emitted {
                format!(
                    "Added the shared `{table}` table. Every `#[commentable]` model attaches \
                     to it, so later models need no migration of their own.",
                    table = super::commentable::COMMENTS_TABLE,
                )
            } else {
                format!(
                    "Reusing the existing `{table}` table — the polymorphic comments table \
                     is shared across every `#[commentable]` model.",
                    table = super::commentable::COMMENTS_TABLE,
                )
            });
        }
    }

    // (a) `src/models/<snake>.rs` + `src/models/mod.rs`
    let models_dir = project_root.join("src").join("models");
    let model_file = models_dir.join(format!("{snake_name}.rs"));
    plan.create(
        model_file,
        render_model_file(
            &pascal_name,
            &table,
            &fields,
            &metadata,
            options.soft_delete,
            if options.sharded {
                options.shard_key.as_deref()
            } else {
                None
            },
            options.id_type,
            backend,
        ),
    );

    let mod_path = models_dir.join("mod.rs");
    let mod_existing = read_or_empty(&mod_path);
    plan.modify(
        mod_path.clone(),
        add_mod_declaration(&mod_existing, &snake_name),
    );
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: mod_path,
        name: snake_name,
    });

    // (b) Diesel migration
    let migration_dir_name = format!("{timestamp}_create_{table}");
    let migration_dir = project_root.join("migrations").join(&migration_dir_name);
    let table_sql = create_table_sql_with_metadata_and_id_for(
        backend,
        &table,
        &schema_fields,
        metadata.indexes(),
        metadata.defaults(),
        options.id_type,
    );
    let up_sql = if options.sharded {
        format!(
            "-- Sharded model: this migration runs against the control DB by default.\n\
             -- To apply to shards, run: autumn migrate --shard <name>\n\
             -- See: autumn migrate --help\n\
             {table_sql}"
        )
    } else {
        table_sql
    };
    // Full-text search (issues #1319, #1910): backend-aware FTS scaffold emitted
    // in the same create-table migration so `autumn migrate` yields a working
    // search with zero manual SQL. On Postgres this is the `search_vector`
    // generated column + GIN index; on SQLite it is an external-content FTS5
    // virtual table + maintenance triggers (`add_search_up_sql_for`). Neither is
    // added to the model struct or schema.rs (the macro loads matched rows by id
    // via raw SQL), so the FTS objects stay outside the model surface on both
    // backends.
    let (up_sql, down_sql) = if metadata.searchable().is_empty() {
        (up_sql, drop_table_sql(&table))
    } else {
        let language = metadata.search_language().unwrap_or("english");
        let search_up = add_search_up_sql_for(backend, &table, language, metadata.searchable())?;
        let search_down = add_search_down_sql_for(backend, &table);
        (
            format!("{up_sql}\n{search_up}"),
            format!("{search_down}{}", drop_table_sql(&table)),
        )
    };
    // Issue #1358: `position`-field maintenance triggers. Appended after the
    // (optional) search scaffold, same reasoning as that block — these are
    // independent DDL objects tied to the table, not the model struct/schema.rs
    // surface. Empty string (byte-identical output) for the overwhelmingly
    // common case of no `position` field.
    let position_up = position_triggers_up_sql_for(backend, &table, &schema_fields);
    let position_down = position_triggers_down_sql_for(backend, &table, &schema_fields);
    let up_sql = if position_up.is_empty() {
        up_sql
    } else {
        format!("{up_sql}\n{position_up}")
    };
    let down_sql = if position_down.is_empty() {
        down_sql
    } else {
        format!("{position_down}{down_sql}")
    };
    // Issue #1367: the cascade a polymorphic foreign key cannot express. The
    // shared `comments` table is created once and cannot know which models will
    // later attach to it, so each commentable parent carries its own cleanup
    // trigger. Without it a deleted parent leaves its thread behind —
    // unreachable, and worse than unreachable if the id is ever reused, since
    // the old comments would surface under the new record.
    let commentable_up = if fields.iter().any(|f| f.kind.is_commentable()) {
        super::commentable::parent_cleanup_sql(backend, &table, &pascal_name)
    } else {
        String::new()
    };
    let (up_sql, down_sql) = if commentable_up.is_empty() {
        (up_sql, down_sql)
    } else {
        (
            format!("{up_sql}{commentable_up}"),
            // Dropped before the table so the trigger never outlives its
            // target. The statement is backend-split: SQLite's DROP TRIGGER
            // takes no `ON <table>`.
            format!(
                "{}{down_sql}",
                super::commentable::parent_cleanup_down_sql(backend, &table)
            ),
        )
    };

    plan.create(migration_dir.join("up.sql"), up_sql);
    plan.create(migration_dir.join("down.sql"), down_sql);

    // (c) `src/schema.rs` entry
    let schema_path = project_root.join("src").join("schema.rs");
    let schema_existing = read_or_empty(&schema_path);
    plan.modify(
        schema_path.clone(),
        append_schema_table_with_id_for(
            backend,
            &schema_existing,
            &table,
            &schema_fields,
            options.id_type,
        ),
    );
    plan.push_revert(crate::generate::emit::Revert::SchemaTable {
        path: schema_path,
        table: table.clone(),
        expected_block: append_schema_table_with_id_for(
            backend,
            "",
            &table,
            &schema_fields,
            options.id_type,
        ),
    });

    // (d) `Cargo.toml` deps — `#[autumn_web::model]` expands to references
    // for `diesel`, `serde`, `serde_json`, `chrono`, and supported field crates
    // such as `uuid`, none of which are in the freshly-`autumn new`-ed project.
    let mut deps: Vec<(&str, &str)> = model_deps(backend).to_vec();
    if metadata.has_validator_rules() {
        deps.push((
            "validator",
            "{ version = \"0.20\", features = [\"derive\"] }",
        ));
    }
    if schema_fields.iter().any(|f| f.kind.is_decimal()) {
        deps.push((
            "rust_decimal",
            match backend {
                DatabaseBackend::Postgres => {
                    "{ version = \"1\", features = [\"db-diesel2-postgres\", \"serde\"] }"
                }
                DatabaseBackend::Sqlite => "{ version = \"1\", features = [\"serde\"] }",
            },
        ));
        let existing_cargo_toml = read_or_empty(&project_root.join("Cargo.toml"));
        warn_if_existing_dep_missing_features(
            &mut plan,
            &existing_cargo_toml,
            "rust_decimal",
            decimal_dep_features(backend),
        );
    }
    plan_cargo_deps(
        &mut plan,
        project_root,
        &deps,
        &project_root.join("src/models"),
    );
    // A SQLite app links a different backend inside `autumn-web` too: the
    // `sqlite` feature flips `RuntimeConnection` and supplies the `SqliteUuid` /
    // `SqliteDecimal` conversions the model file names (issue #1924).
    if backend == DatabaseBackend::Sqlite {
        plan_autumn_web_feature(&mut plan, project_root, "sqlite");
    }

    // (e) Link the new model (and the `schema` module it reads) into the
    // standalone `src/bin/seed.rs` binary, when the project was scaffolded
    // with one (`autumn new --with-seed`). Without this the model's
    // `#[model]` inventory registration is never compiled into the seed
    // binary, so `autumn seed --count N --model M` cannot resolve it
    // (issue #1718; completes AC4 of #1343). `generate scaffold` reuses this
    // planner, so it inherits the same wiring.
    plan_seed_bin_linking(&mut plan, project_root);

    // (f) Issue #1384: a `{translatable}` column lowers to
    // `autumn_web::i18n::Translated`, and `autumn_web::i18n` is behind the
    // NON-DEFAULT `i18n` feature. Without this the generated model would fail
    // to compile with `E0433: could not find 'i18n' in 'autumn_web'` across
    // code the author did not write. `{encrypted}` — the closest precedent —
    // needs no such wiring because `autumn_web::encryption` is ungated; this is
    // the first field-DSL modifier that lowers to a gated module. Mirrors what
    // `generate scaffold --i18n` already does for the view lane.
    if schema_fields.iter().any(Field::is_translatable) {
        plan_autumn_web_feature(&mut plan, project_root, "i18n");
    }

    Ok(plan)
}

/// Ensure `autumn-web`'s `features = [...]` list in the project `Cargo.toml`
/// contains `feature`, folding into any `Modify` action already staged for that
/// file so two planners cannot clobber each other's edit.
fn plan_autumn_web_feature(plan: &mut Plan, project_root: &Path, feature: &str) {
    let cargo_path = project_root.join("Cargo.toml");
    let base = plan
        .actions
        .iter()
        .rev()
        .find_map(|a| match a {
            Action::Modify { path, contents } if path == &cargo_path => Some(contents.clone()),
            _ => None,
        })
        .unwrap_or_else(|| read_or_empty(&cargo_path));
    let updated = ensure_autumn_web_feature(&base, feature);
    if updated != base {
        plan.actions.retain(|a| a.path() != cargo_path);
        plan.modify(cargo_path, updated);
    }
    // Register the revert UNCONDITIONALLY, not only when the edit changed
    // something. `autumn destroy model` recomputes this same plan, and by then
    // the feature is already present — so a revert pushed inside the `if` above
    // would never be registered on the path that needs it, and `destroy` would
    // leave the non-default feature enabled forever. `owner_dir` is
    // `src/models`, so the feature survives until the LAST model is destroyed
    // (the same ownership rule the scaffold and channel generators use).
    plan.push_revert(Revert::CargoAutumnWebFeature {
        path: project_root.join("Cargo.toml"),
        feature: feature.to_owned(),
        owner_dir: Some(project_root.join("src/models")),
    });
}

/// Add a `Modify` action linking `src/models/` + `src/schema.rs` into
/// `src/bin/seed.rs` when that binary exists, unless it's already linked
/// (see [`link_models_into_seed_bin`]). A no-op — no action queued — when the
/// project has no seed binary or the declarations are already present, so a
/// plain non-`--with-seed` project's plan and output are unchanged.
fn plan_seed_bin_linking(plan: &mut Plan, project_root: &Path) {
    let seed_path = project_root.join("src").join("bin").join("seed.rs");
    if !seed_path.exists() {
        return;
    }
    let existing = read_or_empty(&seed_path);
    let linked = link_models_into_seed_bin(&existing);
    if linked != existing {
        plan.modify(seed_path.clone(), linked);
    }
    // Record the destroy-time inverse regardless of whether THIS invocation had
    // to inject (a later model reuses the same already-linked file): `autumn
    // destroy` deletes `src/models/mod.rs` + `src/schema.rs` once the last
    // model's reverts empty them, which would leave these `#[path]` links
    // dangling at missing files. `owner_dir = src/models` gates the revert so it
    // fires only when the last model is destroyed (no other model file remains),
    // keeping the links while sibling models still live (issue #1718).
    if seed_path.exists() {
        plan.push_revert(crate::generate::emit::Revert::SeedBinLinks {
            path: seed_path,
            owner_dir: project_root.join("src").join("models"),
        });
    }
}

/// Whether resource `base`'s model can be found anywhere in the project,
/// checking both layouts the codebase already treats as valid (see
/// `migration.rs`'s `AddSearch` migration shape, which checks the same two
/// locations): the per-resource `src/models/<base>.rs` (mere existence
/// counts — it's this resource's own file, however it's written), and the
/// single-file `src/models.rs` (only counts if its content actually declares
/// the resource's table, via a word-boundary-aware check so `posts` isn't
/// confused with a longer table name like `posts_tags`).
///
/// Shared with the scaffold generator (`pub(super)`): the same presence test
/// that decides whether [`check_reference_targets`] warns also decides
/// whether a scaffolded `references` column can render as a `<select>` of
/// the target table's ids (which needs the target's `src/schema.rs` entry)
/// or must fall back to the derived numeric id input.
pub(super) fn model_file_exists(project_root: &Path, table: &str, base: &str) -> bool {
    let per_resource = project_root
        .join("src")
        .join("models")
        .join(format!("{base}.rs"));
    if per_resource.exists() {
        return true;
    }
    let single_file = project_root.join("src").join("models.rs");
    single_file.exists() && declares_schema_table(&read_or_empty(&single_file), table)
}

/// True if `content` declares `use crate::schema::<table>` for exactly
/// `table` — either as a bare `schema::<table>;` import or as one entry of a
/// grouped import (`schema::{<table>, other};`, possibly wrapped across
/// multiple lines), which is a multi-model `src/models.rs` layout this repo
/// itself uses (see `examples/reddit-clone/src/models.rs`:
/// `use crate::schema::{comments, posts, subreddits, users, votes};`).
/// Word-boundary-aware so `posts` isn't confused with a longer table name
/// like `posts_tags`.
fn declares_schema_table(content: &str, table: &str) -> bool {
    for (idx, _) in content.match_indices("schema::") {
        let after = &content[idx + "schema::".len()..];
        if let Some(rest) = after.strip_prefix('{') {
            let Some(end) = rest.find('}') else {
                continue;
            };
            if rest[..end].split(',').any(|name| name.trim() == table) {
                return true;
            }
        } else if after.starts_with(table)
            && after[table.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        {
            return true;
        }
    }
    false
}

/// Locate the source text of resource `base`'s `#[model]` struct body (the
/// text between its opening and matching closing brace), checking both
/// model-file layouts (see [`model_file_exists`]).
///
/// Scoping to the specific struct's body (rather than the whole file) means
/// callers can inspect the resource's own `#[id]` field even when other
/// models are declared in the same `src/models.rs`. Returns `None` when the
/// struct can't be located/parsed (e.g. a hand-written model using an
/// unconventional struct name, or the file fails to parse as valid Rust) —
/// callers should treat that as "can't verify, don't guess" rather than
/// "model missing", since [`model_file_exists`] already established the
/// model is present.
///
/// Parses the file with `syn` rather than scanning text: earlier text-based
/// heuristics here kept missing real UUID primary keys behind cosmetic
/// variation the compiler doesn't care about — grouped attributes, doc
/// comments, and both `//` and `/* */` comments all defeated a
/// string-matching approach one at a time (issue #1026 follow-ups). A real
/// parse isn't fooled by any of that, since it operates on the token tree,
/// not the source text.
fn model_struct_has_uuid_pk(project_root: &Path, base: &str) -> bool {
    let pascal_name = pascal(base);
    let per_resource = project_root
        .join("src")
        .join("models")
        .join(format!("{base}.rs"));
    let content = if per_resource.exists() {
        read_or_empty(&per_resource)
    } else {
        let single_file = project_root.join("src").join("models.rs");
        if !single_file.exists() {
            return false;
        }
        read_or_empty(&single_file)
    };

    let Ok(file) = syn::parse_file(&content) else {
        return false;
    };

    let Some(item_struct) = file.items.iter().find_map(|item| match item {
        syn::Item::Struct(s) if s.ident == pascal_name => Some(s),
        _ => None,
    }) else {
        return false;
    };

    let syn::Fields::Named(fields) = &item_struct.fields else {
        return false;
    };
    fields
        .named
        .iter()
        .find(|field| field.attrs.iter().any(|a| a.path().is_ident("id")))
        .is_some_and(|field| type_is_uuid(&field.ty))
}

/// Whether a `syn::Type` is a UUID — either the fully-qualified `uuid::Uuid`
/// (what the generator itself always emits) or the bare `Uuid` (idiomatic
/// with a `use uuid::Uuid;` import at the top of the file, a common
/// hand-edit of a generated model).
fn type_is_uuid(ty: &syn::Type) -> bool {
    let syn::Type::Path(type_path) = ty else {
        return false;
    };
    if type_path.qself.is_some() {
        return false;
    }
    match type_path.path.segments.len() {
        1 => type_path.path.segments[0].ident == "Uuid",
        2 => {
            type_path.path.segments[0].ident == "uuid" && type_path.path.segments[1].ident == "Uuid"
        }
        _ => false,
    }
}

/// The `String`/`Text` columns of resource `base`'s `#[model]` struct, in
/// declaration order, each paired with whether it is nullable
/// (`Option<String>`).
///
/// The scaffold generator uses this to pick a human-friendly display column
/// for a `references` field (issue #1146): a `belongs_to` `<select>` and the
/// index/show views render this column's value instead of the raw foreign-key
/// id. Returns an empty vec when the model can't be located/parsed or has no
/// string column — the caller then falls back to rendering the id, exactly the
/// pre-#1146 behavior.
///
/// Parses with `syn` (like [`model_struct_has_uuid_pk`]) so grouped
/// attributes, doc comments, and a multi-model `src/models.rs` layout don't
/// defeat it.
/// The doc comment the model generator puts on a `richtext` column (issue
/// #1255), and the marker [`model_string_columns`] matches to exclude it from
/// `references` display-label candidates.
///
/// A `richtext` column's Rust type is a bare `String`, identical to
/// `String`/`Text`, so the rendered source carries no other signal. Editing or
/// removing this line only downgrades label selection to the pre-#1255
/// behaviour (a Markdown body may be chosen as a `<select>` label) — nothing
/// breaks.
pub(super) const RICH_TEXT_MARKER_DOC: &str =
    "Markdown source (rich text) — render with `autumn_web::markdown::render_user_content`.";

/// Whether `field`'s attributes carry the [`RICH_TEXT_MARKER_DOC`] marker.
fn has_rich_text_marker(field: &syn::Field) -> bool {
    field.attrs.iter().any(|attr| {
        let syn::Meta::NameValue(nv) = &attr.meta else {
            return false;
        };
        if !nv.path.is_ident("doc") {
            return false;
        }
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) = &nv.value
        else {
            return false;
        };
        s.value().trim() == RICH_TEXT_MARKER_DOC
    })
}

pub(super) fn model_string_columns(project_root: &Path, base: &str) -> Vec<(String, bool)> {
    let pascal_name = pascal(base);
    let per_resource = project_root
        .join("src")
        .join("models")
        .join(format!("{base}.rs"));
    let content = if per_resource.exists() {
        read_or_empty(&per_resource)
    } else {
        let single_file = project_root.join("src").join("models.rs");
        if !single_file.exists() {
            return Vec::new();
        }
        read_or_empty(&single_file)
    };

    let Ok(file) = syn::parse_file(&content) else {
        return Vec::new();
    };
    let Some(item_struct) = file.items.iter().find_map(|item| match item {
        syn::Item::Struct(s) if s.ident == pascal_name => Some(s),
        _ => None,
    }) else {
        return Vec::new();
    };
    let syn::Fields::Named(fields) = &item_struct.fields else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for field in &fields.named {
        let Some(ident) = field.ident.as_ref() else {
            continue;
        };
        // A `richtext` column is a `String` in Rust but must not be offered as
        // a `references` display label (issue #1255) — see
        // [`RICH_TEXT_MARKER_DOC`]. This mirrors the self-reference path in
        // `scaffold::target_string_columns`, which filters on the `FieldKind`
        // directly because the in-flight columns are still typed there.
        if has_rich_text_marker(field) {
            continue;
        }
        if let Some(nullable) = string_like_nullability(&field.ty) {
            out.push((ident.to_string(), nullable));
        }
    }
    out
}

/// `Some(false)` for a `String` field, `Some(true)` for `Option<String>`,
/// `None` for any non-string type.
fn string_like_nullability(ty: &syn::Type) -> Option<bool> {
    if let Some(inner) = option_inner_type(ty) {
        return type_is_string(inner).then_some(true);
    }
    type_is_string(ty).then_some(false)
}

/// The `T` of an `Option<T>` type, or `None` when `ty` isn't an `Option<…>`.
fn option_inner_type(ty: &syn::Type) -> Option<&syn::Type> {
    let syn::Type::Path(tp) = ty else {
        return None;
    };
    let seg = tp.path.segments.last()?;
    if seg.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

/// Whether `ty` is the bare `String` type (the only spelling the model
/// generator emits for `String`/`Text` columns).
fn type_is_string(ty: &syn::Type) -> bool {
    let syn::Type::Path(tp) = ty else {
        return false;
    };
    tp.qself.is_none() && tp.path.segments.last().is_some_and(|s| s.ident == "String")
}

/// Validate every `references` field against its target model (issue #1026):
///
/// - A *self*-reference (the target table is `own_table` — the resource this
///   very command is generating) is checked against `own_id_type` directly
///   instead of a filesystem lookup: the target model's file doesn't exist
///   yet (this command is creating it), so a file-existence check could
///   never see it, and a naive "not found, assume it exists" warning would
///   both misfire (the table obviously exists — it's the one being created)
///   and, worse, skip the UUID check entirely. `own_id_type` is `None` for
///   callers that don't track a PK type for the table being altered (`generate
///   migration Add…To…`, which only ever `ALTER TABLE`s an existing table) —
///   in that case the self-reference is simply left unvalidated.
/// - Otherwise, if the target model can't be found anywhere (AC8), record a
///   warning on `plan` — the generator still scaffolds the FK column,
///   constraint, and index, simply assuming the referenced table already
///   exists (e.g. it's created by an out-of-band migration, or generated in
///   a later command).
/// - If the target model *can* be found and its own `#[id]` field is a UUID,
///   fail loudly: `references` only supports the i64/BIGSERIAL PK convention
///   (see issue #1026's scope), and emitting a `BIGINT` FK column against a
///   `UUID PRIMARY KEY` would produce a migration that fails at
///   `autumn migrate` time with an opaque Postgres type-mismatch error —
///   better to fail fast here with an actionable message.
///
/// Shared by `generate model`, `generate scaffold` (via
/// [`plan_model_with_options`]), and `generate migration Add…To…` (via
/// `migration::plan_migration`) so a `references` field gets the same
/// feedback regardless of which subcommand declares it.
///
/// # Errors
/// Returns [`GenerateError::Config`] when a referenced (or self-referenced)
/// model's `#[id]` field is a UUID.
pub(super) fn check_reference_targets(
    plan: &mut Plan,
    project_root: &Path,
    fields: &[Field],
    own_table: &str,
    own_id_type: Option<IdType>,
) -> Result<(), GenerateError> {
    for f in fields {
        if !f.kind.is_reference() {
            continue;
        }
        let Some(table) = f.reference_table() else {
            continue;
        };
        let base = f.name.strip_suffix("_id").unwrap_or(&f.name);

        if table == own_table {
            match own_id_type {
                Some(IdType::Uuid) => {
                    return Err(GenerateError::Config(format!(
                        "'{}' is a self-referential foreign key, but this model uses a UUID \
                         primary key (`--id uuid`). `references` fields only support \
                         i64/BIGSERIAL-keyed targets (issue #1026).",
                        f.name
                    )));
                }
                // Known non-UUID PK: the table is being created right now
                // with that type, so the self-reference is fine.
                Some(_) => continue,
                // Unknown (`generate migration Add…To…`, altering a table
                // whose PK type isn't tracked here): unlike `generate model`,
                // this table may already exist on disk, so still check its
                // model file for a UUID PK if one is found — but don't warn
                // "model not found" for a self-reference, since the table
                // obviously already exists (that's the point of ALTER TABLE).
                None => {
                    if model_struct_has_uuid_pk(project_root, base) {
                        return Err(GenerateError::Config(format!(
                            "'{}' is a self-referential foreign key, but the existing model \
                             declares a UUID primary key. `references` fields only support \
                             i64/BIGSERIAL-keyed targets (issue #1026).",
                            f.name
                        )));
                    }
                    continue;
                }
            }
        }

        if !model_file_exists(project_root, &table, base) {
            plan.warn(format!(
                "'{}' references model '{base}', but src/models/{base}.rs (or a matching \
                 src/models.rs declaration) was not found — assuming table '{table}' \
                 already exists.",
                f.name
            ));
            continue;
        }
        if model_struct_has_uuid_pk(project_root, base) {
            return Err(GenerateError::Config(format!(
                "'{}' references model '{base}', which declares a UUID primary key. \
                 `references` fields only support i64/BIGSERIAL-keyed targets \
                 (issue #1026) — hand-write the migration for a UUID foreign key instead.",
                f.name
            )));
        }
    }
    Ok(())
}

/// Append the virtual `deleted_at` column that `--soft-delete` models add to
/// their migration and `schema.rs` block, matching what [`plan_model_with_options`]
/// applies before rendering those files. Shared with the scaffold generator so
/// the smoke test's throwaway table (built from the same field list) doesn't
/// drift from the real migration's schema for soft-delete models.
///
/// # Errors
/// Returns [`GenerateError::InvalidField`] when `soft_delete` is set and
/// `fields` already declares a `deleted_at` field (that name is reserved for
/// `--soft-delete`).
pub(super) fn augment_fields_for_soft_delete(
    fields: &[Field],
    soft_delete: bool,
) -> Result<std::borrow::Cow<'_, [Field]>, GenerateError> {
    if !soft_delete {
        return Ok(std::borrow::Cow::Borrowed(fields));
    }
    if fields.iter().any(|f| f.name == "deleted_at") {
        return Err(GenerateError::InvalidField {
            token: "deleted_at".to_owned(),
            reason: "'deleted_at' is managed by --soft-delete; remove it from the field list"
                .to_owned(),
        });
    }
    let mut augmented = fields.to_vec();
    augmented.push(Field {
        name: "deleted_at".to_owned(),
        kind: FieldKind::NaiveDateTime,
        nullable: true,
        variants: Vec::new(),
        unique: false,
        constraints: FieldConstraints::default(),
        state_machine: None,
    });
    Ok(std::borrow::Cow::Owned(augmented))
}

/// Direct dependencies the *model* generator's Postgres output requires at
/// compile time. See [`model_deps`] for the backend-aware accessor.
pub(super) const MODEL_DEPS: &[(&str, &str)] = &[
    ("chrono", "{ version = \"0.4\", features = [\"serde\"] }"),
    (
        "diesel",
        "{ version = \"2\", features = [\"postgres\", \"chrono\", \"uuid\"] }",
    ),
    (
        "diesel-async",
        "{ version = \"0.9\", features = [\"postgres\"] }",
    ),
    (
        "pq-sys",
        "{ version = \"0.7\", features = [\"bundled_without_openssl\"] }",
    ),
    ("diesel_migrations", "\"2\""),
    ("serde", "{ version = \"1\", features = [\"derive\"] }"),
    ("serde_json", "\"1\""),
    ("uuid", "{ version = \"1\", features = [\"serde\"] }"),
];

/// [`MODEL_DEPS`] for a `SQLite` app (issue #1924).
///
/// Same shape, different backend: diesel on its `sqlite` feature with the
/// bundled `libsqlite3-sys` amalgamation, `diesel-async` on the sync-connection
/// wrapper that `autumn-web`'s `sqlite` feature runs the pool through, and no
/// `pq-sys` — a `SQLite` app has no reason to name libpq itself. (`autumn-web`'s
/// `db` feature still pulls it in transitively, so this trims the app's direct
/// dependency list, not its link line.) `returning_clauses_for_sqlite_3_35`
/// matches `autumn-web`, so the two never disagree about `RETURNING` support.
pub(super) const MODEL_DEPS_SQLITE: &[(&str, &str)] = &[
    ("chrono", "{ version = \"0.4\", features = [\"serde\"] }"),
    (
        "diesel",
        "{ version = \"2\", features = [\"sqlite\", \"chrono\", \"serde_json\", \
         \"returning_clauses_for_sqlite_3_35\"] }",
    ),
    (
        "diesel-async",
        "{ version = \"0.9\", features = [\"sync-connection-wrapper\"] }",
    ),
    (
        "libsqlite3-sys",
        "{ version = \"0.38\", features = [\"bundled\"] }",
    ),
    ("diesel_migrations", "\"2\""),
    ("serde", "{ version = \"1\", features = [\"derive\"] }"),
    ("serde_json", "\"1\""),
    ("uuid", "{ version = \"1\", features = [\"serde\"] }"),
];

/// The direct dependencies a generated model needs on `backend` (issue #1924).
#[must_use]
pub(super) const fn model_deps(
    backend: DatabaseBackend,
) -> &'static [(&'static str, &'static str)] {
    match backend {
        DatabaseBackend::Postgres => MODEL_DEPS,
        DatabaseBackend::Sqlite => MODEL_DEPS_SQLITE,
    }
}

/// The `rust_decimal` features a `decimal{p,s}` field needs on `backend`
/// (issue #1924).
///
/// Postgres rides `rust_decimal`'s own diesel impls; `SQLite` rides
/// `autumn-web`'s `SqliteDecimal` newtype instead, and `rust_decimal` ships no
/// diesel-`SQLite` feature at all, so asking for the Postgres one would pull
/// libpq into a `SQLite` build.
#[must_use]
pub(super) const fn decimal_dep_features(backend: DatabaseBackend) -> &'static [&'static str] {
    match backend {
        DatabaseBackend::Postgres => &["db-diesel2-postgres", "serde"],
        DatabaseBackend::Sqlite => &["serde"],
    }
}

/// Append a `Modify` action to `plan` that ensures every `(crate, version_spec)`
/// in `deps` is present under `[dependencies]` in the project's `Cargo.toml`.
/// Existing entries are left untouched.
///
/// `owner_dir` is the directory this call's resource files live in (e.g.
/// `src/models`, `src/jobs`) — `autumn destroy` only removes these deps once
/// no OTHER file remains in `owner_dir`, so a sibling resource of the same
/// generator that still needs one of `deps` (e.g. a second `model` also
/// using `uuid`) survives destroying just one of them.
pub(super) fn plan_cargo_deps(
    plan: &mut Plan,
    project_root: &Path,
    deps: &[(&str, &str)],
    owner_dir: &Path,
) {
    let cargo_toml_path = project_root.join("Cargo.toml");
    let existing = read_or_empty(&cargo_toml_path);
    let updated = ensure_cargo_dependencies(&existing, deps);
    if updated != existing {
        plan.modify(cargo_toml_path.clone(), updated);
    }
    // Recorded unconditionally, mirroring every other `push_revert` call in this module,
    // so `autumn destroy` (#1048) — which recomputes this same plan against the
    // already-generated Cargo.toml, where these deps are by definition present — still
    // knows to remove them. Gating on "did the Modify actually change anything" would make
    // it a no-op at destroy time, since re-running this idempotent transform against
    // post-generate disk never produces a diff.
    //
    // `TEMPLATE_SHIPPED_CARGO_DEPS` names are excluded: `autumn new`'s template already
    // declares them (see `templates/Cargo.toml.tmpl`), so `ensure_cargo_dependencies`
    // never adds them for a real project — they are in `MODEL_DEPS`/`SCAFFOLD_EXTRA_DEPS`
    // only as a safety net for a hand-rolled Cargo.toml missing them. Reverting them
    // unconditionally would strip a framework dependency the project needs regardless of
    // any generated resource.
    //
    // Known limitation, out of scope per #1048: for any other name, if a different
    // resource's generator also depends on it, destroying this resource still removes it.
    // Reverting a shared dependency across several generated resources needs the
    // multi-step undo history the issue explicitly scopes out.
    let names: Vec<String> = deps
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !TEMPLATE_SHIPPED_CARGO_DEPS.contains(name))
        .map(str::to_owned)
        .collect();
    if !names.is_empty() {
        plan.push_revert(crate::generate::emit::Revert::CargoDeps {
            path: cargo_toml_path,
            names,
            owner_dir: owner_dir.to_path_buf(),
        });
    }
}

/// Crate dependencies `autumn new`'s own template already declares (see
/// `templates/Cargo.toml.tmpl`) that also happen to appear in
/// [`MODEL_DEPS`]/[`super::scaffold::SCAFFOLD_EXTRA_DEPS`] as a safety net
/// for a hand-rolled project missing them. [`plan_cargo_deps`] never
/// includes these in a `Revert::CargoDeps`, since a real project needs them
/// regardless of whether any resource was ever generated.
pub(super) const TEMPLATE_SHIPPED_CARGO_DEPS: &[&str] =
    &["autumn-web", "maud", "diesel_migrations"];

/// Insert each `(crate, version_spec)` pair at the end of the `[dependencies]`
/// section, skipping entries already present. Pure string transformation —
/// preserves the rest of the file as-is. If the file has no `[dependencies]`
/// section yet, appends a new one with the requested entries.
pub fn ensure_cargo_dependencies(existing: &str, deps: &[(&str, &str)]) -> String {
    let lines: Vec<&str> = existing.lines().collect();

    // Locate the `[dependencies]` table header. Tolerate trailing whitespace
    // and `# comments` after the header (`[dependencies] # shared deps`).
    let Some(deps_idx) = lines
        .iter()
        .position(|l| is_table_header(l, "dependencies"))
    else {
        // No `[dependencies]` section yet — append one with all requested deps.
        use std::fmt::Write as _;
        let mut out = String::with_capacity(existing.len() + 64);
        out.push_str(existing);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str("[dependencies]\n");
        for (name, spec) in deps {
            let _ = writeln!(out, "{name} = {spec}");
        }
        return out;
    };

    // Two concerns are split here:
    // 1. The scan extent — how far the dependency section reaches when deciding which
    //    deps are already declared. `[dependencies.<crate>]` subtables are part of
    //    `[dependencies]`, so they extend the scan until a real boundary such as
    //    `[dev-dependencies]` or `[[bin]]`.
    // 2. The insertion point — where to write new shorthand `key = value` entries. This
    //    stops at the first table header, subtable or not, because TOML attaches
    //    shorthand keys to whichever section header precedes them: a `chrono = "0.4"`
    //    placed after a `[dependencies.chrono]` line would become a key inside that
    //    subtable rather than a sibling shorthand dep.
    let scan_end = lines[deps_idx + 1..]
        .iter()
        .position(|l| is_any_table_header(l) && !is_dep_subtable_boundary_marker(l))
        .map_or(lines.len(), |off| deps_idx + 1 + off);
    let insert_end = lines[deps_idx + 1..]
        .iter()
        .position(|l| is_any_table_header(l))
        .map_or(lines.len(), |off| deps_idx + 1 + off);

    let dep_section = &lines[deps_idx + 1..scan_end];

    let to_add: Vec<(&str, &str)> = deps
        .iter()
        .copied()
        .filter(|(name, _)| !dep_section_has(dep_section, name))
        .collect();
    if to_add.is_empty() {
        return existing.to_owned();
    }

    // Drop trailing blank lines from the shorthand block so the insertion sits
    // flush against the existing entries.
    let mut insert_at = insert_end;
    while insert_at > deps_idx + 1 && lines[insert_at - 1].trim().is_empty() {
        insert_at -= 1;
    }

    let inserted: Vec<String> = to_add
        .iter()
        .map(|(name, spec)| format!("{name} = {spec}"))
        .collect();

    let mut out = String::with_capacity(
        existing.len() + inserted.iter().map(String::len).sum::<usize>() + 16,
    );
    for line in &lines[..insert_at] {
        out.push_str(line);
        out.push('\n');
    }
    for entry in &inserted {
        out.push_str(entry);
        out.push('\n');
    }
    for line in &lines[insert_at..] {
        out.push_str(line);
        out.push('\n');
    }
    // Preserve whether the original file ended with a newline.
    if !existing.ends_with('\n') {
        out.pop();
    }
    out
}

/// Inverse of [`ensure_cargo_dependencies`] (`autumn destroy`, issue #1048).
///
/// Removes the exact `<name> = <spec>` shorthand line for each crate in
/// `names` from `[dependencies]`, leaving every other entry (and the rest of
/// the file) byte-for-byte intact. A no-op for any crate not present in that
/// exact shorthand form — already destroyed, hand-edited into a subtable, or
/// never added by this generator — destroy only reverses lines `generate`
/// itself would have written.
#[must_use]
/// Two callers now: `autumn destroy` (which only ever reverses lines
/// `generate` itself wrote) and `autumn plugin remove` (issue #1631), which
/// operates on a manifest the user owns. The second is why every caller must
/// verify the result — see `plugin::remove::dependency_cleanly_removed`: a
/// dependency written as a multi-line inline table or a
/// `[dependencies.<crate>]` subtable is NOT rewritten correctly by this
/// line-based pass, and writing its output unchecked would leave a `Cargo.toml`
/// Cargo cannot parse.
pub fn remove_cargo_dependencies(existing: &str, names: &[&str]) -> String {
    let mut lines: Vec<&str> = existing.lines().collect();
    let Some(deps_idx) = lines
        .iter()
        .position(|l| is_table_header(l, "dependencies"))
    else {
        return existing.to_owned();
    };
    let mut scan_end = lines[deps_idx + 1..]
        .iter()
        .position(|l| is_any_table_header(l) && !is_dep_subtable_boundary_marker(l))
        .map_or(lines.len(), |off| deps_idx + 1 + off);

    let mut removed_any = false;
    let mut i = deps_idx + 1;
    while i < scan_end {
        let trimmed = lines[i].trim_start();
        let is_target = names.iter().any(|name| {
            trimmed
                .strip_prefix(name)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        });
        if is_target {
            lines.remove(i);
            scan_end -= 1;
            removed_any = true;
            continue;
        }
        i += 1;
    }
    if !removed_any {
        return existing.to_owned();
    }
    // If removing these deps emptied the whole `[dependencies]` section
    // (allowing for a residual blank line — e.g. the separator a later
    // `[dev-dependencies]` insertion added right after the last real entry),
    // drop the header, every blank line in its now-empty body, and the
    // blank separator line before it, if any — restoring the file to what
    // it looked like before `ensure_cargo_dependencies` ever created this
    // section from scratch.
    if lines[deps_idx + 1..scan_end]
        .iter()
        .all(|l| l.trim().is_empty())
    {
        lines.drain(deps_idx..scan_end);
        if deps_idx > 0 && lines[deps_idx - 1].trim().is_empty() {
            lines.remove(deps_idx - 1);
        }
    }
    let mut out = lines.join("\n");
    if existing.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

fn is_table_header(line: &str, table: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix('[') else {
        return false;
    };
    let Some(close_idx) = rest.find(']') else {
        return false;
    };
    if rest[..close_idx].trim() != table {
        return false;
    }
    // Anything after `]` must be whitespace or a `#` comment.
    let after = rest[close_idx + 1..].trim_start();
    after.is_empty() || after.starts_with('#')
}

/// True iff `line` is *any* TOML table header — either a single-bracket
/// `[section]` or an array-of-tables `[[section]]`. Both must terminate the
/// `[dependencies]` table when scanning forward.
fn is_any_table_header(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Strip one or two opening brackets — `[[…]]` is the array-of-tables form.
    let after_open = trimmed
        .strip_prefix("[[")
        .or_else(|| trimmed.strip_prefix('['));
    let Some(rest) = after_open else {
        return false;
    };
    // Find the *first* closing bracket. Whether it's `]` or `]]`, the inner
    // name is everything before that first `]`.
    let Some(close_idx) = rest.find(']') else {
        return false;
    };
    if rest[..close_idx].trim().is_empty() {
        return false;
    }
    // Anything after the closing bracket(s) must be whitespace or `# comment`.
    let after = rest[close_idx + 1..].trim_start();
    let after = after.strip_prefix(']').unwrap_or(after).trim_start();
    after.is_empty() || after.starts_with('#')
}

/// If `line` is a `[dependencies.<crate>]` subtable header, return the inner
/// crate name. Such headers declare a table-form dependency and are part of
/// the dependency section, not a boundary that ends it.
fn dep_subtable_crate_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('[')?;
    let close_idx = rest.find(']')?;
    let after = rest[close_idx + 1..].trim_start();
    if !after.is_empty() && !after.starts_with('#') {
        return None;
    }
    let inner = rest[..close_idx].trim();
    let dep_name = inner.strip_prefix("dependencies")?.trim_start();
    let dep_name = dep_name.strip_prefix('.')?.trim_start();
    if dep_name.is_empty() {
        return None;
    }
    Some(dep_name)
}

fn is_dep_subtable_boundary_marker(line: &str) -> bool {
    dep_subtable_crate_name(line).is_some()
}

/// True iff `dep_section` contains a line declaring `crate_name = …`, or a
/// `[dependencies.<crate_name>]` subtable header.
fn dep_section_has(dep_section: &[&str], crate_name: &str) -> bool {
    dep_section.iter().any(|l| {
        let t = l.trim_start();
        // Strip leading `#` so commented-out lines don't count.
        if t.starts_with('#') {
            return false;
        }
        // `[dependencies.<crate>]` subtable form.
        if let Some(name) = dep_subtable_crate_name(l) {
            return name == crate_name;
        }
        // `crate = …` shorthand form.
        t.split_once('=')
            .is_some_and(|(name, _)| name.trim() == crate_name)
    })
}

/// `true` if `existing` Cargo.toml text already declares `crate_name` as a
/// dependency, but that existing declaration doesn't (as far as this
/// string-only check can tell) already list `feature`.
///
/// [`ensure_cargo_dependencies`] skips any crate name it finds already
/// present — regardless of that entry's version or features — so a project
/// that added `crate_name` for its own reasons (or inherited it from a
/// workspace) before ever running a generator that needs `feature` would
/// otherwise silently get code that fails to compile with no indication why
/// (PR review, issue #1038: `rust_decimal = "1"` without
/// `db-diesel2-postgres` lacks the Diesel `ToSql`/`FromSql` impls a
/// generated `decimal` field's `#[model]` struct needs).
///
/// Checks the common single-line shorthand form (`crate = "1"` or
/// `crate = { version = "1", features = [...] }`) and the
/// `[dependencies.crate]` subtable form. Doesn't attempt to resolve
/// `{ workspace = true }` inheritance or a `features` array split across
/// multiple lines — those are rarer shapes this heuristic can't see through
/// from Cargo.toml text alone, so it may occasionally warn when the feature
/// is in fact present; a false-positive warning is far cheaper than the
/// silent compile failure this check exists to catch.
fn existing_dep_declared_without_feature(existing: &str, crate_name: &str, feature: &str) -> bool {
    let lines: Vec<&str> = existing.lines().collect();
    let Some(deps_idx) = lines
        .iter()
        .position(|l| is_table_header(l, "dependencies"))
    else {
        return false;
    };
    let scan_end = lines[deps_idx + 1..]
        .iter()
        .position(|l| is_any_table_header(l) && !is_dep_subtable_boundary_marker(l))
        .map_or(lines.len(), |off| deps_idx + 1 + off);
    let dep_section = &lines[deps_idx + 1..scan_end];

    if let Some(sub_idx) = dep_section
        .iter()
        .position(|l| dep_subtable_crate_name(l) == Some(crate_name))
    {
        let sub_body_end = dep_section[sub_idx + 1..]
            .iter()
            .position(|l| is_any_table_header(l))
            .map_or(dep_section.len(), |off| sub_idx + 1 + off);
        let sub_body = dep_section[sub_idx + 1..sub_body_end].join("\n");
        return !sub_body.contains(feature);
    }

    dep_section
        .iter()
        .find(|l| {
            let t = l.trim_start();
            !t.starts_with('#')
                && t.split_once('=')
                    .is_some_and(|(name, _)| name.trim() == crate_name)
        })
        .is_some_and(|line| !line.contains(feature))
}

/// If `existing` Cargo.toml text already declares `crate_name` without one
/// or more of `features`, record a single `plan` warning naming exactly
/// which ones to add by hand. See [`existing_dep_declared_without_feature`]
/// for why this check exists.
///
/// Checked against the *full* feature set a generator needs — not just one
/// of them — because `ensure_cargo_dependencies` only ever gets one shot at
/// a crate name: once it's present, no generator will ever revisit it again.
/// A partial check (e.g. only `db-diesel2-postgres`, missing `serde`) would
/// pass a project that already added `rust_decimal` with *some* but not all
/// of the required features, still leaving the generated code unable to
/// compile.
pub(super) fn warn_if_existing_dep_missing_features(
    plan: &mut Plan,
    existing_cargo_toml: &str,
    crate_name: &str,
    features: &[&str],
) {
    let missing: Vec<&str> = features
        .iter()
        .copied()
        .filter(|feature| {
            existing_dep_declared_without_feature(existing_cargo_toml, crate_name, feature)
        })
        .collect();
    if !missing.is_empty() {
        let feature_list = missing
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ");
        plan.warn(format!(
            "Cargo.toml already declares '{crate_name}' without the generated code's \
             required feature(s) — add {feature_list} to its `features` list by hand \
             or the generated code may fail to compile."
        ));
    }
}

/// Pull the version literal out of the right-hand side of a single-line
/// dependency declaration, whether shorthand (`"0.13"`) or the inline-table
/// `{ version = "0.13", ... }` form. Returns `None` when there is no version
/// literal to read (`{ workspace = true }`, git/path deps, etc.).
fn extract_version_literal(rhs: &str) -> Option<&str> {
    let rhs = rhs.trim();
    let after = if let Some(idx) = rhs.find("version") {
        // Inline-table form: skip past `version`, then read the literal.
        &rhs[idx + "version".len()..]
    } else if rhs.starts_with('"') {
        // Shorthand form: the literal starts here.
        rhs
    } else {
        return None;
    };
    let start = after.find('"')? + 1;
    let end = after[start..].find('"')? + start;
    Some(&after[start..end])
}

/// Parse the leading `major.minor` out of a version-requirement literal such
/// as `"0.13"`, `"^0.21"`, `"~0.22.1"`, or `"1"`. A missing minor defaults to
/// `0`. Returns `None` only when the major component itself can't be parsed.
fn parse_major_minor(ver: &str) -> Option<(u64, u64)> {
    // Trim a leading caret/tilde/comparator so `^0.13`, `~0.13`, `>=0.13`
    // parse to the same base version.
    let ver = ver.trim_start_matches(['^', '~', '=', '>', '<', ' ']);
    let mut it = ver.split('.');
    let major = it.next()?.trim().parse::<u64>().ok()?;
    let minor = it
        .next()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Some((major, minor))
}

/// Best-effort: return the `(major, minor)` version `existing` Cargo.toml pins
/// for `crate_name` in `[dependencies]`, or `None` when the crate is absent or
/// its version can't be determined from the text alone.
///
/// Mirrors [`dep_section_has`]'s shape handling so it sees the same declarations
/// `ensure_cargo_dependencies` treats as "already present":
/// - single-line shorthand / inline-table (`crate = "0.13"`,
///   `crate = { version = "0.13", ... }`), and
/// - the `[dependencies.<crate>]` subtable form, scanning its body (up to the
///   next table header) for a `version = "…"` key.
///
/// Returns `None` for shapes with no readable version literal — workspace
/// inheritance (`{ workspace = true }`), git/path deps, or a subtable with no
/// `version` key. Callers treat that undeterminable case conservatively.
fn existing_dep_version(existing: &str, crate_name: &str) -> Option<(u64, u64)> {
    let lines: Vec<&str> = existing.lines().collect();
    let deps_idx = lines
        .iter()
        .position(|l| is_table_header(l, "dependencies"))?;
    let scan_end = lines[deps_idx + 1..]
        .iter()
        .position(|l| is_any_table_header(l) && !is_dep_subtable_boundary_marker(l))
        .map_or(lines.len(), |off| deps_idx + 1 + off);
    let dep_section = &lines[deps_idx + 1..scan_end];

    // `[dependencies.<crate>]` subtable form: scan its body for a `version` key.
    if let Some(sub_idx) = dep_section
        .iter()
        .position(|l| dep_subtable_crate_name(l) == Some(crate_name))
    {
        let sub_body_end = dep_section[sub_idx + 1..]
            .iter()
            .position(|l| is_any_table_header(l))
            .map_or(dep_section.len(), |off| sub_idx + 1 + off);
        for l in &dep_section[sub_idx + 1..sub_body_end] {
            let t = l.trim_start();
            if t.starts_with('#') {
                continue;
            }
            if let Some((key, rhs)) = t.split_once('=')
                && key.trim() == "version"
            {
                return extract_version_literal(rhs).and_then(parse_major_minor);
            }
        }
        // Subtable present but no `version` key → undeterminable.
        return None;
    }

    // `crate = …` single-line shorthand / inline-table form.
    for l in dep_section {
        let t = l.trim_start();
        if t.starts_with('#') {
            continue;
        }
        let Some((name, rhs)) = t.split_once('=') else {
            continue;
        };
        if name.trim() != crate_name {
            continue;
        }
        return extract_version_literal(rhs).and_then(parse_major_minor);
    }
    None
}

/// Best-effort: return `true` if the generator should warn that `existing`
/// Cargo.toml's `crate_name` pin might be too old — i.e. `crate_name` is
/// declared (in *any* shape [`dep_section_has`] recognises) but is *not*
/// provably at version `>= (min_major, min_minor)`.
///
/// [`ensure_cargo_dependencies`] is name-only: once a crate is declared it
/// never bumps the version, so a stale pin silently yields generated code that
/// won't compile. Because that skip fires for both the shorthand and the
/// `[dependencies.<crate>]` subtable shapes, the version check must see both —
/// otherwise a subtable pin of `base64 = "0.13"` would dodge the warning while
/// still breaking the build.
///
/// Decision:
/// - crate absent → `false` (it'll be added fresh at the required version);
/// - version provably `>= (min_major, min_minor)` → `false`;
/// - version provably below → `true`;
/// - version undeterminable but the crate is declared → `true`, conservatively
///   (a spurious warning is far cheaper than a silent compile break).
fn existing_dep_version_below(
    existing: &str,
    crate_name: &str,
    min_major: u64,
    min_minor: u64,
) -> bool {
    let lines: Vec<&str> = existing.lines().collect();
    let Some(deps_idx) = lines
        .iter()
        .position(|l| is_table_header(l, "dependencies"))
    else {
        return false;
    };
    let scan_end = lines[deps_idx + 1..]
        .iter()
        .position(|l| is_any_table_header(l) && !is_dep_subtable_boundary_marker(l))
        .map_or(lines.len(), |off| deps_idx + 1 + off);
    let dep_section = &lines[deps_idx + 1..scan_end];

    // Absent (in every shape ensure_cargo_dependencies would skip) → no warn.
    if !dep_section_has(dep_section, crate_name) {
        return false;
    }
    // Declared: warn unless we can prove the pinned version is new enough.
    existing_dep_version(existing, crate_name)
        .is_none_or(|version| version < (min_major, min_minor))
}

/// If `existing` Cargo.toml already declares `crate_name` at a version below
/// `(min_major, min_minor)`, record a single `plan` warning. Used for deps
/// whose generated code needs a newer API than an older pinned version
/// exposes (e.g. `base64` 0.21+ `Engine`/`engine::general_purpose`), which
/// [`ensure_cargo_dependencies`] silently skips because it is name-only. See
/// [`warn_if_existing_dep_missing_features`] for the sibling case.
pub(super) fn warn_if_existing_dep_below_version(
    plan: &mut Plan,
    existing_cargo_toml: &str,
    crate_name: &str,
    min_major: u64,
    min_minor: u64,
) {
    if existing_dep_version_below(existing_cargo_toml, crate_name, min_major, min_minor) {
        plan.warn(format!(
            "Cargo.toml already declares '{crate_name}', but not provably at \
             version >= {min_major}.{min_minor} — the generated passkeys/2FA \
             handlers use the '{crate_name}' 0.21+ `Engine`/\
             `engine::general_purpose` API; make sure '{crate_name}' >= \
             {min_major}.{min_minor} by hand or the generated code may fail to \
             compile."
        ));
    }
}

/// Reserved resource names whose snake-case form would collide with a special
/// file in the generated layout (e.g. `mod` → `src/models/mod.rs`).
const RESERVED_RESOURCE_NAMES: &[&str] = &["main", "lib"];

/// Validate a resource name is a non-empty `PascalCase` or `snake_case` identifier.
pub(super) fn validate_resource_name(name: &str) -> Result<(), GenerateError> {
    if name.is_empty() {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            "name cannot be empty".into(),
        ));
    }
    let first = name.chars().next().expect("non-empty");
    if !first.is_ascii_alphabetic() {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            "must start with a letter".into(),
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_')
    {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            format!("contains invalid character '{bad}'"),
        ));
    }
    let snake_name = super::naming::snake(name);
    // Snake-case form is used as a module name (`pub mod <snake_name>;`) and as
    // a `crate::models::<snake_name>::…` import path. Rust keywords like `type`,
    // `match`, and `mod` would emit syntactically invalid code.
    if super::dsl::is_rust_keyword(&snake_name) {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            format!(
                "'{name}' is a Rust keyword (its snake_case form '{snake_name}' cannot be a module name)"
            ),
        ));
    }
    if RESERVED_RESOURCE_NAMES.contains(&snake_name.as_str()) {
        return Err(GenerateError::InvalidName(
            name.to_owned(),
            format!(
                "'{name}' is reserved — its snake_case form ('{snake_name}') collides with a special file"
            ),
        ));
    }
    Ok(())
}

/// Field names the model template emits unconditionally (`id` and
/// `created_at`). User-provided fields with these names would produce
/// duplicate struct members and duplicate columns in the migration.
const RESERVED_FIELD_NAMES: &[&str] = &["id", "created_at"];

/// Reject user fields whose name collides with a column the template always
/// emits.
fn validate_field_names(fields: &[Field]) -> Result<(), GenerateError> {
    for f in fields {
        if RESERVED_FIELD_NAMES.contains(&f.name.as_str()) {
            return Err(GenerateError::InvalidField {
                token: format!("{}:{}", f.name, f.rust_type()),
                reason: format!(
                    "'{}' is reserved — the generator always emits this column",
                    f.name
                ),
            });
        }
    }
    Ok(())
}

/// `std`/prelude type names a generated enum's own impl block (see
/// `render_enum_decl`) and the surrounding generated model/scaffold code rely
/// on being their real, unqualified meaning — `String::new()`, `s.parse()`
/// returning a real `std::string::String`, `Vec<u8>` Bytea handling, etc. A
/// field that pascalizes to one of these (e.g. `string:enum{a,b}` ->
/// `pub enum String`) shadows the prelude import for the *entire file* it's
/// declared in (and, once imported via `use crate::models::…::{…, String}`,
/// for the entire scaffold routes file too), breaking unrelated code far
/// from this DSL token — confirmed by generating `string:enum{a,b}` and
/// observing `cargo check` fail with E0308/E0599 throughout the model and
/// routes files.
const PRELUDE_TYPE_NAMES: &[&str] = &["String", "Vec", "Option", "Result", "Box", "Into"];

/// Reject an `enum{…}` field whose generated Rust type name (`pascal(field)`)
/// collides with the model struct itself, one of the companion types the
/// `#[model]`/`#[repository]` macros and the scaffold generator emit from the
/// same resource name, another enum field's own generated type name, or a
/// commonly-relied-upon `std`/prelude type (see [`PRELUDE_TYPE_NAMES`]).
///
/// The companion-type list is `New{Pascal}`, `Update{Pascal}`, `{Pascal}Field`,
/// `{Pascal}DraftExt`, `{Pascal}Preload`, `{Pascal}Associations`,
/// `{Pascal}Factory` (all from `#[model]`; the preload/associations/factory
/// scaffolding is always emitted, even for a model with no associations —
/// see `autumn-macros/src/model.rs`), `{Pascal}Repository`/
/// `Pg{Pascal}Repository` (from `#[repository]`, which `generate scaffold`
/// always adds on top of the model), and `DecodedForm` (the scaffold's form
/// struct). A silent collision would produce a duplicate-type-definition
/// compile error far from this DSL token, so it's rejected here with a
/// pointer back to the offending field.
///
/// Two enum fields on the *same* model can also collide with each other:
/// `pascal()` is not injective (`in_review` and `in__review` both pascalize
/// to `InReview`), so distinct, individually-valid field names can still
/// generate the same enum type name. Each enum field's generated name is
/// checked against every earlier one for exactly this reason.
fn validate_enum_field_collisions(
    pascal_name: &str,
    fields: &[Field],
) -> Result<(), GenerateError> {
    let mut seen_enum_types: Vec<(String, String)> = Vec::new();
    for f in fields {
        let Some(enum_ty) = f.enum_type_name() else {
            continue;
        };
        if PRELUDE_TYPE_NAMES.contains(&enum_ty.as_str()) {
            return Err(GenerateError::InvalidField {
                token: format!("{}:enum{{...}}", f.name),
                reason: format!(
                    "the generated enum type '{enum_ty}' would shadow the standard library's \
                     `{enum_ty}` for the rest of the file; rename the field"
                ),
            });
        }
        let reserved = [
            pascal_name.to_owned(),
            format!("New{pascal_name}"),
            format!("Update{pascal_name}"),
            format!("{pascal_name}Field"),
            format!("{pascal_name}DraftExt"),
            format!("{pascal_name}Preload"),
            format!("{pascal_name}Associations"),
            format!("{pascal_name}Factory"),
            format!("{pascal_name}Repository"),
            format!("Pg{pascal_name}Repository"),
            "DecodedForm".to_owned(),
        ];
        if reserved.contains(&enum_ty) {
            return Err(GenerateError::InvalidField {
                token: format!("{}:enum{{...}}", f.name),
                reason: format!(
                    "the generated enum type '{enum_ty}' collides with a type the generator \
                     already emits for '{pascal_name}'; rename the field"
                ),
            });
        }
        if let Some((other_name, _)) = seen_enum_types.iter().find(|(_, ty)| *ty == enum_ty) {
            return Err(GenerateError::InvalidField {
                token: format!("{}:enum{{...}}", f.name),
                reason: format!(
                    "the generated enum type '{enum_ty}' collides with the one generated for \
                     field '{other_name}'; rename one of the fields"
                ),
            });
        }
        seen_enum_types.push((f.name.clone(), enum_ty));
    }
    Ok(())
}

// Retained as a Postgres-default convenience wrapper for the test suite; the
// backend-aware `parse_model_metadata_for` is what production calls. Not doc
// linked from it: this is `#[cfg(test)]`, so a link would break the doc build.
#[cfg(test)]
pub fn parse_model_metadata(
    fields: &[Field],
    options: &ModelOptions,
) -> Result<ModelMetadata, GenerateError> {
    parse_model_metadata_for(DatabaseBackend::Postgres, fields, options)
}

/// Fold every metadata-bearing `--flag` (`--index`, `--validate`, `--default`,
/// `--searchable`, …) into a [`ModelMetadata`], validated against `fields`.
///
/// `backend` reaches only `--default` rendering (issue #1924): a `decimal`
/// default is an unquoted numeric literal on Postgres and a quoted, normalized
/// text literal on `SQLite`. See [`sql_default_literal`].
///
/// # Errors
/// Returns [`GenerateError::InvalidField`] for a flag naming an unknown field,
/// or carrying a value the field's kind cannot take.
#[allow(
    clippy::too_many_lines,
    reason = "one validation pass per `--flag` that contributes model metadata; \
              splitting it would scatter the shared `metadata` accumulator"
)]
pub fn parse_model_metadata_for(
    backend: DatabaseBackend,
    fields: &[Field],
    options: &ModelOptions,
) -> Result<ModelMetadata, GenerateError> {
    let mut metadata = ModelMetadata::default();

    for index in &options.indexes {
        let field_name = index.trim();
        validate_known_field(fields, field_name, index)?;
        metadata.indexes.insert(field_name.to_owned());
    }

    for validation in &options.validations {
        let (field_name, rule) = split_key_value(validation, '=')?;
        let field =
            field_by_name(fields, field_name).ok_or_else(|| GenerateError::InvalidField {
                token: validation.clone(),
                reason: format!("unknown field '{field_name}'"),
            })?;
        let attr =
            render_validation_attr(field, rule).map_err(|reason| GenerateError::InvalidField {
                token: validation.clone(),
                reason,
            })?;
        metadata
            .validations
            .entry(field_name.to_owned())
            .or_default()
            .push(attr);
    }

    // DSL brace-constraint modifiers (`title:String{min=3,max=120}`,
    // `contact:String{email}`, `age:i32{min=0,max=130}`) fan out into the
    // same `#[validate(...)]` pipeline as the `--validate` flag (issue #1388),
    // so the generated model rejects invalid input server-side through the
    // existing `Validated`/changeset path (422 + per-field errors) rather than
    // a 500 or a silent store. Deduped against any `--validate` rule already
    // recorded for the field, so declaring a constraint both ways doesn't emit
    // it twice. `has_validator_rules()` then pulls in the `validator` crate
    // dependency automatically (see `plan_model_with_options`).
    for field in fields {
        if field.constraints.is_empty() {
            continue;
        }
        for attr in field.validation_attrs() {
            let entry = metadata.validations.entry(field.name.clone()).or_default();
            if !entry.contains(&attr) {
                entry.push(attr);
            }
        }
    }

    for default in &options.defaults {
        let (field_name, value) = split_key_value(default, '=')?;
        let field =
            field_by_name(fields, field_name).ok_or_else(|| GenerateError::InvalidField {
                token: default.clone(),
                reason: format!("unknown field '{field_name}'"),
            })?;
        // `unique` + `--default` is rejected outright (issue #1032 review
        // follow-up) rather than half-supported: a scaffold's `--default`
        // fields are excluded from the generated HTML form (see
        // `scaffold::plan_scaffold`'s `form_fields` filter), so a defaulted
        // `unique` column would have no input to show a duplicate-value
        // error against even if `UNIQUE_CONSTRAINTS` did list it. Worse, a
        // *constant* default value collides with itself on every insert
        // after the first, so the combination rarely means what it looks
        // like it means.
        if field.unique {
            return Err(GenerateError::InvalidField {
                token: default.clone(),
                reason: format!(
                    "field '{field_name}' cannot be both `unique` and have a `--default` \
                     value — a defaulted unique column either only supports one row ever \
                     (a constant default collides with itself on every later insert) or \
                     has no form control to show a duplicate-value error against (the \
                     generated form omits defaulted fields). Remove one of the two."
                ),
            });
        }
        let sql = sql_default_literal(field, value, backend).map_err(|reason| {
            GenerateError::InvalidField {
                token: default.clone(),
                reason,
            }
        })?;
        metadata.defaults.insert(field_name.to_owned(), sql);
    }

    // Issue #1318: a `lock_version` column opts the model into the framework's
    // optimistic-locking primitive. `#[lock_version]` makes the column
    // DB-managed — it is excluded from `New{Model}`, so the INSERT never names
    // it and the SQL column needs a `DEFAULT` or every create would fail the
    // NOT NULL constraint. Recording it as a default here also drops the column
    // from the scaffold's generated HTML form (`plan_scaffold`'s `form_fields`
    // filter): the version is machinery the handler carries in a hidden field,
    // not content the author edits. An explicit `--default lock_version=<n>`
    // wins, so a project seeding versions from a non-zero base keeps its value.
    // NOT validated here: `validate_lock_version_field` is a *generation* policy,
    // and this function also runs while planning a `destroy`, where refusing a
    // legacy column would strand files the user is trying to remove. The
    // planning entry points call it themselves when they are generating.
    if fields.iter().any(is_lock_version_column) {
        metadata
            .defaults
            .entry(LOCK_VERSION_COLUMN.to_owned())
            .or_insert_with(|| "0".to_owned());
    }

    // Issue #1358: a `position` column is likewise DB-managed and excluded
    // from `New{Model}`/`Update{Model}` (`#[position]`), so the SQL column
    // needs a `DEFAULT` too, or every create would fail the NOT NULL
    // constraint before the repository's insert hook ever runs. `DEFAULT 0`
    // is a placeholder only — the generated repository's insert hook
    // overwrites it with the real next-in-scope value inside the same
    // transaction as the insert (see `autumn-macros`' `position_after_insert`
    // splice), the same two-step "DB default, then app-managed overwrite"
    // shape `lock_version` uses above. Recording it here also drops the
    // column from the scaffold's generated HTML form, same as `lock_version`.
    //
    // Issue #1367: a `commentable` counter column is the same shape — DB
    // managed, `#[default]` on the model, `DEFAULT 0` in SQL — except that the
    // overwrite comes from the framework's comment write path rather than an
    // insert hook. `NOT NULL DEFAULT 0` is load-bearing rather than tidy: the
    // maintenance is `SET c = c + 1`, and `NULL + 1` is `NULL`.
    for f in fields {
        if f.kind.is_server_managed() {
            metadata
                .defaults
                .entry(f.name.clone())
                .or_insert_with(|| "0".to_owned());
        }
    }

    // Full-text search's generated `search_page` (in the repository macro)
    // hardcodes an `i64`/`BigInt` primary key: it collects `SearchId { id: i64 }`
    // rows into a `Vec<i64>`, filters with `id.eq_any(&ids)`, and dedups through
    // a `HashMap<i64, _>`. A non-`i64` primary key (e.g. `--id uuid`) would make
    // those `id` operations type-mismatch, so the generated repository would fail
    // to compile. Reject the combination up front — before any files are written
    // — rather than emitting broken code.
    if !options.searchable.is_empty() && options.id_type != IdType::BigSerial {
        return Err(GenerateError::Config(format!(
            "`--searchable` requires an i64 (bigint) primary key; full-text search does not \
             yet support `{}` ids (the repository's `search_page` is hardcoded to `i64`). \
             Re-run without `--searchable`, or use the default `--id bigint` primary key.",
            options.id_type.rust_type()
        )));
    }

    // `search_vector` is the generated FTS column name, hardcoded by the
    // repository macro (`ADD COLUMN search_vector tsvector` in the appended
    // migration, and the `search_vector @@ …` queries in `search_page`). If the
    // model also declares its own field named `search_vector`, the create-table
    // SQL emits that column first and the FTS migration then fails to add it
    // (duplicate column) at `autumn migrate`. Reserve the name for searchable
    // models and reject up front (case-insensitive; only relevant with
    // `--searchable` — a `search_vector` field is harmless otherwise).
    if !options.searchable.is_empty()
        && let Some(field) = fields
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case("search_vector"))
    {
        return Err(GenerateError::Config(format!(
            "`{}` is a reserved column name for `--searchable` models: `search_vector` is \
             the generated tsvector column the FTS migration adds, so a model field of the \
             same name collides with it (duplicate column at `autumn migrate`). Rename the \
             field, or drop `--searchable`.",
            field.name
        )));
    }

    // Full-text search config (issue #1319). Only text (`String`/`Text`) fields
    // can populate a `tsvector`; a non-text field would emit a model that fails
    // to compile against the `#[searchable]` macro and a migration Postgres
    // rejects, so reject it here with an actionable, field-naming error (AC5).
    // Weights follow the `search_page`/wiki convention: the first field gets the
    // highest `A` weight, the rest `B`/`C`/`D` (capped), in the order given.
    for (i, name) in options.searchable.iter().enumerate() {
        let field_name = name.trim();
        let field = field_by_name(fields, field_name).ok_or_else(|| {
            GenerateError::Config(format!(
                "--searchable names '{field_name}', which is not a field of this model. \
                 Only declared text fields can be full-text searchable."
            ))
        })?;
        if !is_string_like(field) {
            return Err(GenerateError::Config(format!(
                "--searchable field '{field_name}' is `{}`, but only text fields \
                 (`String`/`Text`) can be full-text searchable. Remove it from \
                 --searchable (numbers, bools, dates, and references are not text).",
                field.rust_type()
            )));
        }
        // NOTE: the `--searchable` + `{encrypted}` refusal deliberately lives in
        // `validate_encrypted_fields`, not here — this function also runs while
        // planning a `destroy`, where a generation-only refusal would strand the
        // files the user is trying to remove (see this function's contract
        // above, and `plan_model_with_options_for_revert`).
        let weight = b"ABCD"[i.min(3)] as char;
        metadata.searchable.push((field_name.to_owned(), weight));
    }
    if !metadata.searchable.is_empty() {
        // `english` matches the `wiki` example and is the most common default;
        // the FTS dictionary is otherwise out of scope for this slice.
        metadata.search_language = Some("english".to_owned());
    }

    Ok(metadata)
}

/// Reject every `{translatable}` combination the `#[model]` macro refuses,
/// expressed through a *flag* rather than a `{…}` modifier (issue #1384).
///
/// `parse_field` already rejects the modifier spellings (`:unique`, a nullable
/// column, `{encrypted}`, `:states(…)`, the `#[validate]` fan-out). The flags
/// below are folded in **after** parsing — `--unique` by `apply_unique_flags`,
/// the others straight from `options` — so without this pass they slip through
/// and produce either a UNIQUE index over a JSON container (silently useless)
/// or a generated project that does not compile.
///
/// Generation-only: `autumn destroy` recomputes the same plan and must not be
/// blocked by a refusal that only makes sense when emitting.
///
/// # Errors
/// Returns [`GenerateError::Config`] naming the field, the offending flag, and
/// why the combination cannot work.
pub fn validate_translatable_fields(
    fields: &[Field],
    options: &ModelOptions,
) -> Result<(), GenerateError> {
    for field in fields.iter().filter(|f| f.is_translatable()) {
        let name = &field.name;
        if field.unique {
            return Err(GenerateError::Config(format!(
                "field '{name}' is `{{translatable}}` and cannot be `--unique`: the index would                  compare whole per-locale containers, so identical text translated into                  different locale sets would never collide, and the derived `find_by_{name}`                  lookup could never match. Put the uniqueness on a non-translatable column                  (e.g. a `slug`)."
            )));
        }
        if options.indexes.iter().any(|i| i.trim() == *name) {
            return Err(GenerateError::Config(format!(
                "field '{name}' is `{{translatable}}` and cannot be `--index`ed: an equality                  index over a JSON container matches whole containers, never a single locale's                  value (the `#[model]` macro refuses `#[indexed]` + `#[translatable]` for the                  same reason). Drop it from `--index`."
            )));
        }
        if options.searchable.iter().any(|s| s.trim() == *name) {
            return Err(GenerateError::Config(format!(
                "field '{name}' is `{{translatable}}` and cannot be `--searchable`: full-text                  search indexes the stored column, which is a JSON container — the index would                  match locale tags and JSON punctuation, not the prose (the `#[model]` macro                  refuses `#[searchable]` + `#[translatable]`). Drop it from `--searchable`, or                  keep a separate non-translatable column to search."
            )));
        }
        if options.shard_key.as_deref().map(str::trim) == Some(name.as_str()) {
            return Err(GenerateError::Config(format!(
                "field '{name}' is `{{translatable}}` and cannot be the `--shard-key`: the                  shard is chosen by hashing the column value, and a container whose bytes                  change every time any locale is edited would move the row between shards.                  Shard on a stable column (e.g. `tenant_id`)."
            )));
        }
    }
    Ok(())
}

/// Reject every `{encrypted}` combination the encryption runtime or the
/// `#[model]` macro cannot honour (issue #1340), after `--unique` flags have
/// been folded into the fields so both spellings of "make this column
/// equality-queryable" are covered by one check.
///
/// The DSL parser already refuses the per-token combinations it can see
/// (non-`String` kinds, `Option<…>`, `:unique`, `:states(…)`). What is left are
/// the *flag* spellings, which only exist once the caller's options are known.
///
/// Generation-only, like [`validate_lock_version_field`]: `autumn destroy`
/// recomputes the plan it is about to revert, and refusing there would strand
/// the very files the user asked to delete — before `Plan::revert` ever sees
/// `--force`. Nothing these rules reject can be generated in the first place
/// today, but the destroy path must not depend on that staying true.
///
/// # Errors
/// [`GenerateError::InvalidField`] naming the offending field and the fix.
pub fn validate_encrypted_fields(
    fields: &[Field],
    options: &ModelOptions,
) -> Result<(), GenerateError> {
    for field in fields {
        if !field.is_encrypted() {
            continue;
        }
        // AC6, flag spelling: `--unique <col>` reaches the same broken state as
        // the DSL's `:unique`, so it gets the same refusal and the same fix.
        if field.is_randomized_encrypted() && field.unique {
            return Err(GenerateError::InvalidField {
                token: field.name.clone(),
                reason: randomized_equality_lookup_reason(&field.name, "is `unique`"),
            });
        }
        // `--index` is the third spelling of "make this column
        // equality-queryable". A B-tree index over RANDOMIZED ciphertext can
        // never serve a lookup — every write produces a different key for the
        // same plaintext — so it is pure write amplification that also
        // advertises a queryability the column does not have. (On a
        // deterministic column the index is genuinely useful, which is the
        // whole point of that mode, so it is allowed.)
        if field.is_randomized_encrypted() && options.indexes.iter().any(|i| i.trim() == field.name)
        {
            return Err(GenerateError::InvalidField {
                token: field.name.clone(),
                reason: randomized_equality_lookup_reason(&field.name, "has an `--index`"),
            });
        }
        // The shard key routes a query to a physical shard by hashing the
        // value the caller supplies. For a randomized column the caller only
        // ever holds plaintext, whose ciphertext differs on every write, so no
        // lookup could resolve the shard; for a deterministic one the shard
        // assignment would leak plaintext equality at the topology level.
        if options.shard_key.as_deref().map(str::trim) == Some(field.name.as_str()) {
            return Err(GenerateError::InvalidField {
                token: field.name.clone(),
                reason: format!(
                    "field '{}' is `{{encrypted}}` and cannot be the `--shard-key`: the shard is \
                     chosen by hashing the column value, which is ciphertext on disk — a \
                     randomized column hashes differently on every write, and a deterministic \
                     one would leak plaintext equality through shard placement. Shard on a \
                     non-encrypted column (e.g. `tenant_id`).",
                    field.name
                ),
            });
        }
        // Full-text search builds the stored `search_vector` from the DATABASE
        // column value, which for an encrypted column is ciphertext — so a
        // plaintext search would never match, in EITHER mode. The `#[model]`
        // macro rejects `#[searchable]` + `#[encrypted]` outright; mirror that
        // here so the failure names the field at generate time instead of
        // surfacing as a macro error in the generated app.
        if options.searchable.iter().any(|s| s.trim() == field.name) {
            return Err(GenerateError::InvalidField {
                token: field.name.clone(),
                reason: format!(
                    "field '{}' is `{{encrypted}}` and cannot be `--searchable`: full-text \
                     search indexes the stored column, which holds ciphertext, so plaintext \
                     searches would never match (the `#[model]` macro refuses `#[searchable]` \
                     + `#[encrypted]`). Drop it from `--searchable`, or keep a separate \
                     non-encrypted column to search.",
                    field.name
                ),
            });
        }
        // `#[encrypted]` columns must flow through the encrypting `serialize_as`
        // wrapper on insert; a `#[default]` column is excluded from the insert
        // entirely, so the row would hold a raw value the decrypting reader then
        // rejects as a malformed envelope. The `#[model]` macro refuses the
        // pair — surface it here, where the field name is still in hand.
        if options
            .defaults
            .iter()
            .filter_map(|d| d.split_once('=').map(|(name, _)| name.trim()))
            .any(|name| name == field.name)
        {
            return Err(GenerateError::InvalidField {
                token: field.name.clone(),
                reason: format!(
                    "field '{}' is `{{encrypted}}` and cannot also have a `--default`: a \
                     defaulted column bypasses the insert path that encrypts the value, so the \
                     column would store unencrypted data the decrypting reader then rejects \
                     (the `#[model]` macro refuses `#[default]` + `#[encrypted]`). Set the \
                     value explicitly on insert instead.",
                    field.name
                ),
            });
        }
    }
    Ok(())
}

/// The "you still need key material" next step for a model that declares at
/// least one `{encrypted}` column (issue #1340), or `None` when it declares
/// none.
///
/// The generated app boots either way — a missing key ring is a warning in
/// dev/test and a hard failure only in production (see
/// `autumn_web::app`'s `fail_fast_on_missing_encryption_keys`) — but every read
/// and write of the new column fails until the credentials exist. Naming the
/// command and the exact credential paths here is the difference between a
/// working on-ramp and a confusing first request.
#[must_use]
pub fn encryption_key_material_warning(fields: &[Field]) -> Option<String> {
    if !fields.iter().any(Field::is_encrypted) {
        return None;
    }
    let deterministic = fields
        .iter()
        .any(|f| f.encrypted_mode() == Some(EncryptedMode::Deterministic));
    // One flowing paragraph, like every other `plan.warn` — `Plan::print_warnings`
    // prefixes `Warning: ` and does no continuation-line handling, so an
    // embedded TOML block would hang off that prefix at the wrong indent. The
    // credentials are named as dotted paths, matching the runtime's own
    // "Attribute encryption misconfiguration" diagnostic, so the two are
    // greppable against each other; the guide carries the block to paste.
    let extra = if deterministic {
        " and `active_record_encryption.deterministic_key` (required by the \
         deterministic column(s) declared here)"
    } else {
        ""
    };
    Some(format!(
        "This model has at-rest encrypted column(s), which are inert until key material \
         exists: reads and writes of them fail in dev and the app refuses to boot in \
         production. Run `autumn credentials edit` and set \
         `active_record_encryption.primary_key`, \
         `active_record_encryption.key_derivation_salt`{extra} — each a fresh \
         `openssl rand -hex 32` (16 for the salt). \
         See docs/guide/attribute-encryption.md."
    ))
}

fn split_key_value(token: &str, sep: char) -> Result<(&str, &str), GenerateError> {
    let (key, value) = token
        .split_once(sep)
        .ok_or_else(|| GenerateError::InvalidField {
            token: token.to_owned(),
            reason: format!("expected `field{sep}value`"),
        })?;
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return Err(GenerateError::InvalidField {
            token: token.to_owned(),
            reason: format!("expected non-empty field and value in `field{sep}value`"),
        });
    }
    Ok((key, value))
}

pub fn field_by_name<'a>(fields: &'a [Field], name: &str) -> Option<&'a Field> {
    fields.iter().find(|field| field.name == name)
}

/// The column name a model declares to opt into optimistic concurrency
/// (issue #1318).
///
/// The framework's optimistic-locking primitive (issue #575) keys off the
/// `#[lock_version]` field attribute, not off a name — but the *generators*
/// need a nameless-DSL way to opt in, and `lock_version` is the name Rails,
/// Ecto, and this framework's own docs (`docs/guide/cloud-native.md`) already
/// use. Declaring `lock_version:i32` in a `generate model`/`generate scaffold`
/// field list is therefore the opt-in: the generator wires the attribute, the
/// SQL default, and (for scaffolds) the conflict-aware edit form.
pub const LOCK_VERSION_COLUMN: &str = "lock_version";

/// The model's optimistic-locking column, if it declares one (issue #1318).
///
/// Callers can assume the returned field passed [`validate_lock_version_field`]
/// — every planning entry point runs that check before rendering.
#[must_use]
pub fn lock_version_field(fields: &[Field]) -> Option<&Field> {
    field_by_name(fields, LOCK_VERSION_COLUMN)
}

/// Whether `field` is a usable optimistic-locking column (issue #1318): named
/// `lock_version`, non-nullable, and an integer counter.
///
/// The stricter test than [`lock_version_field`], for the call sites that must
/// decide what SQL/attribute to emit rather than whether to complain. A field
/// named `lock_version` that fails this predicate is rejected by
/// [`validate_lock_version_field`] on every planning path, so the two agree —
/// but the emission sites stay independently safe if a future entry point
/// forgets the check.
#[must_use]
pub fn is_lock_version_column(field: &Field) -> bool {
    field.name == LOCK_VERSION_COLUMN
        && !field.nullable
        && matches!(field.kind, FieldKind::I32 | FieldKind::I64)
}

/// Reject a `lock_version` column the locking primitive can't actually use.
///
/// `#[lock_version]`'s generated comparison reads the column as an `i64`, so a
/// non-integer or nullable column would either fail to compile in the emitted
/// model or silently never conflict-check. Failing here — before any file is
/// written — beats handing the author a scaffold that *looks* concurrency-safe
/// and isn't.
///
/// # Errors
/// Returns [`GenerateError::InvalidField`] when a field named `lock_version`
/// is not a non-nullable `i32`/`i64`.
pub fn validate_lock_version_field(
    fields: &[Field],
    defaults: &[String],
) -> Result<(), GenerateError> {
    let Some(field) = lock_version_field(fields) else {
        return Ok(());
    };
    if !is_lock_version_column(field) {
        return Err(GenerateError::InvalidField {
            token: format!("{}:{}", field.name, field.rust_type()),
            reason: format!(
                "the `{LOCK_VERSION_COLUMN}` column opts the model into optimistic locking \
                 (issue #575), so it must be a non-nullable `i32` or `i64` counter — the \
                 generated comparison reads it as an integer. Declare it as \
                 `{LOCK_VERSION_COLUMN}:i32` (or `{LOCK_VERSION_COLUMN}:i64`), or rename the \
                 column if it was not meant to be a lock version."
            ),
        });
    }
    // `unique` + a defaulted column is already rejected for explicit
    // `--default` flags above, for the reason that bites hardest here: the lock
    // column is DB-managed, so EVERY insert takes the same `DEFAULT 0` and the
    // second row created collides with the first. The check above runs before
    // the lock column's default is injected, so it never sees this pairing —
    // catch it here instead of emitting a table that accepts exactly one row.
    if field.unique {
        return Err(GenerateError::InvalidField {
            token: format!("{}:unique", field.name),
            reason: format!(
                "`{LOCK_VERSION_COLUMN}` cannot be `unique`: it is managed by the database \
                 and defaults to 0 on every insert, so a unique index on it would reject the \
                 second row ever created. Drop the `unique` marker."
            ),
        });
    }
    // A seed the counter cannot be incremented from. The generated `UPDATE`
    // evaluates `lock_version + 1` in SQL, and Postgres raises `integer out of
    // range` rather than wrapping — so seeding at the column's maximum makes the
    // FIRST update on every row a 500. Rejecting the seed is the only fix that
    // keeps the emitted statement simple; see the note on `lock_bump` about why
    // the generated SQL deliberately does not emulate the repository's
    // `wrapping_add`.
    let ceiling = if field.kind == FieldKind::I64 {
        i64::MAX
    } else {
        i64::from(i32::MAX)
    };
    for default in defaults {
        let Some((name, value)) = default.split_once('=') else {
            continue;
        };
        if name.trim() != LOCK_VERSION_COLUMN {
            continue;
        }
        if value.trim().parse::<i64>() == Ok(ceiling) {
            return Err(GenerateError::InvalidField {
                token: default.clone(),
                reason: format!(
                    "`{LOCK_VERSION_COLUMN}` cannot be seeded at {ceiling}, the largest value \
                     `{ty}` can hold: the generated UPDATE increments the column in SQL, so the \
                     first save on every row would fail with `integer out of range`. Seed a \
                     lower value, or declare `{LOCK_VERSION_COLUMN}:i64` for more headroom.",
                    ty = field.rust_type(),
                ),
            });
        }
    }
    Ok(())
}

/// Apply `--unique FIELD` flags (issue #1032) to already-parsed fields,
/// mirroring `--index`'s validate-then-apply shape. Unlike `--index`
/// (tracked externally in `ModelMetadata.indexes`), `unique` is carried on
/// the field itself ([`Field::unique`]) — the DSL's inline `:unique`
/// modifier and this flag converge on the same bit, so every SQL-emission
/// and repository-derive call site only ever needs to check `field.unique`.
///
/// # Errors
/// Returns [`GenerateError::InvalidField`] for an unknown field name.
pub fn apply_unique_flags(fields: &mut [Field], uniques: &[String]) -> Result<(), GenerateError> {
    for unique in uniques {
        let field_name = unique.trim();
        validate_known_field(fields, field_name, unique)?;
        if let Some(field) = fields.iter_mut().find(|f| f.name == field_name) {
            field.unique = true;
        }
    }
    Ok(())
}

fn validate_known_field(
    fields: &[Field],
    field_name: &str,
    token: &str,
) -> Result<(), GenerateError> {
    if field_by_name(fields, field_name).is_some() {
        Ok(())
    } else {
        Err(GenerateError::InvalidField {
            token: token.to_owned(),
            reason: format!("unknown field '{field_name}'"),
        })
    }
}

fn render_validation_attr(field: &Field, rule: &str) -> Result<String, String> {
    if rule == "url" || rule == "email" {
        // `richtext` is deliberately excluded even though `is_string_like`
        // accepts it for LENGTH rules: a Markdown body can never satisfy a
        // single-line format validator, so `#[validate(email)]` on one makes the
        // field unwritable. The DSL rejects `body:richtext{email}` for the same
        // reason (issue #1255) — this is the `--validate` flag's matching guard,
        // so the two spellings agree.
        if !is_string_like(field) || field.kind.is_rich_text() {
            return Err(format!("{rule} validation requires String or Text fields"));
        }
        return Ok(rule.to_owned());
    }

    let Some(rest) = rule.strip_prefix("length:") else {
        return Err("supported validation rules: url, email, length:min=N,max=N".to_owned());
    };
    if !is_string_like(field) {
        return Err("length validation requires String or Text fields".to_owned());
    }
    let mut min = None;
    let mut max = None;
    for part in rest.split(',') {
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| "length validation expects min=N and/or max=N".to_owned())?;
        let parsed = value
            .trim()
            .parse::<u64>()
            .map_err(|_| "length validation bounds must be unsigned integers".to_owned())?;
        match key.trim() {
            "min" => min = Some(parsed),
            "max" => max = Some(parsed),
            other => return Err(format!("unsupported length validation option '{other}'")),
        }
    }
    if min.is_none() && max.is_none() {
        return Err("length validation needs at least min=N or max=N".to_owned());
    }
    // A `min` greater than `max` is a self-contradictory rule that no string
    // can ever satisfy: not a mistake to silently accept and generate an
    // always-invalid field, one for which the generated smoke test's own
    // "valid submission" would fail (issue #1124 review).
    if let (Some(min), Some(max)) = (min, max)
        && min > max
    {
        return Err(format!(
            "length validation's min ({min}) cannot be greater than its max ({max})"
        ));
    }

    let mut args = Vec::new();
    if let Some(min) = min {
        args.push(format!("min = {min}"));
    }
    if let Some(max) = max {
        args.push(format!("max = {max}"));
    }
    Ok(format!("length({})", args.join(", ")))
}

const fn is_string_like(field: &Field) -> bool {
    matches!(
        field.kind,
        FieldKind::String | FieldKind::Text | FieldKind::RichText
    )
}

/// Strip a single layer of matching double or single quotes from a
/// `--default` value, tolerating an unquoted value. Shared by the
/// `String`/`Text` and `Enum` arms of [`sql_default_literal`], which both
/// accept `field=value`, `field="value"`, and `field='value'` equivalently.
fn unquote_default_value(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(value)
}

/// Check that a `--default` value's *significant* digits fit a
/// `NUMERIC(precision,scale)` column, so a decimal default never generates a
/// migration that fails at apply time with a Postgres "numeric field
/// overflow" (integer part too wide) — or silently gets rounded away
/// (fractional part too precise), which would defeat the entire point of a
/// field type that exists to avoid silent precision loss.
///
/// `value` is assumed to already be a plain (non-scientific-notation),
/// finite numeric string — the caller checks that first. Trailing/leading
/// zeros are trimmed before counting: `"1.500"` has one significant
/// fractional digit (5), not three, and `"007"` has one significant integer
/// digit, matching Postgres's own `NUMERIC` digit-counting semantics.
fn validate_decimal_default_fits(value: &str, precision: u32, scale: u32) -> Result<(), String> {
    let unsigned = value
        .strip_prefix('-')
        .or_else(|| value.strip_prefix('+'))
        .unwrap_or(value);
    let (int_part, frac_part) = unsigned.split_once('.').unwrap_or((unsigned, ""));

    let frac_digits = frac_part.trim_end_matches('0').len();
    if frac_digits > scale as usize {
        return Err(format!(
            "decimal default '{value}' has {frac_digits} fractional digit(s), but \
             decimal{{{precision},{scale}}} only allows {scale}"
        ));
    }

    let int_digits = int_part.trim_start_matches('0').len();
    let max_int_digits = precision - scale;
    if int_digits > max_int_digits as usize {
        return Err(format!(
            "decimal default '{value}' has {int_digits} integer digit(s), but \
             decimal{{{precision},{scale}}} only allows {max_int_digits}"
        ));
    }

    Ok(())
}

fn sql_default_literal(
    field: &Field,
    value: &str,
    backend: DatabaseBackend,
) -> Result<String, String> {
    match field.kind {
        FieldKind::Bool => match value.to_ascii_lowercase().as_str() {
            "true" => Ok("TRUE".to_owned()),
            "false" => Ok("FALSE".to_owned()),
            _ => Err("bool defaults must be true or false".to_owned()),
        },
        FieldKind::String | FieldKind::Text | FieldKind::RichText => {
            let unquoted = unquote_default_value(value);
            Ok(format!("'{}'", unquoted.replace('\'', "''")))
        }
        FieldKind::I32 => value
            .parse::<i32>()
            .map(|_| value.to_owned())
            .map_err(|_| "i32 defaults must fit the SQL INTEGER range".to_owned()),
        FieldKind::I64 => value
            .parse::<i64>()
            .map(|_| value.to_owned())
            .map_err(|_| "integer defaults must be valid integers".to_owned()),
        FieldKind::F32 | FieldKind::F64 => value
            .parse::<f64>()
            .map(|_| value.to_owned())
            .map_err(|_| "float defaults must be valid numbers".to_owned()),
        FieldKind::Decimal { precision, scale } => {
            let parsed: f64 = value
                .parse()
                .map_err(|_| "decimal defaults must be valid numbers".to_owned())?;
            if !parsed.is_finite() {
                return Err("decimal defaults must be finite numbers (not NaN/infinity)".to_owned());
            }
            if value.contains(['e', 'E']) {
                return Err(
                    "decimal defaults must be written in plain notation (e.g. '19.99'), \
                     not scientific notation"
                        .to_owned(),
                );
            }
            validate_decimal_default_fits(value, precision, scale)?;
            match backend {
                // Emitted as the original string, not `parsed.to_string()` —
                // an unquoted numeric literal is valid Postgres `NUMERIC` SQL
                // whether or not it round-trips exactly through `f64` (this arm
                // only used `f64` to reject non-numeric garbage above).
                DatabaseBackend::Postgres => Ok(value.to_owned()),
                // On SQLite the column is `TEXT`, so the default has to be a
                // text literal: unquoted, SQLite evaluates `DEFAULT 0.10`
                // numerically before applying TEXT affinity and stores `0.1`,
                // or scientific notation for a wide value, which
                // `Decimal::from_str` cannot read back at all.
                //
                // It also has to be the SAME text `SqliteDecimal` would write,
                // which is normalized (`db::sqlite_types`). A default of
                // `0.10` stored verbatim would never equal the `0.1` every
                // later write produces — a `find_by_…` could not match a row
                // holding its own default, and a unique index would admit
                // both. Normalizing through the real `Decimal` rather than by
                // hand is what guarantees the two agree (issue #1924).
                DatabaseBackend::Sqlite => {
                    use autumn_web::reexports::rust_decimal::Decimal;
                    let decimal = value.parse::<Decimal>().map_err(|err| {
                        format!(
                            "decimal default '{value}' is not representable as a \
                             rust_decimal::Decimal, the Rust type a SQLite decimal \
                             column round-trips through: {err}"
                        )
                    })?;
                    Ok(format!("'{}'", decimal.normalize()))
                }
            }
        }
        FieldKind::Json => {
            serde_json::from_str::<serde_json::Value>(value)
                .map_err(|err| format!("json default '{value}' is not valid JSON: {err}"))?;
            // Unlike the plain `String` arm above, the raw value is NOT run
            // through `unquote_default_value` first — JSON syntax already
            // carries its own quoting (`"hello"` is a JSON string; `{}`/`[]`/
            // `42`/`true` are not quoted at all), so stripping a layer here
            // would corrupt a JSON *string* default (`note:json="hi"`) into
            // an invalid literal. Postgres implicitly casts a single-quoted
            // string literal to the column's declared `JSONB` type in a
            // `DEFAULT` clause, so no explicit `::jsonb` cast is needed.
            Ok(format!("'{}'", value.replace('\'', "''")))
        }
        FieldKind::Uuid
        | FieldKind::NaiveDateTime
        | FieldKind::DateTime
        | FieldKind::Bytea
        | FieldKind::Attachment
        | FieldKind::References
        // A slug's value is always auto-derived from its `from` field on
        // create (issue #1260), never a static default.
        | FieldKind::Slug
        // A position's value is always assigned by the repository on insert
        // (issue #1358), never a static default.
        | FieldKind::Position
        // A commentable counter always starts at 0 and is thereafter moved by
        // the framework (issue #1367); the migration's own `DEFAULT 0` is the
        // only default it may have.
        | FieldKind::Commentable => Err(format!(
            "defaults for {} fields are not supported by `autumn generate` yet",
            field.rust_type()
        )),
        FieldKind::Enum => {
            let unquoted = unquote_default_value(value);
            if field.variants.iter().any(|v| v == unquoted) {
                Ok(format!("'{unquoted}'"))
            } else {
                Err(format!(
                    "'{unquoted}' is not a variant of this enum; expected one of: {}",
                    field.variants.join(", ")
                ))
            }
        }
    }
}

/// Render a baseline `#[model]` file (no soft-delete, sharding, or field
/// metadata) — the greenfield reference the `db pull` round-trip property
/// asserts byte-equivalence against. See `generate::introspect`.
#[cfg(test)]
#[must_use]
pub(super) fn render_model_file_for_test(name: &str, table: &str, fields: &[Field]) -> String {
    render_model_file(
        name,
        table,
        fields,
        &ModelMetadata::default(),
        false,
        None,
        IdType::BigSerial,
        DatabaseBackend::Postgres,
    )
}

/// Render the Rust enum type for an `enum{…}` field, plus the trait
/// machinery needed to store it as a `TEXT` column (issue #1030):
/// `Display`/`FromStr` for the form/edit-view round-trip, and manual
/// `diesel::serialize::ToSql`/`deserialize::FromSql` impls (the
/// `AsExpression`/`FromSqlRow` derives alone only describe the SQL type, not
/// how to encode/decode it).
///
/// Always derives `Default` (every model field's Rust type must — see the
/// comment inside). `default_variant`, if given, must be one of
/// `field.variants` and marks that variant `#[default]`, matching the
/// `--default field=variant` SQL default written to the migration;
/// otherwise the first declared variant is `#[default]`.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "This is a single template emitting one enum type plus its trait \
              impls — splitting it produces less readable output, not more."
)]
fn render_enum_decl(
    field: &Field,
    default_variant: Option<&str>,
    backend: DatabaseBackend,
) -> String {
    use std::fmt::Write as _;
    let ty = field
        .enum_type_name()
        .expect("render_enum_decl called on a non-enum field");
    let variants: Vec<String> = field.variants.iter().map(|v| pascal(v)).collect();

    let mut out = String::new();

    // Always derive `Default`, even without an explicit `--default`. The `#[model]`
    // macro's generated `UpdateX` patch struct wraps every field in `Patch<T>` and
    // unconditionally derives `Default` on itself, and `#[derive(Default)]` on a generic
    // type adds a `T: Default` bound for every type parameter whatever variant is
    // `#[default]`ed — so every field's Rust type must implement `Default`, a requirement
    // every other field kind already satisfies via `std`. Absent an explicit `--default
    // field=variant`, the first declared variant is the unsurprising choice, matching how
    // other kinds default to a canonical baseline such as `i32::default() == 0`.
    let default_raw = default_variant.unwrap_or_else(|| {
        field
            .variants
            .first()
            .expect("an enum field always has at least one variant")
    });
    let derives = [
        "Debug",
        "Clone",
        "Copy",
        "PartialEq",
        "Eq",
        "Default",
        "serde::Serialize",
        "serde::Deserialize",
        "diesel::expression::AsExpression",
        "diesel::deserialize::FromSqlRow",
    ];
    let _ = writeln!(out, "#[derive({})]", derives.join(", "));
    out.push_str("#[diesel(sql_type = diesel::sql_types::Text)]\n");
    let _ = writeln!(out, "pub enum {ty} {{");
    for (raw, variant) in field.variants.iter().zip(&variants) {
        if raw == default_raw {
            out.push_str("    #[default]\n");
        }
        let _ = writeln!(out, "    #[serde(rename = \"{raw}\")]");
        let _ = writeln!(out, "    {variant},");
    }
    out.push_str("}\n\n");

    let _ = writeln!(out, "impl {ty} {{");
    let _ = writeln!(
        out,
        "    pub const VARIANTS: [Self; {}] = [{}];",
        variants.len(),
        variants
            .iter()
            .map(|v| format!("Self::{v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    out.push('\n');
    out.push_str("    #[must_use]\n");
    out.push_str("    pub const fn as_str(&self) -> &'static str {\n");
    out.push_str("        match self {\n");
    for (raw, variant) in field.variants.iter().zip(&variants) {
        let _ = writeln!(out, "            Self::{variant} => \"{raw}\",");
    }
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    let _ = writeln!(out, "impl std::fmt::Display for {ty} {{");
    out.push_str("    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n");
    out.push_str("        f.write_str(self.as_str())\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    let _ = writeln!(out, "impl std::str::FromStr for {ty} {{");
    out.push_str("    type Err = String;\n\n");
    out.push_str("    fn from_str(s: &str) -> Result<Self, Self::Err> {\n");
    out.push_str("        match s {\n");
    for (raw, variant) in field.variants.iter().zip(&variants) {
        let _ = writeln!(out, "            \"{raw}\" => Ok(Self::{variant}),");
    }
    let _ = writeln!(
        out,
        "            _ => Err(\"must be one of {}\".to_owned()),",
        field.variants.join(", ")
    );
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    // The conversions target the app's ACTUAL backend: diesel implements
    // `ToSql`/`FromSql` per backend, and a generated app links only the diesel
    // backend feature its database needs, so emitting the other arm would not
    // compile (issue #1924).
    match backend {
        DatabaseBackend::Postgres => {
            let _ = writeln!(
                out,
                "impl diesel::serialize::ToSql<diesel::sql_types::Text, diesel::pg::Pg> for {ty} {{"
            );
            out.push_str(
                "    fn to_sql<'b>(&'b self, out: &mut diesel::serialize::Output<'b, '_, diesel::pg::Pg>) -> diesel::serialize::Result {\n",
            );
            out.push_str(
                "        <str as diesel::serialize::ToSql<diesel::sql_types::Text, diesel::pg::Pg>>::to_sql(self.as_str(), out)\n",
            );
            out.push_str("    }\n");
            out.push_str("}\n\n");

            let _ = writeln!(
                out,
                "impl diesel::deserialize::FromSql<diesel::sql_types::Text, diesel::pg::Pg> for {ty} {{"
            );
            out.push_str(
                "    fn from_sql(bytes: diesel::pg::PgValue<'_>) -> diesel::deserialize::Result<Self> {\n",
            );
            let _ = writeln!(
                out,
                "        let s = <String as diesel::deserialize::FromSql<diesel::sql_types::Text, diesel::pg::Pg>>::from_sql(bytes)?;"
            );
            out.push_str("        s.parse().map_err(Into::into)\n");
            out.push_str("    }\n");
            out.push_str("}\n");
        }
        DatabaseBackend::Sqlite => {
            // `set_value` (not the `str` delegate the Pg arm uses): diesel's
            // SQLite output buffer takes an owned value, so the borrowed
            // `&'static str` is handed over directly.
            let _ = writeln!(
                out,
                "impl diesel::serialize::ToSql<diesel::sql_types::Text, diesel::sqlite::Sqlite> for {ty} {{"
            );
            out.push_str(
                "    fn to_sql<'b>(&'b self, out: &mut diesel::serialize::Output<'b, '_, diesel::sqlite::Sqlite>) -> diesel::serialize::Result {\n",
            );
            out.push_str("        out.set_value(self.as_str());\n");
            out.push_str("        Ok(diesel::serialize::IsNull::No)\n");
            out.push_str("    }\n");
            out.push_str("}\n\n");

            let _ = writeln!(
                out,
                "impl diesel::deserialize::FromSql<diesel::sql_types::Text, diesel::sqlite::Sqlite> for {ty} {{"
            );
            out.push_str(
                "    fn from_sql(bytes: <diesel::sqlite::Sqlite as diesel::backend::Backend>::RawValue<'_>) -> diesel::deserialize::Result<Self> {\n",
            );
            let _ = writeln!(
                out,
                "        let s = <String as diesel::deserialize::FromSql<diesel::sql_types::Text, diesel::sqlite::Sqlite>>::from_sql(bytes)?;"
            );
            out.push_str("        s.parse().map_err(Into::into)\n");
            out.push_str("    }\n");
            out.push_str("}\n");
        }
    }

    out
}

#[allow(
    clippy::too_many_arguments,
    reason = "one parameter per axis of the emitted model file; a struct here would \
              only rename the same list"
)]
fn render_model_file(
    name: &str,
    table: &str,
    fields: &[Field],
    metadata: &ModelMetadata,
    soft_delete: bool,
    shard_key: Option<&str>,
    id_type: IdType,
    // Selects the field Rust types and the generated enum's diesel conversions
    // (issue #1924). Postgres output is byte-for-byte unchanged.
    backend: DatabaseBackend,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(fields.len() * 128 + 256);
    out.push_str("//! Generated by `autumn generate`.\n");
    out.push_str("//!\n");
    out.push_str("//! Edit this file freely — once a generator has run, the\n");
    out.push_str("//! framework treats this as ordinary user code.\n\n");
    let _ = writeln!(out, "use crate::schema::{table};");
    out.push('\n');
    for f in fields {
        if f.is_enum() {
            let default_variant = metadata
                .defaults
                .get(&f.name)
                .and_then(|literal| literal.strip_prefix('\''))
                .and_then(|s| s.strip_suffix('\''));
            out.push_str(&render_enum_decl(f, default_variant, backend));
            out.push('\n');
        }
    }
    // Struct-level `#[commentable(...)]` (#1367), emitted by the `comments:commentable`
    // DSL token. It brings the repository's `add_comment`, `comment_thread`, and
    // `delete_comment` helpers into existence and registers this model with the
    // framework's generic comment router. `by = User` is the convention `autumn generate
    // auth` produces; a project whose author model is named differently changes that one
    // word. No `author_name` is emitted on purpose: the generated `User` carries an
    // `email`, and defaulting a public display name to it would leak addresses into every
    // rendered thread.
    if fields.iter().any(|f| f.kind.is_commentable()) {
        out.push_str(
            "// Threaded, polymorphic comments (#1367): one shared `comments` table,\n\
             // keyed on `(commentable_type, commentable_id)`, attaches to any number of\n\
             // models. Point `by` at this app's author model, and add\n\
             // `author_name = <column>` to render display names instead of `user #id`.\n",
        );
    }
    out.push_str("#[autumn_web::model]\n");
    // `#[commentable]` is consumed by `#[model]`, so it must sit BELOW it —
    // attribute macros are applied top-down, and above it the compiler would
    // report `cannot find attribute commentable in this scope`.
    if let Some(counter) = fields.iter().find(|f| f.kind.is_commentable()) {
        let by = metadata
            .commentable_author
            .as_deref()
            .map_or_else(String::new, |author| format!("by = {author}, "));
        let _ = writeln!(out, "#[commentable({by}counter_cache = {})]", counter.name);
    }
    // Struct-level `#[searchable(language = "…")]` (issue #1319) opts the model
    // into full-text search; the per-field `#[searchable(weight = "…")]` below
    // declare which columns feed the `search_vector` and at what rank weight.
    if let Some(language) = metadata.search_language() {
        let _ = writeln!(out, "#[searchable(language = \"{language}\")]");
    }
    if let Some(key) = shard_key {
        let _ = writeln!(out, "#[shard_key = \"{key}\"]");
    }
    let _ = writeln!(out, "pub struct {name} {{");
    out.push_str("    #[id]\n");
    let _ = writeln!(out, "    pub id: {},", id_type.rust_type());
    for f in fields {
        if metadata.indexes.contains(&f.name) {
            out.push_str("    #[indexed]\n");
        }
        if let Some((_, weight)) = metadata.searchable().iter().find(|(n, _)| n == &f.name) {
            let _ = writeln!(out, "    #[searchable(weight = \"{weight}\")]");
        }
        if let Some(validations) = metadata.validations.get(&f.name) {
            for validation in validations {
                let _ = writeln!(out, "    #[validate({validation})]");
            }
        }
        // Issue #1318: the optimistic-locking column carries `#[lock_version]`
        // rather than `#[default]`. Both mark the column DB-managed (excluded
        // from `New{Model}`), but only `#[lock_version]` puts the expected
        // version on `Update{Model}` and makes `#[repository]`'s update raise
        // `RepositoryError::Conflict` on a stale write — the whole point of
        // declaring the column. `parse_model_metadata` records its SQL
        // `DEFAULT 0` separately, so the migration still backfills the INSERT.
        if is_lock_version_column(f) {
            out.push_str("    #[lock_version]\n");
        } else if f.kind.is_position() {
            // Issue #1358: `#[position]` marks the column DB-managed
            // (excluded from `New{Model}`/`Update{Model}`, like
            // `#[lock_version]`) — the generated repository assigns and
            // maintains its value entirely; see `excluded_from_new` in
            // `autumn-macros`.
            out.push_str("    #[position]\n");
        } else if metadata.defaults.contains_key(&f.name) {
            out.push_str("    #[default]\n");
        }
        // A `:states(…)` DSL modifier (issue #1326) re-emits as a
        // `#[state_machine(transitions(…))]` attribute, in the exact grammar the
        // `autumn_web::model` macro accepts: bare-ident states, an optional
        // `: "guard"` string. Absent when the field declared no state machine —
        // that no-op path is what keeps the non-state-machine output
        // byte-identical to before this feature.
        if let Some(sm) = &f.state_machine {
            let mut inner = String::new();
            for (i, t) in sm.transitions.iter().enumerate() {
                if i > 0 {
                    inner.push_str(", ");
                }
                let _ = write!(inner, "{} -> {}", t.from, t.to);
                if let Some(guard) = &t.guard {
                    let _ = write!(inner, ": \"{guard}\"");
                }
            }
            let _ = writeln!(out, "    #[state_machine(transitions({inner}))]");
        }
        // Issue #1340: a `{encrypted}` / `{encrypted:deterministic}` DSL
        // modifier re-emits as the `#[encrypted(...)]` attribute the `#[model]`
        // macro parses, so the column is stored as an opaque base64 ciphertext
        // envelope while staying a plain `String` in Rust. This is also what
        // the admin generator's `detect_encrypted_fields` reads back off the
        // model source to redact the column, so the spelling here is a
        // contract, not cosmetics. Absent for a plaintext column — that no-op
        // path is what keeps unencrypted output byte-identical.
        match f.encrypted_mode() {
            Some(EncryptedMode::Randomized) => out.push_str("    #[encrypted]\n"),
            Some(EncryptedMode::Deterministic) => {
                out.push_str("    #[encrypted(deterministic)]\n");
            }
            None => {}
        }
        // Issue #1384: a `{translatable}` DSL modifier re-emits as the
        // `#[translatable]` attribute the `#[model]` macro parses. The field's
        // Rust type (`autumn_web::i18n::Translated`, from `Field::rust_type`)
        // is what carries the behaviour; the attribute is what registers the
        // column and emits the `<field>_localized` / `available_locales(..)`
        // accessors. Absent for a monolingual column — that no-op path is what
        // keeps non-translatable output byte-identical.
        if f.is_translatable() {
            out.push_str("    #[translatable]\n");
        }
        // Issue #1255: a `richtext` column renders as a bare `String`, exactly
        // like `String`/`Text`, so nothing in the emitted source would otherwise
        // distinguish it. Emit a marker doc comment that (a) tells a human
        // reading the model that the column holds Markdown source to be rendered
        // through `render_user_content`, and (b) lets
        // [`model_string_columns`] skip it when picking a `references` display
        // label — a whole Markdown body is the worst possible `<select>` option
        // text. See [`RICH_TEXT_MARKER_DOC`].
        if f.kind.is_rich_text() {
            let _ = writeln!(out, "    /// {RICH_TEXT_MARKER_DOC}");
        }
        let _ = writeln!(out, "    pub {}: {},", f.name, f.rust_type_for(backend));
    }
    if soft_delete {
        // `deleted_at` must come before `created_at` here, matching the column
        // order `create_table_sql_with_metadata_and_id` and
        // `schema_table_block_with_id` emit: they append the soft-delete field to
        // the field list, then always append `created_at` last. The repository
        // macro's generated insert-then-`RETURNING` query loads into this struct
        // positionally, so a struct field order that does not match the table's
        // column order produces a Diesel `CompatibleType` mismatch at compile time.
        //
        // `deleted_at` is otherwise DB-managed — NULL on insert, set only by the
        // destroy handler. The migration declares it nullable with no explicit SQL
        // DEFAULT, so Postgres inserts NULL whenever it is omitted from the INSERT
        // column list, and `#[default]` excludes it from `NewX`/`UpdateX`
        // accordingly. Without it the `#[model]` macro treats `deleted_at` as a
        // required field that neither the create nor the update handler populates.
        out.push_str("    #[default]\n");
        out.push_str("    pub deleted_at: Option<chrono::NaiveDateTime>,\n");
    }
    out.push_str("    #[default]\n");
    out.push_str("    pub created_at: chrono::NaiveDateTime,\n");
    out.push_str("}\n");
    out
}

#[cfg(test)]
// Test inputs like `"email:String{encrypted:deterministic}"` are literal DSL
// tokens passed to the generators, not format strings — the `{…}` is the
// scaffold's own constraint-modifier syntax under test.
#[allow(clippy::literal_string_with_formatting_args)]
mod tests {
    use super::*;
    use crate::generate::Flags;
    use crate::generate::emit::Action;
    use std::fs;
    use tempfile::TempDir;

    fn project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        tmp
    }

    /// A project whose `Cargo.toml` actually declares `autumn-web`, so the
    /// feature-wiring pass has a dependency line to edit (the bare `project()`
    /// fixture has none, which no real `autumn new` project ever does).
    fn project_with_autumn_web_dep() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = { version = \"0.6\" }\n",
        )
        .unwrap();
        tmp
    }

    fn paths(plan: &Plan) -> Vec<String> {
        plan.actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(&plan.project_root)
                    .unwrap()
                    .display()
                    .to_string()
                    // Normalize for cross-platform comparisons (Windows uses `\`).
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn plan_creates_expected_file_set() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into(), "body:Text".into()],
            "20260427000000",
        )
        .unwrap();
        let p = paths(&plan);
        assert!(p.contains(&"src/models/post.rs".into()));
        assert!(p.contains(&"src/models/mod.rs".into()));
        assert!(p.contains(&"migrations/20260427000000_create_posts/up.sql".into()));
        assert!(p.contains(&"migrations/20260427000000_create_posts/down.sql".into()));
        assert!(p.contains(&"src/schema.rs".into()));
    }

    #[test]
    fn plan_records_reverts_for_mod_decl_schema_table_and_cargo_deps() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(plan.reverts.iter().any(|r| matches!(
            r,
            crate::generate::emit::Revert::ModDecl { name, .. } if name == "post"
        )));
        assert!(plan.reverts.iter().any(|r| matches!(
            r,
            crate::generate::emit::Revert::SchemaTable { table, .. } if table == "posts"
        )));
        assert!(plan.reverts.iter().any(|r| matches!(
            r,
            crate::generate::emit::Revert::CargoDeps { names, .. } if names.iter().any(|n| n == "diesel")
        )));
    }

    #[test]
    fn generate_then_destroy_model_round_trips_to_original_project_state() {
        // Mirrors a real `autumn new` project's Cargo.toml: `[dependencies]`
        // already exists (autumn-web, diesel_migrations) before any
        // generator runs — `diesel_migrations` is template-shipped (see
        // `TEMPLATE_SHIPPED_CARGO_DEPS`) so `Revert::CargoDeps` never
        // targets it, matching a real project where it always predates
        // any `generate` call.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n\n[dependencies]\nautumn-web = \"0.6.0\"\ndiesel_migrations = \"2\"\n",
        )
        .unwrap();
        let cargo_path = tmp.path().join("Cargo.toml");
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();

        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(tmp.path().join("src/models/post.rs").exists());

        // Destroy recomputes the plan from the same params (a fresh
        // timestamp doesn't matter here since the migration dir is matched
        // by suffix), then reverts it.
        let destroy_plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "99999999999999",
        )
        .unwrap();
        destroy_plan.revert(Flags::default()).unwrap();

        assert!(!tmp.path().join("src/models/post.rs").exists());
        assert!(!tmp.path().join("src/models/mod.rs").exists());
        assert!(!tmp.path().join("src/schema.rs").exists());
        assert!(
            fs::read_dir(tmp.path().join("migrations")).map_or(true, |mut d| d.next().is_none()),
            "migration directory must be removed"
        );
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    #[test]
    fn destroying_one_of_two_models_keeps_shared_dep_the_other_still_needs() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n\n[dependencies]\nautumn-web = \"0.6.0\"\ndiesel_migrations = \"2\"\n",
        )
        .unwrap();
        let cargo_path = tmp.path().join("Cargo.toml");

        plan_model(tmp.path(), "Post", &["owner:Uuid".into()], "20260427000000")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_model(
            tmp.path(),
            "Comment",
            &["owner:Uuid".into()],
            "20260427000001",
        )
        .unwrap()
        .execute(Flags::default())
        .unwrap();
        assert!(fs::read_to_string(&cargo_path).unwrap().contains("uuid"));

        // Destroying Post alone must NOT strip `uuid` — Comment's model file
        // still uses it.
        plan_model(tmp.path(), "Post", &["owner:Uuid".into()], "99999999999999")
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("src/models/post.rs").exists());
        assert!(tmp.path().join("src/models/comment.rs").exists());
        let cargo_after = fs::read_to_string(&cargo_path).unwrap();
        assert!(
            cargo_after.contains("uuid"),
            "uuid must survive — Comment's model still uses it: {cargo_after}"
        );

        // Now destroy the last remaining model — `uuid` must finally go.
        plan_model(
            tmp.path(),
            "Comment",
            &["owner:Uuid".into()],
            "99999999999998",
        )
        .unwrap()
        .revert(Flags::default())
        .unwrap();
        assert!(!tmp.path().join("src/models/comment.rs").exists());
        assert!(
            !fs::read_to_string(&cargo_path).unwrap().contains("uuid"),
            "uuid must be removed once no model uses it anymore"
        );
    }

    #[test]
    fn destroying_last_model_keeps_dep_still_used_by_hand_written_code() {
        // Codex PR review (issue #1048): a project can hand-add a dependency
        // for its own reasons, unrelated to any generated resource. Destroy
        // must not strip it just because the last model that also happened
        // to need it is gone.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n\n[dependencies]\nautumn-web = \"0.6.0\"\ndiesel_migrations = \"2\"\n",
        )
        .unwrap();
        let cargo_path = tmp.path().join("Cargo.toml");

        plan_model(tmp.path(), "Post", &["owner:Uuid".into()], "20260427000000")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        // Hand-written code elsewhere in the project also uses `uuid`,
        // independent of the generated model.
        fs::create_dir_all(tmp.path().join("src/tasks")).unwrap();
        fs::write(
            tmp.path().join("src/tasks/cleanup.rs"),
            "pub fn new_id() -> uuid::Uuid {\n    uuid::Uuid::new_v4()\n}\n",
        )
        .unwrap();
        assert!(fs::read_to_string(&cargo_path).unwrap().contains("uuid"));

        plan_model(tmp.path(), "Post", &["owner:Uuid".into()], "99999999999999")
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("src/models/post.rs").exists());
        assert!(
            fs::read_to_string(&cargo_path).unwrap().contains("uuid"),
            "uuid must survive — hand-written src/tasks/cleanup.rs still uses it"
        );
    }

    #[test]
    fn plan_rejects_lowercase_first_char() {
        let tmp = project();
        let err = plan_model(tmp.path(), "123Bad", &[], "20260427000000").unwrap_err();
        assert!(matches!(err, GenerateError::InvalidName(_, _)));
    }

    #[test]
    fn plan_rejects_reserved_resource_names() {
        // Without this guard, `mod` → `src/models/mod.rs` would silently
        // overwrite the per-resource model file with the mod aggregator.
        for name in ["mod", "main", "lib", "Mod"] {
            let tmp = project();
            let err = plan_model(tmp.path(), name, &[], "20260427000000").unwrap_err();
            assert!(
                matches!(err, GenerateError::InvalidName(_, _)),
                "expected '{name}' to be rejected"
            );
        }
    }

    #[test]
    fn plan_rejects_keyword_resource_names() {
        // `Type` → `mod type;` is invalid Rust syntax without raw idents.
        for name in ["Type", "type", "Match", "match", "Self", "Trait"] {
            let tmp = project();
            let err = plan_model(tmp.path(), name, &[], "20260427000000").unwrap_err();
            assert!(
                matches!(err, GenerateError::InvalidName(_, _)),
                "expected '{name}' to be rejected as a keyword"
            );
            assert!(
                err.to_string().contains("keyword"),
                "expected keyword error for '{name}'; got: {err}"
            );
        }
    }

    #[test]
    fn plan_rejects_id_or_created_at_as_user_field() {
        // The model template always emits `id` and `created_at`. Letting the
        // user re-declare them would produce duplicate struct members and
        // duplicate SQL columns.
        for token in ["id:i64", "created_at:NaiveDateTime"] {
            let tmp = project();
            let err =
                plan_model(tmp.path(), "Post", &[token.into()], "20260427000000").unwrap_err();
            assert!(
                matches!(err, GenerateError::InvalidField { .. }),
                "expected '{token}' to be rejected"
            );
            assert!(
                err.to_string().contains("reserved"),
                "expected reserved-field error for '{token}'; got: {err}"
            );
        }
    }

    #[test]
    fn plan_rejects_unsupported_field_type() {
        let tmp = project();
        let err = plan_model(
            tmp.path(),
            "Post",
            &["price:Money".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(matches!(err, GenerateError::InvalidField { .. }));
    }

    #[test]
    fn plan_outside_project_root_errors() {
        let tmp = TempDir::new().unwrap();
        let err = plan_model(tmp.path(), "Post", &[], "20260427000000").unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    // ── enum field: generated Rust type (issue #1030) ───────────────────────

    #[test]
    fn model_file_declares_enum_type() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &[
                "title:String".into(),
                "status:enum{draft,published,archived}".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(model.contains("pub enum Status"), "got:\n{model}");
        assert!(
            model.contains("#[diesel(sql_type = diesel::sql_types::Text)]"),
            "got:\n{model}"
        );
        assert!(
            model.contains("diesel::expression::AsExpression"),
            "got:\n{model}"
        );
        assert!(
            model.contains("diesel::deserialize::FromSqlRow"),
            "got:\n{model}"
        );
        assert!(
            model.contains("#[serde(rename = \"draft\")]"),
            "got:\n{model}"
        );
        assert!(
            model.contains("#[serde(rename = \"published\")]"),
            "got:\n{model}"
        );
        assert!(
            model.contains("#[serde(rename = \"archived\")]"),
            "got:\n{model}"
        );
        assert!(model.contains("Draft,"), "got:\n{model}");
        assert!(model.contains("Published,"), "got:\n{model}");
        assert!(model.contains("Archived,"), "got:\n{model}");
        assert!(model.contains("pub status: Status,"), "got:\n{model}");
    }

    // ── state machine field emission (issue #1326) ──────────────────────────

    /// AC5: a model with no state-machine field must render exactly as before
    /// this feature — no `#[state_machine]` attribute leaks into the no-SM path.
    /// (The unchanged pre-existing golden/model tests are the primary
    /// byte-identical proof; this is the explicit no-leak assertion.)
    #[test]
    fn model_file_without_state_machine_emits_no_attribute() {
        let fields = crate::generate::dsl::parse_fields(&[
            "title:String".into(),
            "body:Text".into(),
            "published:bool".into(),
        ])
        .unwrap();
        let model = render_model_file_for_test("Post", "posts", &fields);
        assert!(
            !model.contains("#[state_machine"),
            "no state machine declared, so no attribute should render; got:\n{model}"
        );
    }

    /// A `:states(…)` field renders a `#[state_machine(transitions(…))]`
    /// attribute in the exact grammar the `autumn_web::model` macro accepts
    /// (bare-ident states, an optional `: \"guard\"` string), on a `String` field.
    #[test]
    fn model_file_emits_state_machine_attribute() {
        let fields = crate::generate::dsl::parse_fields(&[
            "status:String:states(draft -> published: can_publish, published -> archived)".into(),
        ])
        .unwrap();
        let model = render_model_file_for_test("Page", "pages", &fields);
        assert!(
            model.contains(
                "#[state_machine(transitions(draft -> published: \"can_publish\", \
                 published -> archived))]"
            ),
            "got:\n{model}"
        );
        assert!(model.contains("pub status: String,"), "got:\n{model}");
    }

    // ── `{translatable}` per-locale content (issue #1384) ───────────────────

    /// AC7 (negative half): a model with no `{translatable}` field renders
    /// exactly as before — the attribute never leaks into the ordinary path.
    #[test]
    fn model_file_without_translatable_field_emits_no_attribute() {
        let fields =
            crate::generate::dsl::parse_fields(&["title:String".into(), "body:Text".into()])
                .unwrap();
        let model = render_model_file_for_test("Post", "posts", &fields);
        assert!(
            !model.contains("#[translatable"),
            "nothing declared translatable, so no attribute should render; got:\n{model}"
        );
        assert!(!model.contains("Translated"), "got:\n{model}");
    }

    /// AC1: the DSL token re-emits as `#[translatable]` on a field typed as the
    /// per-locale container, and a plain column in the same model is untouched.
    #[test]
    fn model_file_emits_translatable_attribute_and_container_type() {
        let fields = crate::generate::dsl::parse_fields(&[
            "title:String{translatable}".into(),
            "body:Text{translatable}".into(),
            "slug:String".into(),
        ])
        .unwrap();
        let model = render_model_file_for_test("Post", "posts", &fields);
        assert!(
            model.contains("    #[translatable]\n    pub title: autumn_web::i18n::Translated,"),
            "got:\n{model}"
        );
        assert!(
            model.contains("    #[translatable]\n    pub body: autumn_web::i18n::Translated,"),
            "got:\n{model}"
        );
        assert!(model.contains("    pub slug: String,"), "got:\n{model}");
        assert!(
            !model.contains("#[translatable]\n    pub slug"),
            "plain column must not pick up the attribute; got:\n{model}"
        );
    }

    /// AC1 + AC6 end to end through the real planner: the emitted model,
    /// `schema.rs` entry, migration DDL and `Cargo.toml` feature all land
    /// together, so a `generate model` with a translatable column produces a
    /// project that actually builds.
    #[test]
    fn translatable_model_plan_emits_model_schema_migration_and_feature() {
        let tmp = project_with_autumn_web_dep();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String{translatable}".into(), "slug:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("    #[translatable]\n    pub title: autumn_web::i18n::Translated,"),
            "model: {model}"
        );

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(schema.contains("title -> Text,"), "schema: {schema}");

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("title TEXT NOT NULL DEFAULT '{}'"),
            "up.sql: {up}"
        );
        // AC6: what the generator actually wrote classifies as safe.
        assert!(
            crate::migrate::safety::is_safe(&crate::migrate::safety::classify_sql(&up)),
            "generated migration must classify safe: {up}"
        );

        // The container type lives behind the non-default `i18n` feature.
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("i18n"),
            "generate model must enable autumn-web's `i18n` feature: {cargo}"
        );
    }

    /// #1384 (Codex round 5): `autumn destroy model` must be able to take the
    /// non-default `i18n` feature back out. The revert has to be registered
    /// unconditionally — on the destroy path the feature is already present, so
    /// the Cargo.toml edit is a no-op and a revert pushed only when the edit
    /// changed something would never exist where it is needed.
    #[test]
    fn a_translatable_model_registers_a_revert_for_the_i18n_feature() {
        let tmp = project_with_autumn_web_dep();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String{translatable}".into()],
            "20260427000000",
        )
        .unwrap();
        let has_feature_revert = plan.reverts.iter().any(|r| {
            matches!(
                r,
                crate::generate::emit::Revert::CargoAutumnWebFeature { feature, owner_dir, .. }
                    if feature == "i18n"
                        && owner_dir.as_deref() == Some(&tmp.path().join("src/models"))
            )
        });
        assert!(
            has_feature_revert,
            "expected a CargoAutumnWebFeature revert owned by src/models, got {:?}",
            plan.reverts
        );

        // Recomputing the plan against a project that ALREADY has the feature
        // (the destroy path) still registers it.
        plan.execute(Flags::default()).unwrap();
        let replanned = plan_model(
            tmp.path(),
            "Post",
            &["title:String{translatable}".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            replanned.reverts.iter().any(|r| matches!(
                r,
                crate::generate::emit::Revert::CargoAutumnWebFeature { feature, .. }
                    if feature == "i18n"
            )),
            "the revert must survive a replan where the feature is already present"
        );
    }

    /// A model with no translatable column must not gain the `i18n` feature.
    #[test]
    fn a_plain_model_plan_does_not_enable_the_i18n_feature() {
        let tmp = project_with_autumn_web_dep();
        let before = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap_or_default();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let after = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap_or_default();
        assert_eq!(before.contains("i18n"), after.contains("i18n"));
    }

    /// The flag spellings bypass `parse_field`'s cross-checks (they are folded
    /// in afterwards), so they need their own refusal — otherwise `--unique`
    /// ships a UNIQUE index over a JSON container and `--index`/`--searchable`
    /// emit a model the `#[model]` macro rejects.
    #[test]
    fn translatable_columns_refuse_the_flag_spellings_of_their_restrictions() {
        let cases: [(&str, ModelOptions); 4] = [
            (
                "--unique",
                ModelOptions {
                    uniques: vec!["title".into()],
                    ..ModelOptions::default()
                },
            ),
            (
                "--index",
                ModelOptions {
                    indexes: vec!["title".into()],
                    ..ModelOptions::default()
                },
            ),
            (
                "--searchable",
                ModelOptions {
                    searchable: vec!["title".into()],
                    ..ModelOptions::default()
                },
            ),
            (
                "--shard-key",
                ModelOptions {
                    shard_key: Some("title".into()),
                    ..ModelOptions::default()
                },
            ),
        ];
        for (flag, options) in cases {
            let tmp = project();
            let err = plan_model_with_options(
                tmp.path(),
                "Post",
                &["title:String{translatable}".into()],
                "20260427000000",
                &options,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("translatable"), "{flag}: {err}");
            assert!(err.contains("title"), "{flag}: {err}");
        }
    }

    // ── `{encrypted}` at-rest column encryption (issue #1340) ───────────────

    /// AC3 (negative half): a model with no `{encrypted}` field must render
    /// exactly as before this feature — no `#[encrypted]` attribute leaks into
    /// the ordinary path.
    #[test]
    fn model_file_without_encrypted_field_emits_no_attribute() {
        let fields = crate::generate::dsl::parse_fields(&[
            "title:String".into(),
            "body:Text".into(),
            "published:bool".into(),
        ])
        .unwrap();
        let model = render_model_file_for_test("Post", "posts", &fields);
        assert!(
            !model.contains("#[encrypted"),
            "no encryption declared, so no attribute should render; got:\n{model}"
        );
    }

    /// AC7: `{encrypted}` emits a bare `#[encrypted]` and
    /// `{encrypted:deterministic}` emits `#[encrypted(deterministic)]`, on a
    /// plain `String` model field (the macro's v1 requirement).
    #[test]
    fn model_file_emits_encrypted_attributes_for_both_modes() {
        let fields = crate::generate::dsl::parse_fields(&[
            "api_token:String{encrypted}".into(),
            "email:String{encrypted:deterministic}".into(),
            "username:String".into(),
        ])
        .unwrap();
        let model = render_model_file_for_test("Account", "accounts", &fields);
        assert!(
            model.contains("    #[encrypted]\n    pub api_token: String,"),
            "randomized column must carry a bare `#[encrypted]`; got:\n{model}"
        );
        assert!(
            model.contains("    #[encrypted(deterministic)]\n    pub email: String,"),
            "deterministic column must carry the mode; got:\n{model}"
        );
        // AC3: a non-encrypted DSL field in the SAME model is unaffected.
        assert!(
            model.contains("    pub username: String,"),
            "plain column must be untouched; got:\n{model}"
        );
        assert!(
            !model.contains("#[encrypted]\n    pub username"),
            "plain column must not pick up the attribute; got:\n{model}"
        );
    }

    /// The attribute composes with the `{…}` validation fan-out: both land on
    /// the same field, and the field stays a plain `String`.
    #[test]
    fn model_file_emits_encrypted_alongside_validation_attributes() {
        // Goes through the real plan (not `render_model_file_for_test`) because
        // the `{…}` validation fan-out is applied by `parse_model_metadata`.
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Account",
            &["email:String{encrypted:deterministic,max=254,email}".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/account.rs")).unwrap();
        assert!(
            model.contains("#[validate(length(max = 254))]"),
            "got:\n{model}"
        );
        assert!(model.contains("#[validate(email)]"), "got:\n{model}");
        assert!(
            model.contains("#[encrypted(deterministic)]"),
            "got:\n{model}"
        );
        assert!(model.contains("pub email: String,"), "got:\n{model}");
    }

    /// AC4: the generated migration column is unbounded `TEXT` — sized for the
    /// base64 ciphertext envelope, never a plaintext-width type — and the
    /// migration says so, so whoever reads the SQL later knows why.
    #[test]
    fn encrypted_column_migration_is_text_with_envelope_comment() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Account",
            &[
                "username:String".into(),
                "api_token:String{encrypted}".into(),
            ],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_accounts/up.sql"),
        )
        .unwrap();
        assert!(up.contains("api_token TEXT NOT NULL"), "up.sql: {up}");
        // The comment is SQL-comment-only: nothing but comments precede
        // `CREATE TABLE`, so the DDL itself is unchanged.
        let (head, _) = up
            .split_once("CREATE TABLE")
            .unwrap_or_else(|| panic!("up.sql: {up}"));
        assert!(
            head.lines()
                .all(|l| l.trim().is_empty() || l.trim_start().starts_with("--")),
            "only comments may precede CREATE TABLE: {up}"
        );
        assert!(
            head.contains("api_token"),
            "migration must name the encrypted column in a comment: {up}"
        );
        assert!(
            head.contains("base64") && head.contains("envelope"),
            "migration comment must explain the ciphertext envelope sizing: {up}"
        );
        assert!(
            head.contains("VARCHAR"),
            "migration comment must warn against narrowing to a bounded type: {up}"
        );
    }

    /// A model with no encrypted column keeps a byte-identical migration —
    /// no stray comment block leaks into the ordinary path.
    #[test]
    fn unencrypted_model_migration_has_no_encryption_comment() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(!up.contains("envelope"), "up.sql: {up}");
        assert!(up.starts_with("CREATE TABLE posts ("), "up.sql: {up}");
    }

    /// AC6 (flag half): `--unique` reaches the same broken state as `:unique`,
    /// so the guard must run after `apply_unique_flags`.
    #[test]
    fn unique_flag_on_randomized_encrypted_field_is_rejected() {
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Account",
            &["api_token:String{encrypted}".into()],
            "20260427000000",
            &ModelOptions {
                uniques: vec!["api_token".into()],
                ..ModelOptions::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_token"), "must name the field: {msg}");
        assert!(
            msg.contains("deterministic"),
            "must point at the fix: {msg}"
        );
    }

    /// `--unique` on a DETERMINISTIC encrypted column is the supported path.
    #[test]
    fn unique_flag_on_deterministic_encrypted_field_is_allowed() {
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Account",
            &["email:String{encrypted:deterministic}".into()],
            "20260427000000",
            &ModelOptions {
                uniques: vec!["email".into()],
                ..ModelOptions::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_accounts/up.sql"),
        )
        .unwrap();
        assert!(up.contains("CREATE UNIQUE INDEX"), "up.sql: {up}");
    }

    /// R6: `#[searchable]` + `#[encrypted]` is a hard `#[model]` macro error
    /// (full-text search would index ciphertext). Reject at generate time with
    /// the same explanation instead of emitting uncompilable code.
    #[test]
    fn searchable_flag_on_encrypted_field_is_rejected() {
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Account",
            &["notes:Text{encrypted}".into()],
            "20260427000000",
            &ModelOptions {
                searchable: vec!["notes".into()],
                ..ModelOptions::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("notes"), "must name the field: {msg}");
        assert!(
            msg.contains("encrypted") && msg.contains("searchable"),
            "must name both sides of the conflict: {msg}"
        );
    }

    /// R7: `#[default]` + `#[encrypted]` is a hard `#[model]` macro error (a
    /// defaulted column bypasses the encrypting insert path, so the column
    /// would hold an unencrypted value the decrypting reader then rejects).
    #[test]
    fn default_flag_on_encrypted_field_is_rejected() {
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Account",
            &["api_token:String{encrypted}".into()],
            "20260427000000",
            &ModelOptions {
                defaults: vec!["api_token=none".into()],
                ..ModelOptions::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_token"), "must name the field: {msg}");
        assert!(msg.contains("encrypted"), "must name the conflict: {msg}");
    }

    /// R12: the generated app boots but every encrypted read/write fails until
    /// key material exists, so the generator must say so — naming the command
    /// and the exact credential paths, and only mentioning `deterministic_key`
    /// when a deterministic column was actually declared.
    #[test]
    fn encrypted_model_warns_about_missing_key_material() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Account",
            &["api_token:String{encrypted}".into()],
            "20260427000000",
        )
        .unwrap();
        let warning = plan
            .warnings
            .iter()
            .find(|w| w.contains("encrypt"))
            .unwrap_or_else(|| panic!("expected an encryption warning; got {:?}", plan.warnings));
        assert!(
            warning.contains("autumn credentials edit"),
            "warning must name the command: {warning}"
        );
        assert!(
            warning.contains("active_record_encryption.primary_key"),
            "warning must name the credential: {warning}"
        );
        assert!(
            !warning.contains("deterministic_key"),
            "a randomized-only model needs no deterministic key: {warning}"
        );
    }

    #[test]
    fn deterministic_encrypted_model_warns_about_the_deterministic_key() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Account",
            &["email:String{encrypted:deterministic}".into()],
            "20260427000000",
        )
        .unwrap();
        let warning = plan
            .warnings
            .iter()
            .find(|w| w.contains("encrypt"))
            .unwrap_or_else(|| panic!("expected an encryption warning; got {:?}", plan.warnings));
        assert!(
            warning.contains("deterministic_key"),
            "a deterministic column needs the deterministic key: {warning}"
        );
    }

    #[test]
    fn unencrypted_model_emits_no_encryption_warning() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            !plan.warnings.iter().any(|w| w.contains("encrypt")),
            "got: {:?}",
            plan.warnings
        );
    }

    #[test]
    fn model_file_enum_impls_display_fromstr_tosql_fromsql() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["status:enum{draft,published}".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("impl std::fmt::Display for Status"),
            "got:\n{model}"
        );
        assert!(
            model.contains("impl std::str::FromStr for Status"),
            "got:\n{model}"
        );
        assert!(
            model.contains("must be one of draft, published"),
            "got:\n{model}"
        );
        assert!(
            model.contains(
                "impl diesel::serialize::ToSql<diesel::sql_types::Text, diesel::pg::Pg> for Status"
            ),
            "got:\n{model}"
        );
        assert!(
            model.contains("impl diesel::deserialize::FromSql<diesel::sql_types::Text, diesel::pg::Pg> for Status"),
            "got:\n{model}"
        );
        assert!(model.contains("const VARIANTS"), "got:\n{model}");
        assert!(model.contains("pub const fn as_str"), "got:\n{model}");
    }

    #[test]
    fn enum_field_name_colliding_with_model_name_is_rejected() {
        let tmp = project();
        let err = plan_model(
            tmp.path(),
            "Status",
            &["status:enum{draft,published}".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(matches!(err, GenerateError::InvalidField { .. }));
        assert!(err.to_string().contains("Status"), "got: {err}");
    }

    #[test]
    fn enum_field_name_colliding_with_generated_companion_type_is_rejected() {
        // `status:enum{...}` on a `Field` model would generate `pub enum
        // Field`, colliding with the `#[model]` macro's own `FieldField` enum
        // (one variant per mutable column, used for audit/CDC payloads).
        let tmp = project();
        let err = plan_model(
            tmp.path(),
            "Field",
            &["field:enum{a,b}".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(matches!(err, GenerateError::InvalidField { .. }));
    }

    #[test]
    fn enum_field_name_colliding_with_preload_associations_or_factory_is_rejected() {
        // The `#[model]` macro always emits `{Pascal}Preload`, `{Pascal}Associations`,
        // and `{Pascal}Factory` (autumn-macros/src/model.rs), even for a model
        // with no associations — a field that pascalizes to one of these on
        // model `Post` must be rejected, not just the shorter, already-covered
        // companion names.
        for field_name in ["post_preload", "post_associations", "post_factory"] {
            let tmp = project();
            let err = plan_model(
                tmp.path(),
                "Post",
                &[format!("{field_name}:enum{{a,b}}")],
                "20260427000000",
            )
            .unwrap_err();
            assert!(
                matches!(err, GenerateError::InvalidField { .. }),
                "expected '{field_name}' to be rejected"
            );
        }
    }

    #[test]
    fn two_enum_fields_colliding_with_each_other_are_rejected() {
        // `pascal()` is not injective: `in_review` and `in__review` both
        // pascalize to `InReview`. Without this check, both fields would
        // pass (neither collides with a *reserved* name) and the generator
        // would emit `pub enum InReview` twice in the same model file.
        let tmp = project();
        let err = plan_model(
            tmp.path(),
            "Post",
            &["in_review:enum{a,b}".into(), "in__review:enum{c,d}".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(matches!(err, GenerateError::InvalidField { .. }));
    }

    #[test]
    fn distinct_enum_fields_with_non_colliding_names_are_accepted() {
        let tmp = project();
        plan_model(
            tmp.path(),
            "Post",
            &[
                "status:enum{draft,published}".into(),
                "priority:enum{low,high}".into(),
            ],
            "20260427000000",
        )
        .unwrap();
    }

    #[test]
    fn enum_field_name_shadowing_prelude_type_is_rejected() {
        // A field that pascalizes to `String`/`Vec`/`Option`/`Result`/`Box`/`Into`
        // shadows the prelude import for the entire generated file (and, once
        // imported into the scaffold's routes file, that file too) — verified
        // by generating `string:enum{a,b}` and observing `cargo check` fail
        // with E0308/E0599 far from this token. `into` is included because
        // `render_enum_decl`'s `FromSql` impl calls the unqualified
        // `Into::into` — a field named `into` would generate `pub enum Into`,
        // which shadows that path expression too.
        for field_name in ["string", "vec", "option", "result", "box", "into"] {
            let tmp = project();
            let err = plan_model(
                tmp.path(),
                "Post",
                &[format!("{field_name}:enum{{a,b}}")],
                "20260427000000",
            )
            .unwrap_err();
            assert!(
                matches!(err, GenerateError::InvalidField { .. }),
                "expected '{field_name}' to be rejected"
            );
        }
    }

    // ── decimal field: --default (issue #1038 PR review) ────────────────────

    #[test]
    fn decimal_default_within_precision_and_scale_is_accepted() {
        let fields = parse_fields(&["price:decimal{12,2}".into()]).unwrap();
        let metadata = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["price=19.99".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            metadata.defaults().get("price").map(String::as_str),
            Some("19.99")
        );
    }

    #[test]
    fn decimal_default_rejected_when_integer_part_overflows_precision() {
        // decimal{2,2} has 0 integer digits of budget (precision - scale ==
        // 0), so any value with magnitude >= 1 must be rejected rather than
        // generating a migration Postgres fails at apply time with a
        // "numeric field overflow".
        let fields = parse_fields(&["amount:decimal{2,2}".into()]).unwrap();
        let err = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["amount=1".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("integer digit"), "got: {msg}");
    }

    #[test]
    fn decimal_default_rejected_when_fractional_part_exceeds_scale() {
        // decimal{3,2} only allows 2 fractional digits; a third significant
        // digit would silently round away rather than storing exactly.
        let fields = parse_fields(&["amount:decimal{3,2}".into()]).unwrap();
        let err = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["amount=1.999".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("fractional digit"), "got: {msg}");
    }

    #[test]
    fn decimal_default_tolerates_insignificant_trailing_zeros() {
        // "1.500" has one significant fractional digit (5), not three — it
        // must not be rejected for a scale-2 column just because the source
        // string happens to have an extra trailing zero.
        let fields = parse_fields(&["amount:decimal{4,2}".into()]).unwrap();
        let metadata = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["amount=1.500".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            metadata.defaults().get("amount").map(String::as_str),
            Some("1.500")
        );
    }

    // ── SQLite backend awareness (issue #1614) ─────────────────────────

    /// Null the database-URL environment variables so backend detection reads
    /// the temp project's `autumn.toml` deterministically (a stray real
    /// `DATABASE_URL` in the dev/CI environment would otherwise win).
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

    fn project_with_db_url(url: &str) -> TempDir {
        let tmp = project();
        fs::write(
            tmp.path().join("autumn.toml"),
            format!("[database]\nprimary_url = \"{url}\"\n"),
        )
        .unwrap();
        tmp
    }

    /// A `sqlite://` app emits `SQLite`-valid `CREATE TABLE` DDL and diesel schema
    /// types — no Postgres-only output that would break on `SQLite` (AC #4).
    #[test]
    fn sqlite_app_emits_sqlite_migration_ddl() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            let plan = plan_model(
                tmp.path(),
                "Post",
                &[
                    "title:String".into(),
                    "views:i64".into(),
                    "naive:NaiveDateTime".into(),
                ],
                "20260427000000",
            )
            .unwrap();
            plan.execute(Flags::default()).unwrap();

            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();
            assert!(
                up.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"),
                "up.sql: {up}"
            );
            assert!(up.contains("title TEXT NOT NULL"), "up.sql: {up}");
            assert!(up.contains("views INTEGER NOT NULL"), "up.sql: {up}");
            assert!(up.contains("naive TEXT NOT NULL"), "up.sql: {up}");
            assert!(
                up.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"),
                "up.sql: {up}"
            );
            // No Postgres-only DDL may leak into a SQLite migration.
            for leak in ["BIGSERIAL", "TIMESTAMPTZ", "NOW()", "JSONB", "BIGINT"] {
                assert!(!up.contains(leak), "SQLite up.sql leaked `{leak}`: {up}");
            }

            let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
            // `NaiveDateTime` uses the core, ungated `Timestamp` sql-type.
            assert!(schema.contains("naive -> Timestamp,"), "schema: {schema}");
            // This model declares only a `NaiveDateTime` field, so no
            // timestamptz sql-type of any spelling (the Postgres-only
            // `Timestamptz` or the SQLite `TimestamptzSqlite` that a
            // `DateTime<Utc>` field would emit, #1924) may appear here.
            assert!(
                !schema.contains("Timestamptz"),
                "SQLite schema.rs leaked a timestamptz sql-type: {schema}"
            );
            assert!(
                !schema.contains("Jsonb"),
                "SQLite schema.rs leaked `Jsonb`: {schema}"
            );
        });
    }

    /// The last three kinds #1924 un-rejects — `Uuid`, `Decimal`, and `Enum` —
    /// now plan cleanly on a `SQLite` app and render types that compile there.
    ///
    /// `uuid::Uuid` and `rust_decimal::Decimal` are foreign to `autumn-web`, so
    /// it can implement no diesel conversion for them; the model renders the
    /// `TEXT`-backed newtypes `autumn-web` owns instead. A generated `enum` is
    /// local to the app, so it gets `Sqlite` `ToSql`/`FromSql` impls directly.
    #[test]
    fn sqlite_app_accepts_uuid_decimal_and_enum_after_1924() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            let plan = plan_model(
                tmp.path(),
                "Post",
                &[
                    "title:String".into(),
                    "token:Uuid".into(),
                    "owner:Option<Uuid>".into(),
                    "price:decimal{10,2}".into(),
                    "status:enum{draft,published}".into(),
                ],
                "20260427000000",
            )
            .expect("Uuid/Decimal/enum fields are accepted on SQLite (#1924)");
            plan.execute(Flags::default()).unwrap();

            let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
            assert!(
                model.contains("pub token: autumn_web::db::sqlite_types::SqliteUuid,"),
                "Uuid renders as the SQLite newtype: {model}"
            );
            assert!(
                model.contains("pub owner: Option<autumn_web::db::sqlite_types::SqliteUuid>,"),
                "Option<Uuid> renders as the SQLite newtype: {model}"
            );
            assert!(
                model.contains("pub price: autumn_web::db::sqlite_types::SqliteDecimal,"),
                "Decimal renders as the SQLite newtype: {model}"
            );
            for leak in ["uuid::Uuid", "rust_decimal::Decimal"] {
                assert!(
                    !model.contains(leak),
                    "SQLite model leaked the unconvertible `{leak}`: {model}"
                );
            }

            // The generated enum carries `Sqlite` conversions, not `Pg` ones.
            assert!(
                model.contains("diesel::sqlite::Sqlite"),
                "enum must emit Sqlite ToSql/FromSql: {model}"
            );
            assert!(
                !model.contains("diesel::pg::Pg"),
                "SQLite model must not emit Pg enum conversions: {model}"
            );

            // All three store TEXT, on both the DDL and the diesel schema.
            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();
            assert!(up.contains("token TEXT NOT NULL"), "up.sql: {up}");
            assert!(up.contains("price TEXT NOT NULL"), "up.sql: {up}");
            assert!(up.contains("status TEXT NOT NULL"), "up.sql: {up}");
            assert!(
                !up.contains("NUMERIC"),
                "SQLite up.sql leaked NUMERIC: {up}"
            );

            let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
            for line in [
                "token -> Text,",
                "owner -> Nullable<Text>,",
                "price -> Text,",
                "status -> Text,",
            ] {
                assert!(schema.contains(line), "schema missing `{line}`: {schema}");
            }
        });
    }

    /// `autumn destroy` must not take the `sqlite` feature back out of a `SQLite`
    /// app. It is a whole-app backend flip, not a per-resource capability: no
    /// generated code names it, `autumn new` never writes it, and without it the
    /// app's `sqlite://` URL is refused at boot with `UnsupportedBackend`.
    /// The generic `owner_dir` rule would strip it once `src/models` empties.
    #[test]
    fn destroying_the_last_model_keeps_the_sqlite_feature() {
        with_no_db_env(|| {
            // A project that actually declares `autumn-web`, so the feature-wiring
            // pass has a dependency line to edit.
            let tmp = project_with_autumn_web_dep();
            fs::write(
                tmp.path().join("autumn.toml"),
                "[database]\nprimary_url = \"sqlite://app.db\"\n",
            )
            .unwrap();
            plan_model(
                tmp.path(),
                "Post",
                &["title:String".into()],
                "20260427000000",
            )
            .expect("plan")
            .execute(Flags::default())
            .unwrap();
            let after_generate = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
            assert!(
                after_generate.contains("features = [\"sqlite\"]"),
                "generate must add the feature first: {after_generate}"
            );

            plan_model(
                tmp.path(),
                "Post",
                &["title:String".into()],
                "20260427000001",
            )
            .expect("re-plan for revert")
            .revert(Flags::default())
            .unwrap();

            let after_destroy = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
            assert!(
                after_destroy.contains("features = [\"sqlite\"]"),
                "destroy stripped the backend flip, so the app no longer boots: {after_destroy}"
            );
        });
    }

    /// A `decimal{p,s}` column is `TEXT` on `SQLite`, so the declared precision
    /// and scale bind nothing unless the migration says so. Without the `CHECK`
    /// a repository write persists `123456.789` into a `decimal{5,2}` — the
    /// invariant Postgres gets free from `NUMERIC(5,2)` (Codex #2561, #1924).
    #[test]
    fn sqlite_decimal_column_carries_a_precision_and_scale_check() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            plan_model(
                tmp.path(),
                "Post",
                &["price:decimal{10,2}".into()],
                "20260427000000",
            )
            .expect("plan")
            .execute(Flags::default())
            .unwrap();

            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();
            assert!(up.contains("price TEXT NOT NULL CHECK ("), "up.sql: {up}");
            // The two digit budgets: 2 fractional, 10 - 2 = 8 integer.
            assert!(up.contains("<= 2"), "scale bound missing: {up}");
            assert!(up.contains("<= 8"), "precision bound missing: {up}");
        });
    }

    /// Postgres keeps the real `NUMERIC(p,s)`, which enforces this natively —
    /// no `CHECK` may appear there.
    #[test]
    fn postgres_decimal_column_has_no_check_constraint() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("postgres://localhost/app");
            plan_model(
                tmp.path(),
                "Post",
                &["price:decimal{10,2}".into()],
                "20260427000000",
            )
            .expect("plan")
            .execute(Flags::default())
            .unwrap();

            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();
            assert!(up.contains("price NUMERIC(10,2) NOT NULL"), "up.sql: {up}");
            assert!(
                !up.contains("CHECK ("),
                "Postgres must not gain a CHECK: {up}"
            );
        });
    }

    /// A `decimal` default must reach `SQLite` as a quoted, NORMALIZED text
    /// literal. Unquoted,
    /// `SQLite` evaluates `DEFAULT 0.10` numerically and TEXT affinity stores
    /// `0.1`; a wide value becomes scientific notation, which `Decimal::from_str`
    /// cannot read back at all (Codex #2561, #1924).
    #[test]
    fn sqlite_decimal_default_is_a_quoted_text_literal() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            plan_model(
                tmp.path(),
                "Post",
                &["price:decimal{10,2}".into()],
                "20260427000000",
            )
            .map(|_| ())
            .expect("plan without default");

            let options = ModelOptions {
                defaults: vec!["price=0.10".to_owned()],
                ..Default::default()
            };
            let fields = parse_fields(&["price:decimal{10,2}".into()]).unwrap();

            let sqlite = parse_model_metadata_for(DatabaseBackend::Sqlite, &fields, &options)
                .expect("sqlite metadata");
            assert_eq!(
                sqlite.defaults().get("price").map(String::as_str),
                Some("'0.1'"),
                "a SQLite decimal default must be quoted AND normalized — the same \
                 text `SqliteDecimal` writes, or a row holding its own default \
                 would not match a `find_by_…` for that value"
            );

            let postgres = parse_model_metadata_for(DatabaseBackend::Postgres, &fields, &options)
                .expect("postgres metadata");
            assert_eq!(
                postgres.defaults().get("price").map(String::as_str),
                Some("0.10"),
                "Postgres decimal defaults stay unquoted numeric literals"
            );
        });
    }

    /// A `SQLite` app's `Cargo.toml` must describe the `SQLite` backend: diesel
    /// on its `sqlite` feature with the bundled `libsqlite3-sys`, no `pq-sys`,
    /// and `autumn-web`'s `sqlite` feature — otherwise nothing the generator
    /// emits can compile, whatever the field kinds (issue #1924).
    #[test]
    fn sqlite_app_cargo_deps_target_the_sqlite_backend() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            plan_model(
                tmp.path(),
                "Post",
                &["title:String".into(), "price:decimal{10,2}".into()],
                "20260427000000",
            )
            .expect("plan")
            .execute(Flags::default())
            .unwrap();

            let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
            assert!(
                cargo.contains("features = [\"sqlite\"]") || cargo.contains("\"sqlite\""),
                "autumn-web must carry the sqlite feature: {cargo}"
            );
            assert!(
                cargo.contains("libsqlite3-sys"),
                "the bundled SQLite amalgamation must be a dependency: {cargo}"
            );
            assert!(
                !cargo.contains("pq-sys"),
                "a SQLite app must not name libpq as a direct dependency: {cargo}"
            );
            assert!(
                !cargo.contains("db-diesel2-postgres"),
                "rust_decimal's Postgres diesel feature is wrong here: {cargo}"
            );
        });
    }

    /// A Postgres app's `Cargo.toml` keeps the historical Postgres dependency
    /// set — the backend-aware split must not leak `SQLite` into it.
    #[test]
    fn postgres_app_cargo_deps_are_unchanged() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("postgres://localhost/app");
            plan_model(
                tmp.path(),
                "Post",
                &["title:String".into(), "price:decimal{10,2}".into()],
                "20260427000000",
            )
            .expect("plan")
            .execute(Flags::default())
            .unwrap();

            let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
            assert!(cargo.contains("pq-sys"), "Cargo.toml: {cargo}");
            assert!(cargo.contains("db-diesel2-postgres"), "Cargo.toml: {cargo}");
            assert!(
                !cargo.contains("libsqlite3-sys"),
                "a Postgres app must not link SQLite: {cargo}"
            );
        });
    }

    /// Byte-parity guard for the un-rejection: the same model on a Postgres app
    /// still renders `uuid::Uuid` / `rust_decimal::Decimal` and Postgres-only
    /// enum conversions (issue #1614 AC #10).
    #[test]
    fn postgres_app_model_output_for_uuid_decimal_enum_is_unchanged() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("postgres://localhost/app");
            plan_model(
                tmp.path(),
                "Post",
                &[
                    "token:Uuid".into(),
                    "price:decimal{10,2}".into(),
                    "status:enum{draft,published}".into(),
                ],
                "20260427000000",
            )
            .expect("plan")
            .execute(Flags::default())
            .unwrap();

            let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
            assert!(model.contains("pub token: uuid::Uuid,"), "model: {model}");
            assert!(
                model.contains("pub price: rust_decimal::Decimal,"),
                "model: {model}"
            );
            assert!(
                model.contains("diesel::pg::Pg"),
                "Postgres enum conversions must be unchanged: {model}"
            );
            assert!(
                !model.contains("sqlite_types"),
                "Postgres model must not name the SQLite newtypes: {model}"
            );
        });
    }

    /// A `SQLite` app now ACCEPTS `DateTime<Utc>` and `Attachment` fields at
    /// generate time (issue #1924): `DateTime<Utc>` maps to diesel's
    /// `TimestamptzSqlite` sql-type and `Attachment` (`Blob`) rides
    /// `autumn-web`'s local `Text`/`Sqlite` conversion. This is the un-rejection
    /// half of the contract — these tokens must plan cleanly (no `Config`
    /// error), and the emitted `SQLite` schema/DDL must use the right types.
    #[test]
    fn sqlite_app_accepts_datetime_and_attachment_after_1924() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            let plan = plan_model(
                tmp.path(),
                "Post",
                &[
                    "title:String".into(),
                    "at:DateTime".into(),
                    "cover:Attachment".into(),
                ],
                "20260427000000",
            )
            .expect("DateTime + Attachment fields are accepted on SQLite (#1924)");
            plan.execute(Flags::default()).unwrap();

            // SQLite DDL: DateTime and Attachment both store as TEXT.
            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();
            assert!(up.contains("at TEXT NOT NULL"), "up.sql: {up}");
            // `Attachment` is nullable-by-default (Option<Blob>).
            assert!(up.contains("cover TEXT"), "up.sql: {up}");
            for leak in ["TIMESTAMPTZ", "JSONB", "NUMERIC"] {
                assert!(!up.contains(leak), "SQLite up.sql leaked `{leak}`: {up}");
            }

            // schema.rs: DateTime -> TimestamptzSqlite, Attachment -> Nullable<Text>.
            let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
            assert!(
                schema.contains("at -> TimestamptzSqlite,"),
                "schema: {schema}"
            );
            assert!(
                schema.contains("cover -> Nullable<Text>,"),
                "schema: {schema}"
            );
            assert!(
                !schema.contains("Jsonb"),
                "SQLite schema.rs leaked `Jsonb`: {schema}"
            );
        });
    }

    /// Regression guard: those same field kinds are unchanged on a Postgres app
    /// — the diesel-conversion gate is `SQLite`-only.
    #[test]
    fn postgres_app_field_kinds_without_sqlite_conversion_are_unchanged() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("postgres://localhost/app");
            plan_model(
                tmp.path(),
                "Post",
                &[
                    "title:String".into(),
                    "token:Uuid".into(),
                    "cover:Attachment".into(),
                    "price:decimal{10,2}".into(),
                    "at:DateTime".into(),
                    "status:enum{draft,published}".into(),
                ],
                "20260427000000",
            )
            .expect("Uuid/Attachment/Decimal/DateTime/enum fields must still generate on Postgres");
        });
    }

    /// `generate model --id uuid` on a `SQLite` app is rejected at generate time
    /// citing #1905 (issue #1614 AC #4): `SQLite` has no `gen_random_uuid()` and
    /// the generated `New*` insert type omits `#[id]` fields, so a `TEXT PRIMARY
    /// KEY` column would accept NULL/omitted ids. App-side UUID generation is
    /// deferred to the runtime slice #1905.
    #[test]
    fn sqlite_app_uuid_primary_key_is_rejected_citing_1905() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            let err = plan_model_with_options(
                tmp.path(),
                "Post",
                &["title:String".into()],
                "20260427000000",
                &ModelOptions {
                    id_type: IdType::Uuid,
                    ..Default::default()
                },
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                matches!(err, GenerateError::Config(_)),
                "expected Config error, got: {err:?}"
            );
            assert!(msg.contains("1905"), "must cite issue #1905: {msg}");
            assert!(
                msg.contains("SQLite") && msg.contains("UUID"),
                "message must be actionable: {msg}"
            );
        });
    }

    /// The default INTEGER primary key still works on a `SQLite` app — only the
    /// uuid key is gated, so `generate model` without `--id` is unaffected.
    #[test]
    fn sqlite_app_integer_primary_key_still_works() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            let plan = plan_model_with_options(
                tmp.path(),
                "Post",
                &["title:String".into()],
                "20260427000000",
                &ModelOptions::default(),
            )
            .expect("default INTEGER primary key must still generate on SQLite");
            plan.execute(Flags::default()).unwrap();
            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();
            assert!(
                up.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"),
                "up.sql: {up}"
            );
        });
    }

    /// A `--sharded` model on a `SQLite` app is rejected at generate time as
    /// Postgres-only (issue #1614 AC #4): a sharded resource needs a
    /// `[[database.shards]]` topology, which config validation rejects for a
    /// `SQLite` primary, so no valid `SQLite` config could use it. Unlike FTS /
    /// UUID / unsupported field kinds this is a PERMANENT constraint, so the
    /// message must NOT cite a "coming soon" issue.
    #[test]
    fn sharded_on_sqlite_app_is_rejected_as_postgres_only() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            let err = plan_model_with_options(
                tmp.path(),
                "Post",
                &["title:String".into(), "tenant_id:i64".into()],
                "20260427000000",
                &ModelOptions {
                    sharded: true,
                    shard_key: Some("tenant_id".into()),
                    ..Default::default()
                },
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                matches!(err, GenerateError::Config(_)),
                "expected Config error, got: {err:?}"
            );
            assert!(
                msg.contains("Postgres") && msg.contains("--sharded"),
                "message must point at the Postgres backend requirement: {msg}"
            );
            assert!(
                msg.contains("single-writer"),
                "message must convey the permanent single-host/single-writer constraint: {msg}"
            );
            assert!(
                !msg.contains("issues/"),
                "sharding is permanent, not deferred — must not cite a coming-soon issue: {msg}"
            );
        });
    }

    /// The default (non-sharded) `generate model` path is unaffected on a
    /// `SQLite` app — only `--sharded` is gated.
    #[test]
    fn non_sharded_sqlite_model_still_works() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            plan_model_with_options(
                tmp.path(),
                "Post",
                &["title:String".into()],
                "20260427000000",
                &ModelOptions::default(),
            )
            .expect("non-sharded SQLite model must still generate");
        });
    }

    /// Regression guard: a `--sharded` model on a Postgres app is unchanged —
    /// the sharded gate is `SQLite`-only.
    #[test]
    fn postgres_app_sharded_model_is_unchanged() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("postgres://localhost/app");
            plan_model_with_options(
                tmp.path(),
                "Post",
                &["title:String".into(), "tenant_id:i64".into()],
                "20260427000000",
                &ModelOptions {
                    sharded: true,
                    shard_key: Some("tenant_id".into()),
                    ..Default::default()
                },
            )
            .expect("sharded model must still generate on Postgres");
        });
    }

    /// Regression guard: `generate model --id uuid` on a Postgres app is
    /// unchanged — the uuid gate is `SQLite`-only.
    #[test]
    fn postgres_app_uuid_primary_key_is_unchanged() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("postgres://localhost/app");
            let plan = plan_model_with_options(
                tmp.path(),
                "Post",
                &["title:String".into()],
                "20260427000000",
                &ModelOptions {
                    id_type: IdType::Uuid,
                    ..Default::default()
                },
            )
            .expect("uuid primary key must still generate on Postgres");
            plan.execute(Flags::default()).unwrap();
            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();
            assert!(
                up.contains("id UUID PRIMARY KEY DEFAULT gen_random_uuid()"),
                "up.sql: {up}"
            );
        });
    }

    /// Regression guard: a `postgres://` app and an app with no `autumn.toml`
    /// (backend defaults to Postgres) must both produce the historical,
    /// byte-for-byte-identical Postgres output.
    #[test]
    fn postgres_app_generation_is_unchanged_regression_guard() {
        with_no_db_env(|| {
            let fields = &[
                "title:String".into(),
                "views:i64".into(),
                "at:DateTime".into(),
            ];

            let pg = project_with_db_url("postgres://localhost/app");
            plan_model(pg.path(), "Post", fields, "20260427000000")
                .unwrap()
                .execute(Flags::default())
                .unwrap();
            let pg_up = fs::read_to_string(
                pg.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();

            let none = project();
            plan_model(none.path(), "Post", fields, "20260427000000")
                .unwrap()
                .execute(Flags::default())
                .unwrap();
            let none_up = fs::read_to_string(
                none.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();

            assert_eq!(
                pg_up, none_up,
                "explicit postgres:// and default (no autumn.toml) must be byte-identical"
            );
            assert!(
                pg_up.contains("id BIGSERIAL PRIMARY KEY"),
                "up.sql: {pg_up}"
            );
            assert!(pg_up.contains("views BIGINT NOT NULL"), "up.sql: {pg_up}");
            assert!(pg_up.contains("at TIMESTAMPTZ NOT NULL"), "up.sql: {pg_up}");
            assert!(
                pg_up.contains("created_at TIMESTAMP NOT NULL DEFAULT NOW()"),
                "up.sql: {pg_up}"
            );
        });
    }

    /// FTS on a `SQLite` app now emits an FTS5 external-content virtual table +
    /// maintenance triggers (issue #1910) instead of being rejected — and no
    /// Postgres-only `tsvector`/GIN DDL leaks into the `SQLite` migration.
    #[test]
    fn searchable_on_sqlite_app_emits_fts5() {
        with_no_db_env(|| {
            let tmp = project_with_db_url("sqlite://app.db");
            let plan = plan_model_with_options(
                tmp.path(),
                "Post",
                &["title:String".into(), "body:Text".into()],
                "20260427000000",
                &ModelOptions {
                    searchable: vec!["title".into(), "body".into()],
                    ..Default::default()
                },
            )
            .expect("searchable SQLite model plans without rejection");
            plan.execute(Flags::default()).unwrap();

            let up = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_create_posts/up.sql"),
            )
            .unwrap();
            let down = fs::read_to_string(
                tmp.path()
                    .join("migrations/20260427000000_create_posts/down.sql"),
            )
            .unwrap();

            // External-content FTS5 virtual table over the searchable columns.
            assert!(
                up.contains(
                    "CREATE VIRTUAL TABLE \"posts__fts\" USING fts5(\"title\", \"body\", \
                     content='posts', content_rowid='id', tokenize='unicode61');"
                ),
                "up.sql must create the FTS5 vtable: {up}"
            );
            // Maintenance triggers keep the index in sync.
            for trig in ["posts__fts_ai", "posts__fts_ad", "posts__fts_au"] {
                assert!(up.contains(trig), "up.sql must create trigger {trig}: {up}");
            }
            assert!(
                up.contains("INSERT INTO \"posts__fts\"(\"posts__fts\") VALUES('rebuild');"),
                "up.sql must backfill via 'rebuild': {up}"
            );
            // down.sql drops the triggers and the FTS table.
            assert!(
                down.contains("DROP TABLE IF EXISTS \"posts__fts\";"),
                "down.sql must drop the FTS table: {down}"
            );
            for trig in ["posts__fts_ai", "posts__fts_ad", "posts__fts_au"] {
                assert!(
                    down.contains(&format!("DROP TRIGGER IF EXISTS \"{trig}\";")),
                    "down.sql must drop trigger {trig}: {down}"
                );
            }
            // No Postgres-only FTS DDL may leak into the SQLite migration.
            for leak in ["tsvector", "to_tsvector", "USING gin", "search_vector"] {
                assert!(!up.contains(leak), "SQLite up.sql leaked `{leak}`: {up}");
            }
        });
    }

    /// Issue #1319: the repository macro's `search_page` is hardcoded to an
    /// `i64`/`BigInt` primary key (`SearchId { id: i64 }`, `Vec<i64>`,
    /// `id.eq_any(&ids)`, `HashMap<i64, _>`), so pairing `--searchable` with a
    /// non-i64 (uuid) key would emit a repository that fails to compile.
    /// `parse_model_metadata` rejects the combination directly, independent of
    /// the scaffold command's broader uuid gate.
    #[test]
    fn searchable_with_uuid_primary_key_is_rejected() {
        let fields = parse_fields(&["title:String".into(), "body:Text".into()]).unwrap();
        let err = parse_model_metadata(
            &fields,
            &ModelOptions {
                searchable: vec!["title".into(), "body".into()],
                id_type: IdType::Uuid,
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, GenerateError::Config(_)),
            "expected Config error, got: {err:?}"
        );
        assert!(
            msg.contains("--searchable")
                && (msg.contains("i64") || msg.contains("bigint"))
                && msg.contains("uuid::Uuid"),
            "error must explain the i64-primary-key requirement and name the uuid type: {msg}"
        );
    }

    /// Issue #1319: a model field named `search_vector` collides with the
    /// generated `tsvector` column the FTS migration adds, so pairing it with
    /// `--searchable` is rejected up front. (Field names parse as lowercase
    /// `snake_case`, so the guard's `eq_ignore_ascii_case` is defensive; the
    /// reachable case is a lowercase `search_vector` field.)
    #[test]
    fn searchable_with_search_vector_field_is_rejected() {
        let fields = parse_fields(&["title:String".into(), "search_vector:String".into()]).unwrap();
        let err = parse_model_metadata(
            &fields,
            &ModelOptions {
                searchable: vec!["title".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, GenerateError::Config(_)),
            "expected Config error, got: {err:?}"
        );
        assert!(
            msg.contains("search_vector") && msg.contains("reserved"),
            "error must flag `search_vector` as reserved: {msg}"
        );
    }

    /// A `search_vector` field WITHOUT `--searchable` is harmless and accepted
    /// (the name is only reserved for full-text-search models).
    #[test]
    fn search_vector_field_without_searchable_is_accepted() {
        let fields = parse_fields(&["title:String".into(), "search_vector:String".into()]).unwrap();
        let metadata = parse_model_metadata(&fields, &ModelOptions::default())
            .expect("search_vector without --searchable must be accepted");
        assert!(metadata.searchable().is_empty());
    }

    /// A uuid primary key WITHOUT `--searchable` stays valid (the guard only
    /// fires when full-text search is requested).
    #[test]
    fn uuid_primary_key_without_searchable_is_accepted() {
        let fields = parse_fields(&["title:String".into()]).unwrap();
        let metadata = parse_model_metadata(
            &fields,
            &ModelOptions {
                id_type: IdType::Uuid,
                ..Default::default()
            },
        )
        .expect("uuid without --searchable must be accepted");
        assert!(metadata.searchable().is_empty());
    }

    #[test]
    fn decimal_default_tolerates_leading_zeros_in_integer_part() {
        let fields = parse_fields(&["amount:decimal{2,2}".into()]).unwrap();
        let metadata = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["amount=0.5".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            metadata.defaults().get("amount").map(String::as_str),
            Some("0.5")
        );
    }

    #[test]
    fn decimal_default_accepts_negative_value_within_range() {
        let fields = parse_fields(&["amount:decimal{2,2}".into()]).unwrap();
        let metadata = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["amount=-0.75".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            metadata.defaults().get("amount").map(String::as_str),
            Some("-0.75")
        );
    }

    #[test]
    fn decimal_default_rejects_non_numeric_value() {
        let fields = parse_fields(&["price:decimal".into()]).unwrap();
        let err = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["price=abc".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("valid numbers"));
    }

    #[test]
    fn decimal_default_rejects_scientific_notation() {
        let fields = parse_fields(&["price:decimal".into()]).unwrap();
        let err = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["price=1e2".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("scientific notation"));
    }

    #[test]
    fn decimal_default_rejects_nan_and_infinity() {
        let fields = parse_fields(&["price:decimal".into()]).unwrap();
        for bad in ["nan", "inf", "infinity"] {
            let err = parse_model_metadata(
                &fields,
                &ModelOptions {
                    defaults: vec![format!("price={bad}")],
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("finite"),
                "'{bad}' should be rejected as non-finite: {err}"
            );
        }
    }

    #[test]
    fn decimal_field_warns_when_existing_rust_decimal_dep_lacks_diesel_feature() {
        // Regression test (PR review, issue #1038): `ensure_cargo_dependencies`
        // skips a crate that's already declared, regardless of its features —
        // so a project that already had `rust_decimal = "1"` (e.g. for its own
        // business logic) before ever using a `decimal` field would silently
        // keep that feature-less entry, and the generated `#[model]` field's
        // Diesel `ToSql`/`FromSql` impls (behind `db-diesel2-postgres`) would
        // be missing, failing to compile with no indication why.
        let tmp = project();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nrust_decimal = \"1\"\n",
        )
        .unwrap();
        let plan = plan_model(
            tmp.path(),
            "Product",
            &["price:decimal".into()],
            "20260427000000",
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
        assert!(plan.warnings[0].contains("rust_decimal"));
        assert!(plan.warnings[0].contains("db-diesel2-postgres"));
    }

    #[test]
    fn decimal_field_no_warning_when_existing_rust_decimal_dep_already_has_feature() {
        let tmp = project();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\n\
             rust_decimal = { version = \"1\", features = [\"db-diesel2-postgres\", \"serde\"] }\n",
        )
        .unwrap();
        let plan = plan_model(
            tmp.path(),
            "Product",
            &["price:decimal".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(plan.warnings.is_empty(), "warnings: {:?}", plan.warnings);
    }

    #[test]
    fn decimal_field_warns_naming_only_the_missing_feature() {
        // Regression test (PR review, issue #1038): the earlier version of
        // this check only verified `db-diesel2-postgres`, missing that the
        // generated `#[model]` struct also derives Serialize/Deserialize and
        // so needs `serde` too — a project with `rust_decimal` already
        // declared with `db-diesel2-postgres` but not `serde` would pass the
        // old single-feature check and still fail to compile.
        let tmp = project();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\n\
             rust_decimal = { version = \"1\", features = [\"db-diesel2-postgres\"] }\n",
        )
        .unwrap();
        let plan = plan_model(
            tmp.path(),
            "Product",
            &["price:decimal".into()],
            "20260427000000",
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
        assert!(plan.warnings[0].contains("serde"));
    }

    #[test]
    fn decimal_field_no_warning_when_rust_decimal_not_already_declared() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Product",
            &["price:decimal".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(plan.warnings.is_empty(), "warnings: {:?}", plan.warnings);
    }

    // ── enum field: --default (issue #1030) ─────────────────────────────────

    #[test]
    fn enum_sql_default_literal_quotes_variant() {
        let fields = parse_fields(&["status:enum{draft,published,archived}".into()]).unwrap();
        let metadata = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["status=draft".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            metadata.defaults().get("status").map(String::as_str),
            Some("'draft'")
        );
    }

    #[test]
    fn enum_sql_default_literal_accepts_quoted_variant() {
        // `--default status="draft"`/`status='draft'` must unquote the same
        // way the String/Text arm does (shared `unquote_default_value`).
        let fields = parse_fields(&["status:enum{draft,published}".into()]).unwrap();
        for token in ["status=\"draft\"", "status='draft'"] {
            let metadata = parse_model_metadata(
                &fields,
                &ModelOptions {
                    defaults: vec![token.into()],
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                metadata.defaults().get("status").map(String::as_str),
                Some("'draft'"),
                "token '{token}' should unquote to 'draft'"
            );
        }
    }

    #[test]
    fn enum_default_unknown_variant_errors_at_generate_time() {
        let fields = parse_fields(&["status:enum{draft,published,archived}".into()]).unwrap();
        let err = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["status=bogus".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "got: {msg}");
        assert!(msg.contains("draft"), "got: {msg}");
        assert!(msg.contains("published"), "got: {msg}");
        assert!(msg.contains("archived"), "got: {msg}");
    }

    #[test]
    fn unique_field_with_default_is_rejected() {
        // issue #1032 review follow-up: a `--default` field is excluded from
        // the generated HTML form (see `scaffold::plan_scaffold`'s
        // `form_fields` filter), so a `unique` column that also has a
        // `--default` would have no `UNIQUE_CONSTRAINTS` entry (and, even if
        // it did, no form input to show a duplicate-value error against).
        // Reject the combination outright instead of silently emitting a
        // scaffold whose duplicate handling doesn't work for that field.
        let fields = parse_fields(&["email:String:unique".into()]).unwrap();
        let err = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["email='a@b.com'".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("email"), "got: {msg}");
        assert!(msg.contains("unique"), "got: {msg}");
        assert!(msg.contains("default"), "got: {msg}");
    }

    #[test]
    fn unique_flag_field_with_default_is_rejected() {
        // Same rejection, but for the `--unique FIELD` flag path rather than
        // the inline `:unique` DSL marker — `apply_unique_flags` must run
        // before `parse_model_metadata` sees the `--default` token for this
        // to catch it (it does, in `plan_model_with_options`).
        let mut fields = parse_fields(&["email:String".into()]).unwrap();
        apply_unique_flags(&mut fields, &["email".to_owned()]).unwrap();
        let err = parse_model_metadata(
            &fields,
            &ModelOptions {
                defaults: vec!["email='a@b.com'".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, GenerateError::InvalidField { .. }));
    }

    #[test]
    fn enum_default_emits_rust_default_impl_and_sql_default() {
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Post",
            &["status:enum{draft,published,archived}".into()],
            "20260427000000",
            &ModelOptions {
                defaults: vec!["status=draft".into()],
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("#[derive(")
                && model.contains("Default")
                && model.contains("pub enum Status"),
            "expected Default in the enum's derive list: {model}"
        );
        assert!(
            model.contains("#[default]\n    #[serde(rename = \"draft\")]\n    Draft,"),
            "expected #[default] on the Draft variant: {model}"
        );
        assert!(
            model.contains("#[default]\n    pub status: Status,"),
            "field-level #[default] marker must still be emitted so status is \
             excluded from NewPost: {model}"
        );

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(up.contains("DEFAULT 'draft'"), "got:\n{up}");
    }

    // ── enum field: nullable (issue #1030) ──────────────────────────────────

    #[test]
    fn nullable_enum_model_field_is_option() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["status:Option<enum{draft,published}>".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("pub status: Option<Status>,"),
            "got:\n{model}"
        );

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(up.contains("status TEXT NULL"), "got:\n{up}");
        assert!(
            up.contains("CHECK (status IN ('draft', 'published'))"),
            "got:\n{up}"
        );
    }

    #[test]
    fn execute_writes_idiomatic_model() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into(), "published:bool".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(model.contains("#[autumn_web::model]"));
        assert!(model.contains("pub struct Post"));
        assert!(model.contains("pub title: String,"));
        assert!(model.contains("pub published: bool,"));
        assert!(model.contains("#[id]"));
        assert!(model.contains("pub id: i64,"));
        assert!(model.contains("created_at: chrono::NaiveDateTime"));

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(up.contains("CREATE TABLE posts ("));
        assert!(up.contains("title TEXT NOT NULL"));
        assert!(up.contains("published BOOLEAN NOT NULL"));
        assert!(up.contains("id BIGSERIAL PRIMARY KEY"));

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(schema.contains("posts (id)"));
        assert!(schema.contains("title -> Text,"));
    }

    #[test]
    fn rerunning_with_force_overwrites_model_but_appends_schema() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        // Second run: same model, --force.
        let plan2 = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427100000",
        )
        .unwrap();
        plan2
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        // Only one `posts (id)` block — append is idempotent.
        assert_eq!(schema.matches("posts (id)").count(), 1);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = project();
        let plan = plan_model(tmp.path(), "Post", &[], "20260427000000").unwrap();
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(!tmp.path().join("src/models/post.rs").exists());
        assert!(!tmp.path().join("src/schema.rs").exists());
    }

    #[test]
    fn collision_reports_clean_path() {
        let tmp = project();
        // Pre-create the file so the next run collides.
        let dir = tmp.path().join("src/models");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("post.rs"), "// existing").unwrap();
        let plan = plan_model(tmp.path(), "Post", &[], "20260427000000").unwrap();
        let err = plan.execute(Flags::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("post.rs"));
    }

    #[test]
    fn modify_actions_marked_correctly() {
        let tmp = project();
        let plan = plan_model(tmp.path(), "Post", &[], "20260427000000").unwrap();
        let modify_count = plan
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Modify { .. }))
            .count();
        // mod.rs, schema.rs, and Cargo.toml are always Modify.
        assert!(modify_count >= 3);
    }

    #[test]
    fn ensure_cargo_dependencies_appends_missing() {
        let original = "[package]\n\
name = \"x\"\n\
\n\
[dependencies]\n\
autumn-web = \"0.3\"\n";
        let updated = ensure_cargo_dependencies(
            original,
            &[
                ("chrono", "\"0.4\""),
                ("autumn-web", "\"99\""), // already present — must not duplicate
            ],
        );
        assert!(updated.contains("autumn-web = \"0.3\""));
        assert!(updated.contains("chrono = \"0.4\""));
        assert_eq!(updated.matches("autumn-web =").count(), 1);
    }

    #[test]
    fn remove_cargo_dependencies_restores_original() {
        let original = "[package]\n\
name = \"x\"\n\
\n\
[dependencies]\n\
autumn-web = \"0.3\"\n";
        let updated = ensure_cargo_dependencies(original, &[("chrono", "\"0.4\"")]);
        assert_ne!(updated, original);
        let reverted = remove_cargo_dependencies(&updated, &["chrono"]);
        assert_eq!(reverted, original);
    }

    #[test]
    fn remove_cargo_dependencies_removes_only_named_crates() {
        let original = "[dependencies]\nautumn-web = \"0.3\"\n";
        let updated =
            ensure_cargo_dependencies(original, &[("chrono", "\"0.4\""), ("serde", "\"1\"")]);
        let reverted = remove_cargo_dependencies(&updated, &["chrono"]);
        assert!(!reverted.contains("chrono"));
        assert!(reverted.contains("serde = \"1\""));
        assert!(reverted.contains("autumn-web = \"0.3\""));
    }

    #[test]
    fn remove_cargo_dependencies_is_idempotent_when_absent() {
        let original = "[dependencies]\nautumn-web = \"0.3\"\n";
        assert_eq!(remove_cargo_dependencies(original, &["chrono"]), original);
    }

    #[test]
    fn remove_cargo_dependencies_does_not_touch_prefix_sharing_crate() {
        let original = "[dependencies]\ndiesel-async = \"0.8\"\n";
        assert_eq!(remove_cargo_dependencies(original, &["diesel"]), original);
    }

    #[test]
    fn remove_cargo_dependencies_collapses_now_empty_section_created_from_scratch() {
        // Mirrors `ensure_cargo_dependencies`'s "no [dependencies] section
        // yet" branch: a minimal project with none at all.
        let original = "[package]\nname = \"x\"\n";
        let updated = ensure_cargo_dependencies(original, &[("chrono", "\"0.4\"")]);
        assert_ne!(updated, original);
        let reverted = remove_cargo_dependencies(&updated, &["chrono"]);
        assert_eq!(reverted, original);
    }

    #[test]
    fn ensure_cargo_dependencies_idempotent() {
        let original = "[package]\nname = \"x\"\n\n[dependencies]\nchrono = \"0.4\"\n";
        let once = ensure_cargo_dependencies(original, &[("chrono", "\"0.4\"")]);
        let twice = ensure_cargo_dependencies(&once, &[("chrono", "\"0.4\"")]);
        assert_eq!(once, twice);
        assert_eq!(once, original);
    }

    #[test]
    fn ensure_cargo_dependencies_inserts_before_next_section() {
        let original = "[package]\nname = \"x\"\n\n\
[dependencies]\nautumn-web = \"0.3\"\n\n\
[dev-dependencies]\ntempfile = \"3\"\n";
        let updated = ensure_cargo_dependencies(original, &[("chrono", "\"0.4\"")]);
        let chrono_pos = updated.find("chrono = \"0.4\"").unwrap();
        let dev_deps_pos = updated.find("[dev-dependencies]").unwrap();
        assert!(
            chrono_pos < dev_deps_pos,
            "chrono must land in [dependencies], not [dev-dependencies]"
        );
    }

    #[test]
    fn ensure_cargo_dependencies_treats_array_of_tables_as_boundary() {
        // `[[bin]]` is an array-of-tables header — it must terminate the
        // `[dependencies]` block. Without this, generated deps land *inside*
        // the `[[bin]]` entry and Cargo silently ignores them.
        let original = "[package]\nname = \"x\"\n\n\
[dependencies]\nautumn-web = \"0.3\"\n\n\
[[bin]]\nname = \"app\"\npath = \"src/main.rs\"\n";
        let updated = ensure_cargo_dependencies(original, &[("chrono", "\"0.4\"")]);
        let chrono_pos = updated.find("chrono = \"0.4\"").unwrap();
        let bin_pos = updated.find("[[bin]]").unwrap();
        assert!(
            chrono_pos < bin_pos,
            "chrono must land in [dependencies], not inside [[bin]]:\n{updated}"
        );
    }

    #[test]
    fn ensure_cargo_dependencies_recognises_subtable_form() {
        // `[dependencies.chrono]` is the table-form way to declare a dep.
        // The scanner must treat that header as part of `[dependencies]`,
        // not as the next section, AND must recognise that `chrono` is
        // already declared so we don't duplicate it.
        let original = "[package]\nname = \"x\"\n\n\
[dependencies]\nautumn-web = \"0.3\"\n\n\
[dependencies.chrono]\nversion = \"0.4\"\nfeatures = [\"serde\"]\n";
        let updated = ensure_cargo_dependencies(
            original,
            &[
                ("chrono", "\"99\""), // already declared via subtable — must not duplicate
                ("diesel", "\"2\""),
            ],
        );
        // `chrono` already declared via [dependencies.chrono] — must not be
        // re-added in shorthand form.
        assert!(
            !updated.contains("chrono = \"99\""),
            "[dependencies.chrono] subtable form must count as 'chrono is declared':\n{updated}"
        );
        // `diesel` was missing and should land inside [dependencies] — i.e.
        // before the [dependencies.chrono] subtable header.
        let diesel_pos = updated.find("diesel = \"2\"").unwrap();
        let chrono_subtable_pos = updated.find("[dependencies.chrono]").unwrap();
        assert!(
            diesel_pos < chrono_subtable_pos,
            "new dep must land inside [dependencies], above any [dependencies.X] subtable:\n{updated}"
        );
    }

    #[test]
    fn dep_subtable_crate_name_parses_canonical_form() {
        assert_eq!(
            dep_subtable_crate_name("[dependencies.chrono]"),
            Some("chrono")
        );
        assert_eq!(
            dep_subtable_crate_name("  [dependencies.chrono] # opt"),
            Some("chrono")
        );
        // Non-dependency tables, dev-deps, and bare `[dependencies]` are not
        // subtable forms.
        assert_eq!(dep_subtable_crate_name("[dependencies]"), None);
        assert_eq!(dep_subtable_crate_name("[dev-dependencies.chrono]"), None);
        assert_eq!(dep_subtable_crate_name("[package]"), None);
        assert_eq!(dep_subtable_crate_name("[[bin]]"), None);
    }

    #[test]
    fn ensure_cargo_dependencies_skips_commented_out_entries() {
        let original = "[dependencies]\n# autumn-web = \"0.2\"\n";
        let updated = ensure_cargo_dependencies(original, &[("autumn-web", "\"0.3\"")]);
        assert!(updated.contains("autumn-web = \"0.3\""));
    }

    #[test]
    fn ensure_cargo_dependencies_handles_header_with_trailing_comment() {
        // `[dependencies] # shared deps` is valid TOML — must not be treated
        // as a missing section.
        let original = "[dependencies] # shared deps\nautumn-web = \"0.3\"\n";
        let updated = ensure_cargo_dependencies(original, &[("chrono", "\"0.4\"")]);
        // No second `[dependencies]` table appended.
        assert_eq!(
            updated.matches("[dependencies]").count(),
            1,
            "duplicate [dependencies] table appended:\n{updated}"
        );
        assert!(updated.contains("chrono = \"0.4\""));
    }

    #[test]
    fn ensure_cargo_dependencies_treats_indented_section_as_a_header() {
        // Indented headers are accepted by cargo and our scanner mustn't
        // treat them as bare dep entries.
        let original = "[package]\nname = \"x\"\n\n[dependencies]\nautumn-web = \"0.3\"\n\n  [dev-dependencies]\ntempfile = \"3\"\n";
        let updated = ensure_cargo_dependencies(original, &[("chrono", "\"0.4\"")]);
        let chrono_pos = updated.find("chrono = \"0.4\"").unwrap();
        let dev_deps_pos = updated.find("[dev-dependencies]").unwrap();
        assert!(
            chrono_pos < dev_deps_pos,
            "chrono must land in [dependencies], not [dev-dependencies]"
        );
    }

    #[test]
    fn existing_dep_version_below_detects_old_base64() {
        // Shorthand and inline-table forms below 0.22 → true.
        assert!(existing_dep_version_below(
            "[dependencies]\nbase64 = \"0.13\"\n",
            "base64",
            0,
            22
        ));
        assert!(existing_dep_version_below(
            "[dependencies]\nbase64 = { version = \"0.20\" }\n",
            "base64",
            0,
            22
        ));
        // Leading caret/comparator is tolerated.
        assert!(existing_dep_version_below(
            "[dependencies]\nbase64 = \"^0.21\"\n",
            "base64",
            0,
            22
        ));
        // At or above the floor → no warning.
        assert!(!existing_dep_version_below(
            "[dependencies]\nbase64 = \"0.22\"\n",
            "base64",
            0,
            22
        ));
        assert!(!existing_dep_version_below(
            "[dependencies]\nbase64 = \"0.22.1\"\n",
            "base64",
            0,
            22
        ));
        assert!(!existing_dep_version_below(
            "[dependencies]\nbase64 = \"1.0\"\n",
            "base64",
            0,
            22
        ));
        // Absent crate → no warning.
        assert!(!existing_dep_version_below(
            "[dependencies]\nserde = \"1\"\n",
            "base64",
            0,
            22
        ));
        // `[dependencies.base64]` subtable with an old `version` key → warn.
        assert!(existing_dep_version_below(
            "[dependencies]\n[dependencies.base64]\nversion = \"0.13\"\n",
            "base64",
            0,
            22
        ));
        // Subtable under a populated `[dependencies]` table, old version → warn.
        assert!(existing_dep_version_below(
            "[dependencies]\nserde = \"1\"\n\n[dependencies.base64]\nversion = \"0.13\"\nfeatures = [\"std\"]\n",
            "base64",
            0,
            22
        ));
        // `[dependencies.base64]` subtable at/above the floor → no warning.
        assert!(!existing_dep_version_below(
            "[dependencies]\n[dependencies.base64]\nversion = \"0.22\"\n",
            "base64",
            0,
            22
        ));
        // Subtable with NO `version` key → undeterminable → warn conservatively.
        assert!(existing_dep_version_below(
            "[dependencies]\n[dependencies.base64]\nworkspace = true\n",
            "base64",
            0,
            22
        ));
        // Shorthand workspace inheritance is undeterminable → warn (the crate is
        // declared, but we can't prove it's new enough).
        assert!(existing_dep_version_below(
            "[dependencies]\nbase64 = { workspace = true }\n",
            "base64",
            0,
            22
        ));
        // A commented-out old pin must not trip the check (crate is absent).
        assert!(!existing_dep_version_below(
            "[dependencies]\n# base64 = \"0.13\"\n",
            "base64",
            0,
            22
        ));
    }

    #[test]
    fn warn_if_existing_dep_below_version_records_one_warning() {
        let mut plan = Plan::new(std::path::PathBuf::from("."));
        warn_if_existing_dep_below_version(
            &mut plan,
            "[dependencies]\nbase64 = \"0.13\"\n",
            "base64",
            0,
            22,
        );
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
        assert!(plan.warnings[0].contains("base64"));

        let mut plan = Plan::new(std::path::PathBuf::from("."));
        warn_if_existing_dep_below_version(
            &mut plan,
            "[dependencies]\nbase64 = \"0.22\"\n",
            "base64",
            0,
            22,
        );
        assert!(plan.warnings.is_empty(), "warnings: {:?}", plan.warnings);

        // Subtable pin below the floor fires exactly one warning.
        let mut plan = Plan::new(std::path::PathBuf::from("."));
        warn_if_existing_dep_below_version(
            &mut plan,
            "[dependencies]\n[dependencies.base64]\nversion = \"0.13\"\n",
            "base64",
            0,
            22,
        );
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
        assert!(plan.warnings[0].contains("base64"));
    }

    #[test]
    fn plan_includes_cargo_toml_modification() {
        let tmp = project();
        let plan = plan_model(tmp.path(), "Post", &[], "20260427000000").unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("Cargo.toml")),
            "plan must touch Cargo.toml so generated code compiles"
        );
    }

    #[test]
    fn execute_adds_chrono_and_diesel_to_cargo_toml() {
        let tmp = TempDir::new().unwrap();
        // Realistic `autumn new` Cargo.toml.
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n\n\
[dependencies]\nautumn-web = \"0.3\"\n",
        )
        .unwrap();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let cargo_toml = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        for dep in [
            "chrono",
            "diesel",
            "diesel-async",
            "serde",
            "serde_json",
            "diesel_migrations",
        ] {
            assert!(
                cargo_toml.contains(&format!("{dep} =")),
                "missing '{dep}' in Cargo.toml after `generate model`:\n{cargo_toml}"
            );
        }
    }

    #[test]
    fn execute_adds_uuid_dependencies_for_uuid_fields() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n\n\
[dependencies]\nautumn-web = \"0.3\"\n",
        )
        .unwrap();

        let plan = plan_model(
            tmp.path(),
            "ApiToken",
            &["token:Uuid".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let cargo_toml = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo_toml.contains("uuid = { version = \"1\", features = [\"serde\"] }"),
            "uuid::Uuid fields need a direct uuid dependency with serde support:\n{cargo_toml}"
        );
        assert!(
            cargo_toml.contains(
                "diesel = { version = \"2\", features = [\"postgres\", \"chrono\", \"uuid\"] }"
            ),
            "Diesel schema Uuid fields need diesel's uuid feature:\n{cargo_toml}"
        );
    }

    // ── Soft-delete model generation (issue #689) ─────────────────

    #[test]
    fn plan_model_with_soft_delete_emits_deleted_at_migration_column() {
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ModelOptions {
                soft_delete: true,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("deleted_at"),
            "soft_delete migration must include deleted_at column: {up}"
        );
        assert!(
            up.contains("NULL"),
            "soft_delete deleted_at must be nullable (no NOT NULL): {up}"
        );
    }

    #[test]
    fn plan_model_with_soft_delete_emits_deleted_at_field_in_struct() {
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ModelOptions {
                soft_delete: true,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("deleted_at"),
            "soft_delete model struct must include deleted_at field: {model}"
        );
        assert!(
            model.contains("Option<"),
            "soft_delete deleted_at field must be Option<...>: {model}"
        );
    }

    #[test]
    fn plan_model_without_soft_delete_does_not_emit_deleted_at() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            !model.contains("deleted_at"),
            "model without soft_delete must not contain deleted_at: {model}"
        );
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(
            !up.contains("deleted_at"),
            "migration without soft_delete must not contain deleted_at: {up}"
        );
    }

    #[test]
    fn plan_model_soft_delete_rejects_explicit_deleted_at_field() {
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into(), "deleted_at:NaiveDateTime".into()],
            "20260427000000",
            &ModelOptions {
                soft_delete: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("deleted_at"),
            "providing deleted_at with soft_delete must error; got: {msg}"
        );
    }

    #[test]
    fn plan_model_soft_delete_schema_includes_deleted_at_column() {
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ModelOptions {
                soft_delete: true,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("deleted_at"),
            "schema.rs must include deleted_at column when soft_delete is enabled: {schema}"
        );
    }

    // ── sharding tests ─────────────────────────────────────────────────────

    #[test]
    fn model_emits_shard_key_attr() {
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Account",
            &["tenant_id:i64".into(), "name:String".into()],
            "20260427000000",
            &ModelOptions {
                sharded: true,
                shard_key: Some("tenant_id".into()),
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/account.rs")).unwrap();
        assert!(
            model.contains("#[shard_key = \"tenant_id\"]"),
            "sharded model must emit #[shard_key] attribute: {model}"
        );
    }

    #[test]
    fn model_no_shard_key_attr_when_not_sharded() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            !model.contains("shard_key"),
            "non-sharded model must not emit shard_key: {model}"
        );
    }

    #[test]
    fn migration_notes_shard_target_when_sharded() {
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Account",
            &["tenant_id:i64".into()],
            "20260427000000",
            &ModelOptions {
                sharded: true,
                shard_key: Some("tenant_id".into()),
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let up_sql = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_accounts/up.sql"),
        )
        .unwrap();
        assert!(
            up_sql.contains("autumn migrate --shard"),
            "sharded migration up.sql must note autumn migrate --shard: {up_sql}"
        );
        assert!(
            up_sql.contains("control DB"),
            "sharded migration up.sql must note control DB default: {up_sql}"
        );
    }

    #[test]
    fn migration_no_shard_comment_when_not_sharded() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let up_sql = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(
            !up_sql.contains("autumn migrate --shard"),
            "non-sharded migration must not have shard comment: {up_sql}"
        );
    }

    // ── IdType (issue #1400) ───────────────────────────────────────────────

    #[test]
    fn plan_default_id_type_emits_bigserial_and_i64() {
        // AC4: the default (BigSerial) must be byte-for-byte identical to today's output.
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ModelOptions::default(),
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("pub id: i64,"),
            "default must emit i64: {model}"
        );

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("id BIGSERIAL PRIMARY KEY"),
            "default must emit BIGSERIAL: {up}"
        );

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("id -> Int8,"),
            "default schema must emit Int8: {schema}"
        );
    }

    #[test]
    fn plan_uuid_id_type_emits_uuid_type_in_all_outputs() {
        // AC1: --id uuid threads through model, migration, and schema.
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
            &ModelOptions {
                id_type: IdType::Uuid,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("pub id: uuid::Uuid,"),
            "uuid must emit uuid::Uuid: {model}"
        );
        assert!(
            !model.contains("pub id: i64"),
            "uuid model must not contain i64: {model}"
        );

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("id UUID PRIMARY KEY DEFAULT gen_random_uuid()"),
            "uuid migration: {up}"
        );
        assert!(
            !up.contains("BIGSERIAL"),
            "uuid migration must not contain BIGSERIAL: {up}"
        );

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("id -> Uuid,"),
            "uuid schema must emit Uuid type: {schema}"
        );
        assert!(
            !schema.contains("id -> Int8"),
            "uuid schema must not contain Int8: {schema}"
        );
    }

    #[test]
    fn plan_uuid_id_migration_has_uuidv7_comment() {
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Post",
            &[],
            "20260427000000",
            &ModelOptions {
                id_type: IdType::Uuid,
                ..Default::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("UUIDv7"),
            "uuid migration must document UUIDv7 upgrade path: {up}"
        );
    }

    #[test]
    fn uuid_dep_always_present_in_model_deps() {
        // AC5: the uuid crate is always in MODEL_DEPS regardless of --id.
        let uuid_dep = MODEL_DEPS.iter().find(|(k, _)| *k == "uuid");
        assert!(
            uuid_dep.is_some(),
            "MODEL_DEPS must always include the uuid crate (AC5)"
        );
        let (_, spec) = uuid_dep.unwrap();
        assert!(
            spec.contains("serde"),
            "uuid dep must include serde feature"
        );
    }

    #[test]
    fn fk_field_uuid_generates_uuid_column() {
        // AC3: a field like `author_id:Uuid` already works via FieldKind::Uuid.
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["author_id:Uuid".into(), "body:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/comment.rs")).unwrap();
        assert!(
            model.contains("pub author_id: uuid::Uuid,"),
            "FK Uuid field: {model}"
        );

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_comments/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("author_id UUID NOT NULL"),
            "FK Uuid migration: {up}"
        );
    }

    // ── references field type (issue #1026) ────────────────────────────────

    #[test]
    fn references_field_emits_i64_struct_field_fk_constraint_and_index() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["body:Text".into(), "post:references".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/comment.rs")).unwrap();
        assert!(
            model.contains("pub post_id: i64,"),
            "references field must render as i64: {model}"
        );

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_comments/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("post_id BIGINT NOT NULL REFERENCES posts(id)"),
            "up.sql must emit the FK column + constraint: {up}"
        );
        assert!(
            up.contains("CREATE INDEX idx_comments_post_id ON comments (post_id);"),
            "up.sql must emit an automatic FK index: {up}"
        );

        let down = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_comments/down.sql"),
        )
        .unwrap();
        assert!(
            down.contains("DROP TABLE comments"),
            "down.sql drops the whole table, cleanly reversing the FK/index/column: {down}"
        );

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("post_id -> Int8,"),
            "schema.rs must use Int8 for the FK column: {schema}"
        );
    }

    #[test]
    fn nullable_references_field_emits_option_i64() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references?".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/comment.rs")).unwrap();
        assert!(
            model.contains("pub post_id: Option<i64>,"),
            "nullable references field must render as Option<i64>: {model}"
        );

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_comments/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("post_id BIGINT NULL REFERENCES posts(id)"),
            "{up}"
        );
    }

    #[test]
    fn references_field_warns_when_target_model_is_missing() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
        assert!(plan.warnings[0].contains("post"));
        assert!(plan.warnings[0].contains("posts"));
    }

    #[test]
    fn references_field_no_warning_when_target_model_exists() {
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(models_dir.join("post.rs"), "// existing Post model\n").unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            plan.warnings.is_empty(),
            "no warning expected once src/models/post.rs exists: {:?}",
            plan.warnings
        );
    }

    #[test]
    fn no_references_field_means_no_warnings() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn references_field_errors_when_target_model_has_uuid_pk() {
        // `references` only supports the i64/BIGSERIAL PK convention; a FK
        // column typed BIGINT against a UUID PRIMARY KEY would fail at
        // `autumn migrate` time with an opaque Postgres error, so this must
        // fail loudly at generate time instead.
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: uuid::Uuid,\n}\n",
        )
        .unwrap();

        let err = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("post"), "error should name the field: {msg}");
        assert!(
            msg.contains("UUID"),
            "error should explain the UUID PK mismatch: {msg}"
        );
    }

    #[test]
    fn references_field_no_error_when_target_model_has_bigserial_pk() {
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: i64,\n}\n",
        )
        .unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn references_field_no_warning_when_target_declared_in_single_file_models_rs() {
        // The single-file `src/models.rs` layout is an equally valid place
        // for a model to live (see `migration.rs`'s `AddSearch` shape, which
        // checks both locations) — it must not produce a false-positive
        // "model not found" warning.
        let tmp = project();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/models.rs"),
            "use crate::schema::posts;\n\n#[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: i64,\n}\n",
        )
        .unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            plan.warnings.is_empty(),
            "no warning expected once Post is declared in src/models.rs: {:?}",
            plan.warnings
        );
    }

    #[test]
    fn references_field_no_warning_for_grouped_schema_import_in_single_file_models_rs() {
        // Multi-model `src/models.rs` files commonly group their schema
        // imports (e.g. `examples/reddit-clone/src/models.rs`:
        // `use crate::schema::{comments, posts, subreddits, users, votes};`)
        // rather than one `use` per table — this must still count as "Post
        // declared here", not a false "model not found" warning.
        let tmp = project();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/models.rs"),
            "use crate::schema::{comments, posts, subreddits, users, votes};\n\n\
             #[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: i64,\n}\n",
        )
        .unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            plan.warnings.is_empty(),
            "grouped schema import must be recognized: {:?}",
            plan.warnings
        );
    }

    #[test]
    fn references_field_detects_uuid_pk_with_grouped_schema_import() {
        // Same grouped-import layout, but the target model has a UUID PK —
        // the hard error must still fire, not be silently skipped.
        let tmp = project();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/models.rs"),
            "use crate::schema::{comments, posts};\n\n\
             #[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: uuid::Uuid,\n}\n",
        )
        .unwrap();

        let err = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn references_field_grouped_schema_import_not_confused_by_table_name_prefix() {
        // Only `posts_tags` is grouped-imported — `posts` must not match.
        let tmp = project();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/models.rs"),
            "use crate::schema::{posts_tags, users};\n\n\
             #[autumn_web::model]\npub struct PostsTag {\n    #[id]\n    pub id: i64,\n}\n",
        )
        .unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert_eq!(
            plan.warnings.len(),
            1,
            "'posts_tags' in a grouped import must not satisfy a reference to 'posts': {:?}",
            plan.warnings
        );
    }

    #[test]
    fn references_field_no_warning_for_multiline_grouped_schema_import() {
        // rustfmt commonly wraps long grouped imports across lines.
        let tmp = project();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/models.rs"),
            "use crate::schema::{\n    comments,\n    posts,\n    users,\n};\n\n\
             #[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: i64,\n}\n",
        )
        .unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            plan.warnings.is_empty(),
            "multi-line grouped schema import must be recognized: {:?}",
            plan.warnings
        );
    }

    #[test]
    fn references_field_still_warns_when_single_file_models_rs_lacks_the_model() {
        // An unrelated `src/models.rs` (e.g. only declaring `User`) must not
        // be mistaken for a `Post` declaration.
        let tmp = project();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/models.rs"),
            "use crate::schema::users;\n\n#[autumn_web::model]\npub struct User {\n    #[id]\n    pub id: i64,\n}\n",
        )
        .unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1, "warnings: {:?}", plan.warnings);
    }

    #[test]
    fn references_field_not_confused_by_table_name_prefix_in_single_file_models_rs() {
        // Only `posts_tags`/`PostsTag` is declared — `posts` must not be
        // treated as found just because it's a string-prefix of
        // `posts_tags` (regression: word-boundary-unaware substring match).
        let tmp = project();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/models.rs"),
            "use crate::schema::posts_tags;\n\n#[autumn_web::model]\npub struct PostsTag {\n    #[id]\n    pub id: i64,\n}\n",
        )
        .unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert_eq!(
            plan.warnings.len(),
            1,
            "'posts_tags' must not satisfy a reference to 'posts': {:?}",
            plan.warnings
        );
    }

    #[test]
    fn references_field_detects_uuid_pk_even_when_id_field_is_renamed() {
        // The `#[model]` macro identifies the primary key by the `#[id]`
        // attribute, not by the field name `id` — a hand-edited model file
        // (the generator's own doc comment says these are "ordinary user
        // code" once generated) can legally rename it.
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[id]\n    pub uid: uuid::Uuid,\n}\n",
        )
        .unwrap();

        let err = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn references_field_not_confused_by_an_unrelated_field_literally_named_id() {
        // The real primary key is `pk: i64`; an unrelated field happens to be
        // named `id` and typed `uuid::Uuid`. Only the `#[id]`-tagged field
        // should be inspected.
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[id]\n    pub pk: i64,\n    pub id: uuid::Uuid,\n}\n",
        )
        .unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            plan.warnings.is_empty(),
            "an unrelated field named 'id' must not trigger a false UUID-PK error: {:?}",
            plan.warnings
        );
    }

    #[test]
    fn references_field_detects_unqualified_uuid_type() {
        // A hand-edited model using `use uuid::Uuid;` + `pub id: Uuid,`
        // (idiomatic, not what the generator itself emits, but a common
        // hand-edit) must still be detected as a UUID primary key.
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "use uuid::Uuid;\n\n#[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: Uuid,\n}\n",
        )
        .unwrap();

        let err = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn references_field_detects_uuid_pk_through_intervening_attribute() {
        // An attribute (or doc comment) between `#[id]` and the field
        // declaration — e.g. `#[serde(rename = "id")]` — must not be
        // mistaken for the field line itself.
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[id]\n    #[serde(rename = \"id\")]\n    pub id: uuid::Uuid,\n}\n",
        )
        .unwrap();

        let err = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn references_field_detects_uuid_pk_through_intervening_doc_comment() {
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[id]\n    /// The primary key.\n    pub id: uuid::Uuid,\n}\n",
        )
        .unwrap();

        let err = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn references_field_detects_uuid_pk_with_trailing_inline_comment() {
        // `pub id: uuid::Uuid, // primary key` — the trailing comment must
        // not get swept into the extracted type text (which would make it
        // compare unequal to "uuid::Uuid" and miss the UUID PK).
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: uuid::Uuid, // primary key\n}\n",
        )
        .unwrap();

        let err = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn references_field_detects_uuid_pk_with_trailing_block_comment() {
        // `pub id: uuid::Uuid, /* primary key */` — a block comment, not a
        // line comment. The syn-based parser isn't fooled by either.
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[id]\n    pub id: uuid::Uuid, /* primary key */\n}\n",
        )
        .unwrap();

        let err = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn references_field_detects_uuid_pk_regardless_of_attribute_order() {
        // `#[id]` doesn't have to be the first attribute on the field — a
        // real parse (unlike a "look at the line after #[id]" scan) handles
        // any attribute order the same way rustc does.
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.rs"),
            "#[autumn_web::model]\npub struct Post {\n    #[serde(rename = \"id\")]\n    #[id]\n    pub id: uuid::Uuid,\n}\n",
        )
        .unwrap();

        let err = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
    }

    #[test]
    fn references_field_handles_unparseable_model_file_gracefully() {
        // A model file that isn't valid Rust (or uses a struct name that
        // doesn't match) must not panic — "can't verify" falls back to no
        // error, same as if the model couldn't be found at all.
        let tmp = project();
        let models_dir = tmp.path().join("src/models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(models_dir.join("post.rs"), "this is not valid rust {{{").unwrap();

        let plan = plan_model(
            tmp.path(),
            "Comment",
            &["post:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(plan.warnings.is_empty());
    }

    // ── self-referential `references` (issue #1026 follow-up) ──────────────

    #[test]
    fn self_referential_references_errors_when_own_id_is_uuid() {
        // `Category category:references --id uuid`: the target model
        // ("Category" itself) doesn't exist on disk yet — this command is
        // creating it — so a filesystem lookup can never see it. The
        // self-reference must be checked against the model's own
        // `--id uuid` directly instead of silently skipping the UUID check.
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Category",
            &["name:String".into(), "category:references".into()],
            "20260427000000",
            &ModelOptions {
                id_type: IdType::Uuid,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"));
        assert!(err.to_string().contains("self-referential"));
    }

    #[test]
    fn self_referential_references_fine_with_default_bigserial_id() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Category",
            &["name:String".into(), "category:references".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            plan.warnings.is_empty(),
            "a self-reference to the table being created now must not warn \
             'model not found': {:?}",
            plan.warnings
        );
    }

    #[test]
    fn self_referential_references_emits_correct_fk_sql() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Category",
            &["name:String".into(), "category:references".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_categories/up.sql"),
        )
        .unwrap();
        assert!(up.contains("category_id BIGINT NOT NULL REFERENCES categories(id)"));
    }

    // ── optimistic locking: `lock_version` (issue #1318) ────────────────────
    //
    // A model opts into optimistic concurrency by declaring a field literally
    // named `lock_version`. The generator wires the framework's shipped
    // primitive (`#[lock_version]`, issue #575) rather than leaving it an inert
    // integer column.

    #[test]
    fn lock_version_field_emits_lock_version_attribute() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i32".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("#[lock_version]\n    pub lock_version: i32,"),
            "a `lock_version` column must carry the framework's `#[lock_version]` \
             attribute so `#[repository]` update raises RepositoryError::Conflict: {model}"
        );
    }

    #[test]
    fn lock_version_column_gets_sql_default_zero() {
        // `#[lock_version]` excludes the column from `NewPost`, so the INSERT
        // omits it — without a SQL DEFAULT every create would fail on the
        // NOT NULL constraint.
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i32".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_posts/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("lock_version INTEGER NOT NULL DEFAULT 0"),
            "got:\n{up}"
        );
    }

    #[test]
    fn position_field_emits_position_attribute() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Task",
            &["title:String".into(), "rank:position".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/task.rs")).unwrap();
        assert!(
            model.contains("#[position]\n    pub rank: i64,"),
            "a `position` column must carry the framework's `#[position]` attribute so it is \
             excluded from New/UpdateTask: {model}"
        );
    }

    #[test]
    fn position_column_gets_sql_default_zero() {
        // `#[position]` excludes the column from `NewTask`, so the INSERT
        // omits it — without a SQL DEFAULT every create would fail on the
        // NOT NULL constraint before the repository's insert hook overwrites
        // the placeholder with the real next-in-scope value.
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Task",
            &["title:String".into(), "rank:position".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_tasks/up.sql"),
        )
        .unwrap();
        assert!(up.contains("rank BIGINT NOT NULL DEFAULT 0"), "got:\n{up}");
    }

    #[test]
    fn lock_version_bigint_is_also_supported() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i64".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(
            model.contains("#[lock_version]\n    pub lock_version: i64,"),
            "got:\n{model}"
        );
    }

    #[test]
    fn model_without_lock_version_emits_no_lock_version_attribute() {
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(!model.contains("lock_version"), "got:\n{model}");
    }

    #[test]
    fn lock_version_with_non_integer_type_is_rejected() {
        // Silently ignoring the field would leave the author believing they
        // opted into optimistic locking when they did not.
        let tmp = project();
        let err = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:String".into()],
            "20260427000000",
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("lock_version") && msg.contains("i32"),
            "the error must name the field and the supported types: {msg}"
        );
    }

    #[test]
    fn a_model_with_no_insertable_columns_is_rejected() {
        // Every column DB-managed => an empty `NewPost`, whose Diesel
        // `Insertable` derive does not compile. `generate model` reaches this
        // as easily as `generate scaffold` did, and the scaffold delegates here,
        // so the guard belongs on this path.
        let tmp = project();
        let err = plan_model(
            tmp.path(),
            "Post",
            &["lock_version:i32".into()],
            "20260427000000",
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("no insertable columns") && msg.contains("NewPost"),
            "got: {msg}"
        );

        // The mixed case: two columns declared, none left after `--default`
        // drops one and `#[lock_version]` drops the other.
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i32".into()],
            "20260427000000",
            &ModelOptions {
                defaults: vec!["title=x".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("no insertable columns"),
            "got: {err}"
        );

        // One ordinary column alongside is enough.
        let tmp = project();
        plan_model(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i32".into()],
            "20260427000000",
        )
        .expect("a lock column plus an ordinary column must generate");
    }

    #[test]
    fn destroying_a_legacy_lock_version_model_is_never_blocked_by_the_new_checks() {
        // Same hazard as the scaffold gates: `destroy model` recomputes the plan
        // it is about to revert, so a generation-only refusal would fire before
        // `Plan::revert` ever sees `--force`, permanently stranding the files.
        for cols in [
            vec!["title:String".to_owned(), "lock_version:String".to_owned()],
            vec![
                "title:String".to_owned(),
                "lock_version:Option<i32>".to_owned(),
            ],
            vec!["lock_version:i32".to_owned()],
        ] {
            let tmp = project();
            let plan = plan_model_with_options_for_revert(
                tmp.path(),
                "Post",
                &cols,
                "20260427000000",
                &ModelOptions::default(),
            );
            assert!(
                plan.is_ok(),
                "destroying a legacy model with {cols:?} must plan: {:?}",
                plan.err()
            );
        }

        // Structural errors still apply on the revert path — without a valid
        // field list there is no plan to revert at all.
        let tmp = project();
        assert!(
            plan_model_with_options_for_revert(
                tmp.path(),
                "Post",
                &["title:NotAType".to_owned()],
                "20260427000000",
                &ModelOptions::default(),
            )
            .is_err(),
            "a malformed field list must still fail on the revert path"
        );
    }

    #[test]
    fn a_lock_version_seeded_at_its_ceiling_is_rejected() {
        // The generated UPDATE increments the column in SQL, and Postgres raises
        // `integer out of range` rather than wrapping — verified against
        // Postgres 16 — so seeding at the maximum makes the FIRST save on every
        // row a 500, not a distant theoretical overflow.
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i32".into()],
            "20260427000000",
            &ModelOptions {
                defaults: vec!["lock_version=2147483647".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("2147483647") && msg.contains("i64"),
            "the refusal must name the ceiling and the way out: {msg}"
        );

        // `i64` has its own, much higher ceiling.
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i64".into()],
            "20260427000000",
            &ModelOptions {
                defaults: vec!["lock_version=9223372036854775807".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("9223372036854775807"),
            "got: {err}"
        );

        // An i32 ceiling is fine on an i64 column, and any seed below the
        // ceiling is fine on either — the counter can still be incremented.
        for (ty, seed) in [
            ("lock_version:i64", "lock_version=2147483647"),
            ("lock_version:i32", "lock_version=2147483646"),
            ("lock_version:i32", "lock_version=5"),
        ] {
            let tmp = project();
            plan_model_with_options(
                tmp.path(),
                "Post",
                &["title:String".into(), ty.into()],
                "20260427000000",
                &ModelOptions {
                    defaults: vec![seed.into()],
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| panic!("{ty} seeded {seed} must be accepted: {e}"));
        }
    }

    #[test]
    fn unique_lock_version_is_rejected() {
        // The column is DB-managed and defaults to 0 on every insert, so a
        // unique index on it would reject the second row ever created — and
        // the `--default` + `unique` guard above never sees the pairing,
        // because the lock column's default is injected after it runs.
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i32".into()],
            "20260427000000",
            &ModelOptions {
                uniques: vec!["lock_version".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("lock_version") && msg.contains("unique"),
            "got: {msg}"
        );
    }

    #[test]
    fn lock_version_emits_a_plan_warning_so_the_opt_in_is_never_silent() {
        // `lock_version` is a magic name: declaring it changes what the column
        // *is*. Someone who wanted an ordinary counter must be told.
        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:i32".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("optimistic locking") && w.contains("Rename")),
            "expected an opt-in warning naming the escape hatch: {:?}",
            plan.warnings
        );

        let tmp = project();
        let plan = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into()],
            "20260427000000",
        )
        .unwrap();
        assert!(
            !plan.warnings.iter().any(|w| w.contains("lock")),
            "a model without the column must not warn: {:?}",
            plan.warnings
        );
    }

    #[test]
    fn nullable_lock_version_is_rejected() {
        // `Option<i32>`, not `i32?` — the latter is not this DSL's nullable
        // spelling, so it fails in the type parser and never reaches the
        // optimistic-locking guard this test is about.
        let tmp = project();
        let err = plan_model(
            tmp.path(),
            "Post",
            &["title:String".into(), "lock_version:Option<i32>".into()],
            "20260427000000",
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("lock_version") && msg.contains("non-nullable"),
            "the nullability guard must be what rejects it: {msg}"
        );
    }

    /// Review finding (AC6 hole): `--index` is the third spelling of "make this
    /// column equality-queryable". A B-tree index over randomized ciphertext
    /// can never serve a lookup, so it is pure write amplification that also
    /// advertises a queryability the column does not have.
    #[test]
    fn index_flag_on_randomized_encrypted_field_is_rejected() {
        let tmp = project();
        let err = plan_model_with_options(
            tmp.path(),
            "Account",
            &["api_token:String{encrypted}".into()],
            "20260427000000",
            &ModelOptions {
                indexes: vec!["api_token".into()],
                ..ModelOptions::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_token"), "must name the field: {msg}");
        assert!(
            msg.contains("deterministic"),
            "must point at the fix: {msg}"
        );
    }

    /// …but on a DETERMINISTIC column an index is exactly what makes the mode
    /// worth its equality-leakage cost, so it is allowed.
    #[test]
    fn index_flag_on_deterministic_encrypted_field_is_allowed() {
        let tmp = project();
        let plan = plan_model_with_options(
            tmp.path(),
            "Account",
            &["email:String{encrypted:deterministic}".into()],
            "20260427000000",
            &ModelOptions {
                indexes: vec!["email".into()],
                ..ModelOptions::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let up = fs::read_to_string(
            tmp.path()
                .join("migrations/20260427000000_create_accounts/up.sql"),
        )
        .unwrap();
        assert!(
            up.contains("CREATE INDEX idx_accounts_email ON accounts (email);"),
            "up.sql: {up}"
        );
    }

    /// Review finding: the shard is chosen by hashing the column value, which
    /// is ciphertext on disk — unusable for a randomized column, and a
    /// plaintext-equality leak at the topology level for a deterministic one.
    #[test]
    fn shard_key_on_an_encrypted_field_is_rejected() {
        for mode in ["encrypted", "encrypted:deterministic"] {
            let tmp = project();
            let err = plan_model_with_options(
                tmp.path(),
                "Account",
                &[format!("tenant:String{{{mode}}}")],
                "20260427000000",
                &ModelOptions {
                    sharded: true,
                    shard_key: Some("tenant".into()),
                    ..ModelOptions::default()
                },
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("tenant"), "must name the field: {msg}");
            assert!(msg.contains("shard-key"), "must name the flag: {msg}");
        }
    }

    /// The generation-only encryption refusals must NOT fire while `destroy`
    /// recomputes the plan it is about to revert — that would strand exactly
    /// the files the user asked to delete, before `Plan::revert` ever sees
    /// `--force`. (Same posture as `validate_lock_version_field`.)
    #[test]
    fn destroy_recompute_skips_the_generation_only_encryption_refusals() {
        let tmp = project();
        let options = ModelOptions {
            uniques: vec!["api_token".into()],
            ..ModelOptions::default()
        };
        // Generating this is refused…
        assert!(
            plan_model_with_options(
                tmp.path(),
                "Account",
                &["api_token:String{encrypted}".into()],
                "20260427000000",
                &options,
            )
            .is_err()
        );
        // …but recomputing it for a destroy must still produce a plan.
        assert!(
            plan_model_with_options_for_revert(
                tmp.path(),
                "Account",
                &["api_token:String{encrypted}".into()],
                "20260427000000",
                &options,
            )
            .is_ok(),
            "destroy must be able to recompute a plan it is about to revert"
        );
    }
}
