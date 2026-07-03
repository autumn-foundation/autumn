//! `autumn generate model` — emit a `#[model]` struct, its migration, and a
//! `schema.rs` table block.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::dsl::{Field, FieldKind, IdType, parse_fields};
use super::emit::Plan;
use super::naming::{pascal, pluralize, snake};
use super::schema_edit::{
    add_mod_declaration, append_schema_table_with_id, create_table_sql_with_metadata_and_id,
    drop_table_sql,
};
use super::{GenerateError, ensure_project_root, read_or_empty};

/// Optional metadata applied to generated model fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelOptions {
    /// Field names that should receive `#[indexed]` and SQL indexes.
    pub indexes: Vec<String>,
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMetadata {
    indexes: BTreeSet<String>,
    validations: BTreeMap<String, Vec<String>>,
    defaults: BTreeMap<String, String>,
}

impl ModelMetadata {
    #[must_use]
    pub fn has_validator_rules(&self) -> bool {
        !self.validations.is_empty()
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
    ensure_project_root(project_root)?;
    validate_resource_name(name)?;
    let fields = parse_fields(field_tokens)?;
    validate_field_names(&fields)?;
    let pascal_name = pascal(name);
    validate_enum_field_collisions(&pascal_name, &fields)?;
    let metadata = parse_model_metadata(&fields, options)?;

    let snake_name = snake(name);
    let table = pluralize(&snake_name);

    // When soft_delete is enabled, append a virtual `deleted_at` field so
    // the SQL migration and schema.rs block include the nullable column.
    let schema_fields = augment_fields_for_soft_delete(&fields, options.soft_delete)?;

    let mut plan = Plan::new(project_root);
    check_reference_targets(
        &mut plan,
        project_root,
        &fields,
        &table,
        Some(options.id_type),
    )?;

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
        ),
    );

    let mod_path = models_dir.join("mod.rs");
    let mod_existing = read_or_empty(&mod_path);
    plan.modify(mod_path, add_mod_declaration(&mod_existing, &snake_name));

    // (b) Diesel migration
    let migration_dir_name = format!("{timestamp}_create_{table}");
    let migration_dir = project_root.join("migrations").join(&migration_dir_name);
    let table_sql = create_table_sql_with_metadata_and_id(
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
    plan.create(migration_dir.join("up.sql"), up_sql);
    plan.create(migration_dir.join("down.sql"), drop_table_sql(&table));

    // (c) `src/schema.rs` entry
    let schema_path = project_root.join("src").join("schema.rs");
    let schema_existing = read_or_empty(&schema_path);
    plan.modify(
        schema_path,
        append_schema_table_with_id(&schema_existing, &table, &schema_fields, options.id_type),
    );

    // (d) `Cargo.toml` deps — `#[autumn_web::model]` expands to references
    // for `diesel`, `serde`, `serde_json`, `chrono`, and supported field crates
    // such as `uuid`, none of which are in the freshly-`autumn new`-ed project.
    let mut deps: Vec<(&str, &str)> = MODEL_DEPS.to_vec();
    if metadata.has_validator_rules() {
        deps.push((
            "validator",
            "{ version = \"0.20\", features = [\"derive\"] }",
        ));
    }
    plan_cargo_deps(&mut plan, project_root, &deps);

    Ok(plan)
}

/// Whether resource `base`'s model can be found anywhere in the project,
/// checking both layouts the codebase already treats as valid (see
/// `migration.rs`'s `AddSearch` migration shape, which checks the same two
/// locations): the per-resource `src/models/<base>.rs` (mere existence
/// counts — it's this resource's own file, however it's written), and the
/// single-file `src/models.rs` (only counts if its content actually declares
/// the resource's table, via a word-boundary-aware check so `posts` isn't
/// confused with a longer table name like `posts_tags`).
fn model_file_exists(project_root: &Path, table: &str, base: &str) -> bool {
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
    });
    Ok(std::borrow::Cow::Owned(augmented))
}

/// Direct dependencies the *model* generator's output requires at compile time.
pub(super) const MODEL_DEPS: &[(&str, &str)] = &[
    ("chrono", "{ version = \"0.4\", features = [\"serde\"] }"),
    (
        "diesel",
        "{ version = \"2\", features = [\"postgres\", \"chrono\", \"uuid\"] }",
    ),
    (
        "diesel-async",
        "{ version = \"0.8\", features = [\"postgres\"] }",
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

/// Append a `Modify` action to `plan` that ensures every `(crate, version_spec)`
/// in `deps` is present under `[dependencies]` in the project's `Cargo.toml`.
/// Existing entries are left untouched.
pub(super) fn plan_cargo_deps(plan: &mut Plan, project_root: &Path, deps: &[(&str, &str)]) {
    let cargo_toml_path = project_root.join("Cargo.toml");
    let existing = read_or_empty(&cargo_toml_path);
    let updated = ensure_cargo_dependencies(&existing, deps);
    if updated != existing {
        plan.modify(cargo_toml_path, updated);
    }
}

/// Insert each `(crate, version_spec)` pair at the end of the `[dependencies]`
/// section, skipping entries already present. Pure string transformation —
/// preserves the rest of the file as-is. If the file has no `[dependencies]`
/// section yet, appends a new one with the requested entries.
pub(super) fn ensure_cargo_dependencies(existing: &str, deps: &[(&str, &str)]) -> String {
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

    // We split two concerns here:
    // 1. The "scan extent" — how far the dependency section reaches when
    //    deciding which deps are already declared. `[dependencies.<crate>]`
    //    subtables are *part of* `[dependencies]`, so they extend the scan
    //    until a real boundary like `[dev-dependencies]` or `[[bin]]`.
    // 2. The "insertion point" — where to write new shorthand `key = value`
    //    entries. This stops at the FIRST table header (subtable or not),
    //    because TOML attaches shorthand keys to whichever section header
    //    precedes them: a `chrono = "0.4"` placed *after* a
    //    `[dependencies.chrono]` line would become a key inside that
    //    subtable, not a sibling shorthand dep.
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

/// Reject an `enum{…}` field whose generated Rust type name (`pascal(field)`)
/// collides with the model struct itself or one of the companion types the
/// `#[model]`/`#[repository]` macros and the scaffold generator emit from the
/// same resource name — `New{Pascal}`, `Update{Pascal}`, `{Pascal}Field`,
/// `{Pascal}DraftExt` (from `#[model]`), `{Pascal}Repository`/
/// `Pg{Pascal}Repository` (from `#[repository]`, which `generate scaffold`
/// always adds on top of the model), and `DecodedForm` (the scaffold's form
/// struct). A silent collision would produce a duplicate-type-definition
/// compile error far from this DSL token, so it's rejected here with a
/// pointer back to the offending field.
fn validate_enum_field_collisions(
    pascal_name: &str,
    fields: &[Field],
) -> Result<(), GenerateError> {
    for f in fields {
        let Some(enum_ty) = f.enum_type_name() else {
            continue;
        };
        let reserved = [
            pascal_name.to_owned(),
            format!("New{pascal_name}"),
            format!("Update{pascal_name}"),
            format!("{pascal_name}Field"),
            format!("{pascal_name}DraftExt"),
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
    }
    Ok(())
}

pub fn parse_model_metadata(
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

    for default in &options.defaults {
        let (field_name, value) = split_key_value(default, '=')?;
        let field =
            field_by_name(fields, field_name).ok_or_else(|| GenerateError::InvalidField {
                token: default.clone(),
                reason: format!("unknown field '{field_name}'"),
            })?;
        let sql =
            sql_default_literal(field, value).map_err(|reason| GenerateError::InvalidField {
                token: default.clone(),
                reason,
            })?;
        metadata.defaults.insert(field_name.to_owned(), sql);
    }

    Ok(metadata)
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
        if !is_string_like(field) {
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
    matches!(field.kind, FieldKind::String | FieldKind::Text)
}

fn sql_default_literal(field: &Field, value: &str) -> Result<String, String> {
    match field.kind {
        FieldKind::Bool => match value.to_ascii_lowercase().as_str() {
            "true" => Ok("TRUE".to_owned()),
            "false" => Ok("FALSE".to_owned()),
            _ => Err("bool defaults must be true or false".to_owned()),
        },
        FieldKind::String | FieldKind::Text => {
            let unquoted = value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(value);
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
        FieldKind::Uuid
        | FieldKind::NaiveDateTime
        | FieldKind::DateTime
        | FieldKind::Bytea
        | FieldKind::Attachment
        | FieldKind::References => Err(format!(
            "defaults for {} fields are not supported by `autumn generate` yet",
            field.rust_type()
        )),
        FieldKind::Enum => {
            let unquoted = value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(value);
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
fn render_enum_decl(field: &Field, default_variant: Option<&str>) -> String {
    use std::fmt::Write as _;
    let ty = field
        .enum_type_name()
        .expect("render_enum_decl called on a non-enum field");
    let variants: Vec<String> = field.variants.iter().map(|v| pascal(v)).collect();

    let mut out = String::new();

    // Always derive `Default`, even without an explicit `--default`: the
    // `#[model]` macro's generated `UpdateX` patch struct wraps every field
    // in `Patch<T>` and unconditionally derives `Default` on itself, and
    // `#[derive(Default)]` on a generic type adds a `T: Default` bound for
    // every type parameter regardless of which variant is `#[default]`d —
    // so every field's Rust type must implement `Default`, the same
    // requirement every other field kind already satisfies via `std`. Absent
    // an explicit `--default field=variant`, the first declared variant is
    // the natural, unsurprising choice (matching how other kinds default to
    // a canonical baseline, e.g. `i32::default() == 0`).
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

    out
}

fn render_model_file(
    name: &str,
    table: &str,
    fields: &[Field],
    metadata: &ModelMetadata,
    soft_delete: bool,
    shard_key: Option<&str>,
    id_type: IdType,
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
            out.push_str(&render_enum_decl(f, default_variant));
            out.push('\n');
        }
    }
    out.push_str("#[autumn_web::model]\n");
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
        if let Some(validations) = metadata.validations.get(&f.name) {
            for validation in validations {
                let _ = writeln!(out, "    #[validate({validation})]");
            }
        }
        if metadata.defaults.contains_key(&f.name) {
            out.push_str("    #[default]\n");
        }
        let _ = writeln!(out, "    pub {}: {},", f.name, f.rust_type());
    }
    if soft_delete {
        // `deleted_at` must come *before* `created_at` here, matching the
        // column order `create_table_sql_with_metadata_and_id`/
        // `schema_table_block_with_id` emit (they append the soft-delete
        // field to the field list, then always append `created_at` last).
        // The repository macro's generated insert-then-`RETURNING` query
        // loads into this struct positionally, so a struct field order that
        // doesn't match the table's column order produces a Diesel
        // `CompatibleType` mismatch at compile time.
        //
        // `deleted_at` is otherwise DB-managed (NULL on insert, set only by
        // the destroy handler): the migration declares it nullable with no
        // explicit SQL DEFAULT, so Postgres inserts NULL whenever it's
        // omitted from the INSERT column list. `#[default]` excludes it from
        // `NewX`/`UpdateX` accordingly -- without it, the `#[model]` macro
        // treats `deleted_at` as a required field, and neither the
        // `create`/`update` handler ever populates it.
        out.push_str("    #[default]\n");
        out.push_str("    pub deleted_at: Option<chrono::NaiveDateTime>,\n");
    }
    out.push_str("    #[default]\n");
    out.push_str("    pub created_at: chrono::NaiveDateTime,\n");
    out.push_str("}\n");
    out
}

#[cfg(test)]
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
            &["price:Decimal".into()],
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
}
