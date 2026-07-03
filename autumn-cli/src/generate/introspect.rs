//! Pure planning for `autumn db pull` — turn an introspected Postgres schema
//! into the same file set the greenfield generators emit: a `#[model]` struct
//! per table, a `diesel::table!` block in `src/schema.rs`, the `pub mod`
//! aggregator line in `src/models/mod.rs`, and (optionally) a
//! `#[repository(Model)]` trait per table.
//!
//! No database access and no clock happen here — the live introspection lives
//! in [`crate::db_pull`]. Keeping this module pure means the type mapping and
//! the emitted file shape are unit-testable without Docker, and the round-trip
//! property (a greenfield-generated table re-derived here is byte-identical)
//! can be asserted directly against [`super::model`].

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use super::dsl::FieldKind;
use super::emit::Plan;
use super::naming::{pascal, snake};
use super::schema_edit::{add_mod_declaration, singularize, update_main_rs};
use super::{GenerateError, ensure_project_root, read_or_empty};

/// A single introspected column, in catalog (`ordinal_position`) order.
// A flat catalog descriptor: each bool is an independent fact read straight from
// `information_schema`, not interacting state, so separate fields read clearer
// than packing them into an enum.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Column name (used verbatim as the struct field and schema column).
    pub name: String,
    /// Mapped field kind (the inverse of the DSL forward map).
    pub kind: FieldKind,
    /// True when the column is nullable (`Option<T>` in the model).
    pub nullable: bool,
    /// True when the column participates in the table's primary key.
    pub is_pk: bool,
    /// True when the column has a database default (`column_default IS NOT NULL`).
    /// A `created_at` column with a default is annotated `#[default]` so it stays
    /// out of `NewX` (e.g. `created_at TIMESTAMP NOT NULL DEFAULT NOW()`).
    pub has_default: bool,
    /// True when the column default is a sequence generator (`nextval(...)`, i.e.
    /// `SERIAL`/`BIGSERIAL`). Distinguishes a database-assigned `id` from a
    /// constant default such as `DEFAULT 0`, which would collide on insert since
    /// the macros exclude `#[id]` from `NewX`.
    pub has_sequence_default: bool,
    /// True when the column is a stored generated column
    /// (`GENERATED ALWAYS AS (...) STORED`). Such columns are read-only, so they
    /// are annotated `#[default]` to keep them out of inserts and updates.
    pub is_generated: bool,
    /// True when the column is an identity column (`GENERATED ... AS IDENTITY`).
    /// The database supplies the value, so it is annotated `#[default]` to keep
    /// it out of inserts/updates (a `GENERATED ALWAYS` identity rejects explicit
    /// writes outright).
    pub is_identity: bool,
}

impl Column {
    /// Whether the database supplies this column's value on insert — a sequence
    /// default (`SERIAL`/`BIGSERIAL`, `nextval(...)`) or an identity column. Such
    /// a value must not appear in `NewX`, and an `id` PK must satisfy this to be
    /// insertable at all.
    #[must_use]
    pub const fn is_db_generated(&self) -> bool {
        self.has_sequence_default || self.is_identity
    }

    /// The Rust type for the model struct, wrapping nullable columns in `Option`.
    #[must_use]
    pub fn rust_type(&self) -> String {
        let inner = self.kind.rust_type();
        if self.nullable {
            format!("Option<{inner}>")
        } else {
            inner.to_owned()
        }
    }

    /// The Diesel `schema.rs` type token, wrapping nullable columns in `Nullable`.
    #[must_use]
    pub fn schema_type(&self) -> String {
        let inner = self.kind.schema_type();
        if self.nullable {
            format!("Nullable<{inner}>")
        } else {
            inner.to_owned()
        }
    }
}

/// An introspected table: its name plus its columns in catalog order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSchema {
    /// The table name (as it appears in Postgres, typically plural).
    pub table: String,
    /// Columns in `ordinal_position` order.
    pub columns: Vec<Column>,
}

/// Options controlling what `db pull` emits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PullOptions {
    /// Also emit a `#[repository(Model)]` trait per table.
    pub with_repository: bool,
    /// Overwrite/replace existing artifacts (mirrors the generator `--force`).
    /// When set, an existing `schema.rs` `table!` block for a pulled table is
    /// replaced with the freshly introspected one instead of left untouched.
    pub force: bool,
    /// True when the user named specific tables on the command line. An
    /// unsupported explicitly-requested table is a hard error; during an
    /// unscoped pull (every table) an unsupported table is skipped with a notice
    /// so the supported tables still come through.
    pub explicit: bool,
}

/// Compute every filesystem action an `autumn db pull` would perform.
///
/// Pure planning step — no I/O. The live introspection in [`crate::db_pull`]
/// builds the `tables` and then calls this. Unlike the greenfield generators,
/// **no migration is emitted**: the tables already exist in the database.
///
/// # Errors
/// Returns [`GenerateError::NotInProject`] when `project_root` is not an Autumn
/// project, or surfaces collisions when the model/repository files already exist
/// and `--force` was not given (during [`Plan::execute`]).
pub fn plan_pull(
    project_root: &Path,
    tables: &[TableSchema],
    options: PullOptions,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;

    let mut plan = Plan::new(project_root);

    let models_dir = project_root.join("src").join("models");
    let repos_dir = project_root.join("src").join("repositories");

    // Fold the in-place edits (mod.rs / schema.rs / repositories/mod.rs) across
    // every table into a single computed string each, so multi-table pulls touch
    // each aggregator file exactly once.
    let mut models_mod = read_or_empty(&models_dir.join("mod.rs"));
    let mut schema = read_or_empty(&project_root.join("src").join("schema.rs"));
    let mut repos_mod = read_or_empty(&repos_dir.join("mod.rs"));

    // Detect generated-name collisions up front: two tables can singularize to
    // the same module (e.g. `status` and `statuses`), which would otherwise make
    // the second model file silently clobber the first.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // Track whether any repository was actually emitted, so the `repositories`
    // module is only wired up when at least one exists.
    let mut emitted_repository = false;
    // `routes![]` entries for the read-only API handlers of emitted repositories.
    let mut route_entries: Vec<String> = Vec::new();

    for table in tables {
        let resource = singularize(&table.table);
        let snake_name = snake(&resource);
        let pascal_name = pascal(&resource);

        // A table that can't be turned into compilable Autumn code (no usable
        // model name, an invalid column identifier, or no `id` BIGINT primary
        // key) is a hard error when the user asked for it by name, or skipped
        // with a notice during an unscoped pull so the rest still come through.
        if let Some(reason) = unsupported_reason(table, &resource) {
            if options.explicit {
                return Err(GenerateError::Config(format!(
                    "cannot pull table '{}': {reason}",
                    table.table
                )));
            }
            eprintln!("  \u{2139} Skipping table '{}': {reason}", table.table);
            continue;
        }

        if !seen.insert(snake_name.clone()) {
            return Err(GenerateError::Config(format!(
                "two pulled tables map to the same model module '{snake_name}' \
                 (last from table '{}'); pull them individually or rename one",
                table.table
            )));
        }

        // (a) src/models/<snake>.rs
        plan.create(
            models_dir.join(format!("{snake_name}.rs")),
            render_model(&pascal_name, &table.table, &table.columns),
        );
        // (b) src/models/mod.rs aggregator line
        models_mod = add_mod_declaration(&models_mod, &snake_name);
        // (c) src/schema.rs table! block
        schema = upsert_schema_block(&schema, &table.table, &table.columns, options.force)?;

        // (d) optional repository. `unsupported_reason` already guaranteed the
        // Autumn `id`/`i64` PK convention the repository macro assumes, so the
        // emitted trait is always compilable. The real table name is passed
        // through so the schema import is correct for irregular plurals.
        if options.with_repository {
            plan.create(
                repos_dir.join(format!("{snake_name}.rs")),
                super::scaffold::render_repository_for_pull(
                    &pascal_name,
                    &snake_name,
                    &table.table,
                ),
            );
            repos_mod = add_mod_declaration(&repos_mod, &snake_name);
            // Register the read-only JSON handlers the `api = ...` repository
            // generates so `/api/<table>` is reachable. Mutating handlers
            // (create/update/delete) are intentionally left out until the user
            // adds a policy — matching the scaffold's default posture and the
            // security note in the generated repository.
            route_entries.push(format!("repositories::{snake_name}::{snake_name}_api_list"));
            route_entries.push(format!("repositories::{snake_name}::{snake_name}_api_get"));
            emitted_repository = true;
        }
    }

    // If an unscoped pull skipped every table, leave the project untouched
    // rather than writing empty aggregators, bumping deps, and exiting "ok".
    // (An explicitly-requested unsupported table already errored above.)
    if seen.is_empty() {
        eprintln!("  \u{2139} No supported tables to pull \u{2014} nothing was generated.");
        return Ok(Plan::new(project_root));
    }

    plan.modify(models_dir.join("mod.rs"), models_mod);
    plan.modify(project_root.join("src").join("schema.rs"), schema);

    // src/main.rs: declare the modules the emitted code lives in. A fresh
    // `autumn new` main.rs declares none of these, so without this the
    // generated app would not compile.
    let main_path = project_root.join("src").join("main.rs");
    if let Ok(main_existing) = std::fs::read_to_string(&main_path) {
        let mut mods = vec!["models", "schema"];
        if emitted_repository {
            mods.push("repositories");
        }
        let updated = update_main_rs(&main_existing, &mods, &route_entries);
        plan.modify(main_path, updated);
    }

    if emitted_repository {
        plan.modify(repos_dir.join("mod.rs"), repos_mod);
    }

    // Cargo.toml: the `#[model]` expansion references diesel/serde/chrono/uuid,
    // none of which a freshly-`autumn new`-ed project carries. The `#[model]`
    // macro itself is exported only behind `autumn-web`'s `db` feature, which a
    // DB-free `autumn new --daemon` project disables; enable it so the pulled
    // models compile there too.
    let cargo_path = project_root.join("Cargo.toml");
    let existing = read_or_empty(&cargo_path);
    let with_deps = super::model::ensure_cargo_dependencies(&existing, super::model::MODEL_DEPS);
    let (updated, db_enabled) =
        super::schema_edit::ensure_autumn_web_feature_status(&with_deps, "db");
    if !db_enabled {
        // No `autumn-web` dependency could be located to enable `db` on. The
        // generated `#[autumn_web::model]` code needs that feature, so warn
        // rather than emit silently-uncompilable output.
        eprintln!(
            "  \u{26a0} Could not enable autumn-web's `db` feature automatically. \
             Add `features = [\"db\"]` to the `autumn-web` dependency in Cargo.toml \
             so the generated models compile."
        );
    }
    if updated != existing {
        plan.modify(cargo_path, updated);
    }

    Ok(plan)
}

/// Render a `#[model]` struct from introspected columns.
///
/// Columns are emitted in catalog order; the primary-key column is annotated
/// `#[id]` and a column with a database default is annotated `#[default]` (so it
/// stays out of `NewX`). For a greenfield-generated table — `id` `int8` PK first,
/// user fields, `created_at` (`DEFAULT NOW()`) last — this is byte-identical to
/// `render_model_file` in `super::model`, which the round-trip property relies on.
#[must_use]
pub fn render_model(pascal_name: &str, table: &str, columns: &[Column]) -> String {
    let mut out = String::with_capacity(columns.len() * 64 + 256);
    out.push_str("//! Generated by `autumn generate`.\n");
    out.push_str("//!\n");
    out.push_str("//! Edit this file freely — once a generator has run, the\n");
    out.push_str("//! framework treats this as ordinary user code.\n\n");
    let _ = writeln!(out, "use crate::schema::{table};");
    out.push('\n');
    // The model macro infers the table as `pascal_to_snake(Struct) + "s"`, which
    // is wrong for irregular plurals (`person` -> `persons`, not `people`). Emit
    // an explicit `table = "..."` override whenever the inference would not match
    // the real table name, so the struct compiles against the emitted schema
    // block. For regular plurals the inference matches and we emit the bare
    // attribute, keeping greenfield round-trips byte-identical.
    if inferred_table_name(pascal_name) == table {
        out.push_str("#[autumn_web::model]\n");
    } else {
        let _ = writeln!(out, "#[autumn_web::model(table = \"{table}\")]");
    }
    let _ = writeln!(out, "pub struct {pascal_name} {{");
    for col in columns {
        if col.is_pk {
            out.push_str("    #[id]\n");
        } else if is_write_excluded(col) {
            // Read-only / framework-managed columns are kept out of `NewX` and
            // the update set via `#[default]`: a `created_at` with a DB default,
            // or a stored generated column. (A plain mutable column with a DB
            // default stays settable — `#[default]` would lock it out of writes.)
            out.push_str("    #[default]\n");
        }
        let _ = writeln!(out, "    pub {}: {},", col.name, col.rust_type());
    }
    out.push_str("}\n");
    out
}

/// Mirror the model macro's table-name inference (`pascal_to_snake(Struct) + "s"`)
/// so `db pull` can tell when an explicit `table = "..."` override is required.
fn inferred_table_name(pascal_name: &str) -> String {
    format!("{}s", snake(pascal_name))
}

/// Whether a column must be excluded from inserts and updates (`#[default]`):
/// a stored generated column, an identity column, a sequence-backed column
/// (`SERIAL`/`BIGSERIAL`, where the database advances the sequence), or the
/// framework-managed `created_at` (when it carries a DB default). A plain
/// mutable column with a DB default stays settable. The primary key is handled
/// separately by `#[id]` (which takes precedence in `render_model`).
fn is_write_excluded(col: &Column) -> bool {
    col.is_db_generated() || col.is_generated || (col.name == "created_at" && col.has_default)
}

/// Build a `diesel::table!` block from introspected columns.
#[must_use]
pub fn render_schema_block(table: &str, columns: &[Column]) -> String {
    let pk: Vec<&str> = columns
        .iter()
        .filter(|c| c.is_pk)
        .map(|c| c.name.as_str())
        .collect();
    // `plan_pull` rejects/skips tables without an `id` primary key before they
    // reach here, so `pk` is non-empty in practice; default to the Autumn `id`
    // convention rather than inventing the first column as the key.
    let pk_clause = if pk.is_empty() {
        "id".to_owned()
    } else {
        pk.join(", ")
    };
    let mut out = String::new();
    out.push_str("diesel::table! {\n");
    let _ = writeln!(out, "    {table} ({pk_clause}) {{");
    for col in columns {
        let _ = writeln!(out, "        {} -> {},", col.name, col.schema_type());
    }
    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

/// Insert or refresh a `table!` block in `schema.rs`.
///
/// - If no block for `table` exists, append the freshly introspected one.
/// - If a block exists and matches the live table, leave it untouched.
/// - If a block exists, `force` is true, replace it in place so a re-pull after
///   the live table changed doesn't leave the schema block stale.
/// - If a block exists but differs from the live table and `force` is false,
///   error: keeping the stale block while regenerating the model from new
///   columns would emit a model that references a mismatched schema and fail to
///   compile.
fn upsert_schema_block(
    existing: &str,
    table: &str,
    columns: &[Column],
    force: bool,
) -> Result<String, GenerateError> {
    let block = render_schema_block(table, columns);
    // Locate any existing block once and branch on the result, rather than
    // re-scanning the (growing) schema for has/matches/replace separately.
    match schema_block_range(existing, table) {
        Some((start, end)) => {
            if force {
                let mut out = String::with_capacity(existing.len() + block.len());
                out.push_str(&existing[..start]);
                out.push_str(block.trim_end());
                out.push_str(&existing[end..]);
                Ok(out)
            } else if existing[start..end].trim() == block.trim() {
                Ok(existing.to_owned())
            } else {
                Err(GenerateError::Config(format!(
                    "src/schema.rs already has a `table!` block for '{table}' that differs from \
                     the live database; re-run with --force to replace it (the regenerated model \
                     would otherwise reference a stale schema and fail to compile)"
                )))
            }
        }
        None if existing.is_empty() => Ok(block),
        None => {
            let trimmed = existing.trim_end();
            Ok(format!("{trimmed}\n\n{block}"))
        }
    }
}

/// Byte range `[start, end)` of the `table!` block (qualified `diesel::table!`
/// or the bare `table!` re-export) that declares `table`, including any path
/// qualifier so a replacement isn't double-prefixed. `None` if not found.
fn schema_block_range(existing: &str, table: &str) -> Option<(usize, usize)> {
    let needle = format!("{table} (");
    let bytes = existing.as_bytes();
    let mut search_from = 0;
    while let Some(macro_rel) = existing[search_from..].find("table!") {
        let name_start = search_from + macro_rel;
        // Walk back over an optional path qualifier (e.g. `diesel::`).
        let mut macro_start = name_start;
        while macro_start > 0 {
            let c = bytes[macro_start - 1];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b':' {
                macro_start -= 1;
            } else {
                break;
            }
        }
        // Find the opening brace of this macro invocation.
        let Some(brace_rel) = existing[name_start..].find('{') else {
            break;
        };
        let open = name_start + brace_rel;
        // Match braces to find the end of the macro body, ignoring `{`/`}` that
        // appear inside a `//` line comment or a "..." string (Diesel blocks can
        // carry `///` doc comments and `#[sql_name = "..."]` attributes).
        let mut depth = 0usize;
        let mut end = None;
        let mut in_string = false;
        let mut in_line_comment = false;
        let region = &existing.as_bytes()[open..];
        let mut i = 0;
        while i < region.len() {
            let b = region[i];
            if in_line_comment {
                if b == b'\n' {
                    in_line_comment = false;
                }
            } else if in_string {
                if b == b'\\' {
                    i += 1; // skip the escaped byte
                } else if b == b'"' {
                    in_string = false;
                }
            } else if b == b'/' && region.get(i + 1) == Some(&b'/') {
                in_line_comment = true;
                i += 1;
            } else if b == b'"' {
                in_string = true;
            } else if b == b'{' {
                depth += 1;
            } else if b == b'}' {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + i + 1);
                    break;
                }
            }
            i += 1;
        }
        let Some(end) = end else { break };
        // Does this macro define the table we're looking for?
        if existing[open..end]
            .lines()
            .any(|l| l.trim().starts_with(&needle))
        {
            return Some((macro_start, end));
        }
        search_from = end;
    }
    None
}

/// Whether `columns` follow the Autumn `id BIGINT` primary-key convention the
/// `#[model]` / `#[repository]` macros assume: exactly one primary-key column,
/// named `id`, of type `i64`, whose value is generated by the database
/// (`BIGSERIAL`/`nextval(...)` or `GENERATED ... AS IDENTITY`). The macros
/// always exclude `#[id]` from `NewX`, so an `id` without a sequence/identity
/// default could never be inserted (and a constant default like `DEFAULT 0`
/// would collide).
fn has_autumn_id_pk(columns: &[Column]) -> bool {
    let pks: Vec<&Column> = columns.iter().filter(|c| c.is_pk).collect();
    matches!(
        pks.as_slice(),
        [pk] if pk.name == "id" && pk.kind == FieldKind::I64 && pk.is_db_generated()
    )
}

/// Validate that an introspected table can be emitted as compilable Autumn code.
///
/// Returns `Some(reason)` when the table cannot be turned into compilable Autumn
/// code, so the caller can either error (explicit request) or skip (unscoped
/// pull) rather than emit broken `.rs`. The cases:
/// - the singularized table name is not a usable Rust module/struct name,
/// - a column name is not a valid `snake_case` identifier or is a Rust keyword
///   (e.g. a `type` column would produce `pub type: ...`), or
/// - the table does not follow the Autumn `id` `BIGINT` primary-key convention.
///   The `#[model]` macro references the `id` column directly (upsert/on-conflict
///   helpers), so a table without a single `id`/`i64` PK cannot compile.
fn unsupported_reason(table: &TableSchema, resource: &str) -> Option<String> {
    if super::model::validate_resource_name(resource).is_err() {
        return Some(format!(
            "the derived model name '{resource}' is not a valid Rust identifier"
        ));
    }
    // The raw table name is emitted verbatim as the `schema.rs` module and in
    // `use crate::schema::<table>`, so it too must be a valid identifier (a
    // table named `as` singularizes to a fine model `A` but breaks the schema).
    if !super::dsl::is_valid_ident(&table.table) || super::dsl::is_rust_keyword(&table.table) {
        return Some(format!(
            "the table name '{}' is not a valid snake_case Rust identifier (or is a reserved keyword)",
            table.table
        ));
    }
    for col in &table.columns {
        if !super::dsl::is_valid_ident(&col.name) || super::dsl::is_rust_keyword(&col.name) {
            return Some(format!(
                "column '{}' is not a valid snake_case Rust identifier (or is a reserved keyword)",
                col.name
            ));
        }
    }
    if !has_autumn_id_pk(&table.columns) {
        // Tailor the message for a UUID `id` PK: that is a shape Autumn can
        // *generate* (`generate model --id uuid`) but `db pull` cannot yet
        // round-trip, so point that out specifically rather than implying the
        // table simply lacks an `id` column.
        let pks: Vec<&Column> = table.columns.iter().filter(|c| c.is_pk).collect();
        if matches!(pks.as_slice(), [pk] if pk.name == "id" && pk.kind == FieldKind::Uuid) {
            return Some(
                "it uses a UUID `id` primary key, which `db pull` does not yet support \
                 (introspection currently round-trips only database-generated `id` BIGINT \
                 (i64) primary keys); UUID-keyed models can still be created with \
                 `autumn generate model … --id uuid`"
                    .to_owned(),
            );
        }
        return Some(
            "it lacks the Autumn convention of a single database-generated `id` BIGINT (i64) \
             primary key (BIGSERIAL/DEFAULT or GENERATED AS IDENTITY); the #[model] macro \
             references the `id` column directly and excludes it from inserts"
                .to_owned(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::Flags;
    use crate::generate::emit::Action;
    use std::fs;
    use tempfile::TempDir;

    /// A realistic `autumn new` project: Cargo.toml + a main.rs with the
    /// builder chain so module wiring can be exercised.
    fn project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n\n[dependencies]\nautumn-web = \"0.5\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/main.rs"),
            "use autumn_web::prelude::*;\n\n#[autumn_web::main]\nasync fn main() {\n    autumn_web::app()\n        .routes(routes![])\n        .run()\n        .await;\n}\n",
        )
        .unwrap();
        tmp
    }

    fn col(name: &str, kind: FieldKind, nullable: bool, is_pk: bool) -> Column {
        Column {
            name: name.to_owned(),
            kind,
            nullable,
            is_pk,
            has_default: false,
            has_sequence_default: false,
            is_generated: false,
            is_identity: false,
        }
    }

    /// The conventional Autumn `id BIGSERIAL PRIMARY KEY` column: i64 PK whose
    /// value is database-generated via a sequence (`has_sequence_default`).
    fn id_col() -> Column {
        Column {
            name: "id".to_owned(),
            kind: FieldKind::I64,
            nullable: false,
            is_pk: true,
            has_default: true,
            has_sequence_default: true,
            is_generated: false,
            is_identity: false,
        }
    }

    /// A conventional `created_at` column: `TIMESTAMP NOT NULL DEFAULT NOW()`,
    /// so it carries a database default (`has_default = true`).
    fn created_at_col() -> Column {
        Column {
            name: "created_at".to_owned(),
            kind: FieldKind::NaiveDateTime,
            nullable: false,
            is_pk: false,
            has_default: true,
            has_sequence_default: false,
            is_generated: false,
            is_identity: false,
        }
    }

    /// The columns a greenfield `Post title:String body:Text published:bool`
    /// table introspects to: `id` int8 PK, the user fields, `created_at` last.
    fn post_table() -> TableSchema {
        TableSchema {
            table: "posts".to_owned(),
            columns: vec![
                id_col(),
                col("title", FieldKind::String, false, false),
                col("body", FieldKind::String, false, false),
                col("published", FieldKind::Bool, false, false),
                created_at_col(),
            ],
        }
    }

    fn rel_paths(plan: &Plan) -> Vec<String> {
        plan.actions
            .iter()
            .map(|a| {
                a.path()
                    .strip_prefix(&plan.project_root)
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn plan_pull_outside_project_root_errors() {
        let tmp = TempDir::new().unwrap();
        let err = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    #[test]
    fn plan_pull_creates_expected_file_set_and_no_migration() {
        let tmp = project();
        let plan = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap();
        let p = rel_paths(&plan);
        assert!(p.contains(&"src/models/post.rs".into()));
        assert!(p.contains(&"src/models/mod.rs".into()));
        assert!(p.contains(&"src/schema.rs".into()));
        assert!(
            p.iter().all(|path| !path.contains("migrations")),
            "db pull must not emit a migration: {p:?}"
        );
    }

    #[test]
    fn plan_pull_model_honors_conventions() {
        let tmp = project();
        let plan = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();

        let model = fs::read_to_string(tmp.path().join("src/models/post.rs")).unwrap();
        assert!(model.contains("#[autumn_web::model]"));
        assert!(model.contains("pub struct Post"));
        assert!(model.contains("#[id]"));
        assert!(model.contains("pub id: i64,"), "i64 PK must be preserved");
        assert!(model.contains("pub title: String,"));
        assert!(model.contains("pub published: bool,"));
        assert!(model.contains("#[default]\n    pub created_at: chrono::NaiveDateTime,"));
    }

    #[test]
    fn plan_pull_nullable_columns_become_option() {
        let tmp = project();
        let table = TableSchema {
            table: "notes".to_owned(),
            columns: vec![id_col(), col("body", FieldKind::String, true, false)],
        };
        let plan = plan_pull(tmp.path(), &[table], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/note.rs")).unwrap();
        assert!(
            model.contains("pub body: Option<String>,"),
            "nullable column must be Option<T>: {model}"
        );
        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(
            schema.contains("body -> Nullable<Text>,"),
            "nullable column schema must be Nullable<T>: {schema}"
        );
    }

    #[test]
    fn plan_pull_schema_block_and_mod_line() {
        let tmp = project();
        let plan = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert!(schema.contains("posts (id)"));
        assert!(schema.contains("title -> Text,"));
        assert!(schema.contains("id -> Int8,"));

        let mod_rs = fs::read_to_string(tmp.path().join("src/models/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod post;"));
    }

    #[test]
    fn plan_pull_wires_modules_into_main_rs() {
        let tmp = project();
        let plan = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main.contains("mod models;"),
            "main.rs must declare models: {main}"
        );
        assert!(
            main.contains("mod schema;"),
            "main.rs must declare schema: {main}"
        );
    }

    #[test]
    fn plan_pull_multiple_tables_fold_aggregators_once() {
        let tmp = project();
        let comments = TableSchema {
            table: "comments".to_owned(),
            columns: vec![
                id_col(),
                col("body", FieldKind::String, false, false),
                created_at_col(),
            ],
        };
        let plan = plan_pull(
            tmp.path(),
            &[post_table(), comments],
            PullOptions::default(),
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let schema = fs::read_to_string(tmp.path().join("src/schema.rs")).unwrap();
        assert_eq!(schema.matches("posts (id)").count(), 1);
        assert_eq!(schema.matches("comments (id)").count(), 1);

        let mod_rs = fs::read_to_string(tmp.path().join("src/models/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod post;"));
        assert!(mod_rs.contains("pub mod comment;"));
        // exactly one Modify per aggregator file.
        let schema_mods = plan
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Modify { path, .. } if path.ends_with("schema.rs")))
            .count();
        assert_eq!(schema_mods, 1);
    }

    #[test]
    fn plan_pull_with_repository_flag() {
        let tmp = project();
        let plan = plan_pull(
            tmp.path(),
            &[post_table()],
            PullOptions {
                with_repository: true,
                ..PullOptions::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();

        let repo = fs::read_to_string(tmp.path().join("src/repositories/post.rs")).unwrap();
        assert!(
            repo.contains(
                "#[autumn_web::repository(Post, table = \"posts\", api = \"/api/posts\")]"
            )
        );
        assert!(repo.contains("use crate::schema::posts;"));
        let repo_mod = fs::read_to_string(tmp.path().join("src/repositories/mod.rs")).unwrap();
        assert!(repo_mod.contains("pub mod post;"));
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("mod repositories;"));
        // The read-only API handlers are registered so /api/posts is reachable;
        // mutating handlers are intentionally left out until a policy is added.
        assert!(
            main.contains("repositories::post::post_api_list"),
            "API list route must be registered: {main}"
        );
        assert!(main.contains("repositories::post::post_api_get"));
        assert!(
            !main.contains("post_api_create"),
            "mutating API routes must not be auto-registered: {main}"
        );
    }

    #[test]
    fn plan_pull_unscoped_all_unsupported_writes_nothing() {
        let tmp = project();
        // Only a uuid-keyed table: unscoped pull skips it and must not touch the
        // project (no empty mod.rs/schema.rs, no Cargo.toml churn).
        let sessions = TableSchema {
            table: "sessions".to_owned(),
            columns: vec![col("token", FieldKind::Uuid, false, true)],
        };
        let plan = plan_pull(tmp.path(), &[sessions], PullOptions::default()).unwrap();
        assert!(
            plan.actions.is_empty(),
            "an all-skipped unscoped pull must produce no actions"
        );
        plan.execute(Flags::default()).unwrap();
        assert!(!tmp.path().join("src/schema.rs").exists());
        assert!(!tmp.path().join("src/models/mod.rs").exists());
    }

    #[test]
    fn plan_pull_without_repository_flag_emits_no_repository() {
        let tmp = project();
        let plan = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(!tmp.path().join("src/repositories/post.rs").exists());
    }

    #[test]
    fn plan_pull_collision_without_force_errors() {
        let tmp = project();
        let models = tmp.path().join("src/models");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("post.rs"), "// existing").unwrap();
        let plan = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap();
        let err = plan.execute(Flags::default()).unwrap_err();
        assert!(matches!(err, GenerateError::Collisions(_)));
        // Untouched without --force.
        assert_eq!(
            fs::read_to_string(models.join("post.rs")).unwrap(),
            "// existing"
        );
    }

    #[test]
    fn plan_pull_force_overwrites() {
        let tmp = project();
        let models = tmp.path().join("src/models");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("post.rs"), "// existing").unwrap();
        let plan = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap();
        plan.execute(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();
        assert!(
            fs::read_to_string(models.join("post.rs"))
                .unwrap()
                .contains("pub struct Post")
        );
    }

    #[test]
    fn plan_pull_dry_run_writes_nothing() {
        let tmp = project();
        let plan = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap();
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(!tmp.path().join("src/models/post.rs").exists());
        assert!(!tmp.path().join("src/schema.rs").exists());
    }

    // ── Fail-loud guards for brownfield edge cases ──────────────────────────

    #[test]
    fn plan_pull_errors_on_table_without_primary_key_when_explicit() {
        let tmp = project();
        let no_pk = TableSchema {
            table: "audit_logs".to_owned(),
            columns: vec![
                col("message", FieldKind::String, false, false),
                created_at_col(),
            ],
        };
        let err = plan_pull(
            tmp.path(),
            &[no_pk],
            PullOptions {
                explicit: true,
                ..PullOptions::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("audit_logs"),
            "error must name the table: {msg}"
        );
        assert!(msg.contains("primary key"), "error must explain why: {msg}");
        assert!(!tmp.path().join("src/models/audit_log.rs").exists());
    }

    #[test]
    fn unsupported_reason_explains_uuid_id_pk() {
        // A UUID `id` PK is a shape Autumn can generate but db pull cannot yet
        // round-trip; the reason must call that out specifically, and an explicit
        // pull must fail loudly rather than emit broken code.
        let uuid_posts = TableSchema {
            table: "posts".to_owned(),
            columns: vec![
                Column {
                    name: "id".to_owned(),
                    kind: FieldKind::Uuid,
                    nullable: false,
                    is_pk: true,
                    has_default: true,
                    has_sequence_default: false,
                    is_generated: false,
                    is_identity: false,
                },
                col("title", FieldKind::String, false, false),
                created_at_col(),
            ],
        };
        let reason =
            unsupported_reason(&uuid_posts, "Post").expect("UUID id PK must be unsupported");
        assert!(
            reason.contains("UUID") && reason.contains("db pull"),
            "reason must call out UUID + db pull: {reason}"
        );

        let tmp = project();
        let err = plan_pull(
            tmp.path(),
            &[uuid_posts],
            PullOptions {
                explicit: true,
                ..PullOptions::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("UUID"),
            "explicit pull error must mention UUID: {err}"
        );
        assert!(!tmp.path().join("src/models/post.rs").exists());
    }

    #[test]
    fn plan_pull_skips_unsupported_table_on_unscoped_pull() {
        let tmp = project();
        // A no-PK table during an unscoped pull is skipped (not errored), so
        // the supported tables alongside it still come through.
        let no_pk = TableSchema {
            table: "audit_logs".to_owned(),
            columns: vec![col("message", FieldKind::String, false, false)],
        };
        let plan = plan_pull(tmp.path(), &[no_pk, post_table()], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(!tmp.path().join("src/models/audit_log.rs").exists());
        assert!(tmp.path().join("src/models/post.rs").exists());
    }

    #[test]
    fn plan_pull_errors_on_invalid_column_identifier_when_explicit() {
        let tmp = project();
        // `type` is a Rust keyword; emitting `pub type: ...` would not compile.
        let bad = TableSchema {
            table: "items".to_owned(),
            columns: vec![id_col(), col("type", FieldKind::String, false, false)],
        };
        let err = plan_pull(
            tmp.path(),
            &[bad],
            PullOptions {
                explicit: true,
                ..PullOptions::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("items"), "error must name the table: {msg}");
        assert!(msg.contains("type"), "error must name the column: {msg}");
        assert!(!tmp.path().join("src/models/item.rs").exists());
    }

    #[test]
    fn plan_pull_errors_on_colliding_generated_module_names() {
        let tmp = project();
        // `status` and `statuses` both singularize to `status`.
        let status = TableSchema {
            table: "status".to_owned(),
            columns: vec![id_col()],
        };
        let statuses = TableSchema {
            table: "statuses".to_owned(),
            columns: vec![id_col()],
        };
        let err = plan_pull(tmp.path(), &[status, statuses], PullOptions::default()).unwrap_err();
        assert!(
            err.to_string().contains("same model module"),
            "error must explain the collision: {err}"
        );
    }

    #[test]
    fn plan_pull_default_annotation_rules() {
        let tmp = project();
        let table = TableSchema {
            table: "widgets".to_owned(),
            columns: vec![
                id_col(),
                Column {
                    // A mutable column with a DB default must stay settable, so it
                    // must NOT be `#[default]` (that would lock it out of writes).
                    name: "status".to_owned(),
                    kind: FieldKind::String,
                    nullable: false,
                    is_pk: false,
                    has_default: true, // e.g. DEFAULT 'draft'
                    has_sequence_default: false,
                    is_generated: false,
                    is_identity: false,
                },
                Column {
                    // A stored generated column is read-only -> `#[default]`.
                    name: "search".to_owned(),
                    kind: FieldKind::String,
                    nullable: false,
                    is_pk: false,
                    has_default: false,
                    has_sequence_default: false,
                    is_generated: true,
                    is_identity: false,
                },
                col("label", FieldKind::String, false, false),
                created_at_col(),
            ],
        };
        let plan = plan_pull(tmp.path(), &[table], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/widget.rs")).unwrap();
        assert!(model.contains("#[id]\n    pub id: i64,"));
        // mutable defaulted column: plain field, settable.
        assert!(model.contains("    pub status: String,"));
        assert!(!model.contains("#[default]\n    pub status:"));
        // generated column: read-only via #[default].
        assert!(model.contains("#[default]\n    pub search: String,"));
        // framework-managed created_at: #[default].
        assert!(model.contains("#[default]\n    pub created_at: chrono::NaiveDateTime,"));
        // the PK must not be doubly annotated.
        assert!(!model.contains("#[id]\n    #[default]"));
        // a plain column has neither.
        assert!(model.contains("    pub label: String,"));
    }

    #[test]
    fn plan_pull_emits_table_override_for_irregular_plural() {
        let tmp = project();
        // `people` singularizes to `person`; the macro would infer `persons`.
        let people = TableSchema {
            table: "people".to_owned(),
            columns: vec![id_col(), created_at_col()],
        };
        let plan = plan_pull(tmp.path(), &[people], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/person.rs")).unwrap();
        assert!(
            model.contains("#[autumn_web::model(table = \"people\")]"),
            "irregular plural needs an explicit table override: {model}"
        );
        assert!(model.contains("use crate::schema::people;"));
    }

    #[test]
    fn plan_pull_skips_non_id_pk_table_entirely() {
        let tmp = project();
        // A uuid-keyed table cannot be modeled: the `#[model]` macro references
        // the `id` column directly. On an unscoped pull it is skipped entirely —
        // neither the model nor the repository is emitted.
        let sessions = TableSchema {
            table: "sessions".to_owned(),
            columns: vec![
                col("token", FieldKind::Uuid, false, true),
                col("data", FieldKind::String, false, false),
            ],
        };
        let plan = plan_pull(
            tmp.path(),
            &[sessions],
            PullOptions {
                with_repository: true,
                ..PullOptions::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(!tmp.path().join("src/models/session.rs").exists());
        assert!(!tmp.path().join("src/repositories/session.rs").exists());
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(!main.contains("mod repositories;"));
    }

    #[test]
    fn plan_pull_repository_uses_real_table_name_for_irregular_plural() {
        let tmp = project();
        // `statuses` singularizes to `status`; re-pluralizing would yield the
        // wrong `statuss` schema import. The repo must use the real table name.
        let statuses = TableSchema {
            table: "statuses".to_owned(),
            columns: vec![id_col(), created_at_col()],
        };
        let plan = plan_pull(
            tmp.path(),
            &[statuses],
            PullOptions {
                with_repository: true,
                ..PullOptions::default()
            },
        )
        .unwrap();
        plan.execute(Flags::default()).unwrap();
        let repo = fs::read_to_string(tmp.path().join("src/repositories/status.rs")).unwrap();
        assert!(
            repo.contains("use crate::schema::statuses;"),
            "repo must import the real table module: {repo}"
        );
        assert!(
            repo.contains("table = \"statuses\""),
            "repo macro must pin the real table name: {repo}"
        );
        assert!(
            repo.contains("api = \"/api/statuses\""),
            "repo REST mount must use the real table name: {repo}"
        );
        assert!(!repo.contains("statuss\""), "must not re-pluralize: {repo}");
    }

    #[test]
    fn plan_pull_rejects_non_database_generated_id() {
        let tmp = project();
        // `id BIGINT PRIMARY KEY` with no default/identity: the macro excludes
        // `#[id]` from inserts, so rows could never be created.
        let bare_id = TableSchema {
            table: "things".to_owned(),
            columns: vec![
                Column {
                    name: "id".to_owned(),
                    kind: FieldKind::I64,
                    nullable: false,
                    is_pk: true,
                    has_default: false,
                    has_sequence_default: false,
                    is_generated: false,
                    is_identity: false,
                },
                col("label", FieldKind::String, false, false),
            ],
        };
        let err = plan_pull(
            tmp.path(),
            &[bare_id],
            PullOptions {
                explicit: true,
                ..PullOptions::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("database-generated"),
            "error must explain the id must be DB-generated: {err}"
        );
    }

    #[test]
    fn plan_pull_accepts_identity_id_and_excludes_identity_columns() {
        let tmp = project();
        let invoices = TableSchema {
            table: "invoices".to_owned(),
            columns: vec![
                // id via GENERATED AS IDENTITY (no column_default) is supported.
                Column {
                    name: "id".to_owned(),
                    kind: FieldKind::I64,
                    nullable: false,
                    is_pk: true,
                    has_default: false,
                    has_sequence_default: false,
                    is_generated: false,
                    is_identity: true,
                },
                // A non-PK identity column must be kept out of writes.
                Column {
                    name: "invoice_no".to_owned(),
                    kind: FieldKind::I64,
                    nullable: false,
                    is_pk: false,
                    has_default: false,
                    has_sequence_default: false,
                    is_generated: false,
                    is_identity: true,
                },
                created_at_col(),
            ],
        };
        let plan = plan_pull(tmp.path(), &[invoices], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/invoice.rs")).unwrap();
        assert!(model.contains("#[id]\n    pub id: i64,"));
        assert!(
            model.contains("#[default]\n    pub invoice_no: i64,"),
            "identity column must be write-excluded: {model}"
        );
    }

    #[test]
    fn plan_pull_excludes_non_pk_sequence_column_from_writes() {
        let tmp = project();
        // A non-PK `BIGSERIAL` column is database-advanced, so it must stay out
        // of inserts/updates (`#[default]`) — same as identity/generated.
        let table = TableSchema {
            table: "invoices".to_owned(),
            columns: vec![
                id_col(),
                Column {
                    name: "invoice_no".to_owned(),
                    kind: FieldKind::I64,
                    nullable: false,
                    is_pk: false,
                    has_default: true,
                    has_sequence_default: true,
                    is_generated: false,
                    is_identity: false,
                },
                created_at_col(),
            ],
        };
        let plan = plan_pull(tmp.path(), &[table], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        let model = fs::read_to_string(tmp.path().join("src/models/invoice.rs")).unwrap();
        assert!(
            model.contains("#[default]\n    pub invoice_no: i64,"),
            "non-PK sequence column must be write-excluded: {model}"
        );
    }

    #[test]
    fn plan_pull_rejects_keyword_table_name_when_explicit() {
        let tmp = project();
        // A (quoted) table literally named `as` would emit `use crate::schema::as;`.
        let bad = TableSchema {
            table: "as".to_owned(),
            columns: vec![id_col()],
        };
        let err = plan_pull(
            tmp.path(),
            &[bad],
            PullOptions {
                explicit: true,
                ..PullOptions::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("table name 'as'"),
            "error must name the offending table: {err}"
        );
    }

    #[test]
    fn plan_pull_rejects_bare_underscore_column_when_explicit() {
        let tmp = project();
        // `pub _: String` is the reserved wildcard and would not compile.
        let bad = TableSchema {
            table: "items".to_owned(),
            columns: vec![id_col(), col("_", FieldKind::String, false, false)],
        };
        let err = plan_pull(
            tmp.path(),
            &[bad],
            PullOptions {
                explicit: true,
                ..PullOptions::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("column '_'"),
            "error must name the offending column: {err}"
        );
    }

    #[test]
    fn plan_pull_enables_autumn_web_db_feature() {
        let tmp = project();
        // The `#[model]` macro is gated behind autumn-web's `db` feature, which a
        // DB-free daemon project disables, so the pull must enable it.
        let table = TableSchema {
            table: "widgets".to_owned(),
            columns: vec![id_col(), col("name", FieldKind::String, false, false)],
        };
        let plan = plan_pull(tmp.path(), &[table], PullOptions::default()).unwrap();
        plan.execute(Flags::default()).unwrap();
        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("autumn-web = { version = \"0.5\", features = [\"db\"] }"),
            "pull must enable autumn-web's db feature: {cargo}"
        );
    }

    #[test]
    fn plan_pull_force_replaces_stale_schema_block() {
        let tmp = project();
        // Pre-seed schema.rs with a stale block (old column set) for `posts`.
        let src = tmp.path().join("src");
        fs::write(
            src.join("schema.rs"),
            "diesel::table! {\n    posts (id) {\n        id -> Int8,\n        old_col -> Text,\n    }\n}\n",
        )
        .unwrap();
        let plan = plan_pull(
            tmp.path(),
            &[post_table()],
            PullOptions {
                force: true,
                ..PullOptions::default()
            },
        )
        .unwrap();
        plan.execute(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();
        let schema = fs::read_to_string(src.join("schema.rs")).unwrap();
        assert!(
            !schema.contains("old_col"),
            "stale column must be replaced under --force: {schema}"
        );
        assert!(schema.contains("title -> Text,"));
        assert_eq!(schema.matches("posts (id)").count(), 1);
    }

    #[test]
    fn plan_pull_force_replaces_block_with_brace_in_comment() {
        let tmp = project();
        let src = tmp.path().join("src");
        // A hand-edited block whose doc comment contains a `}` must not confuse
        // brace-matching: the whole block (through its real closing brace) is
        // replaced, leaving no dangling brace.
        fs::write(
            src.join("schema.rs"),
            "diesel::table! {\n    posts (id) {\n        /// trailing brace } in a comment\n        old_col -> Text,\n    }\n}\n",
        )
        .unwrap();
        let plan = plan_pull(
            tmp.path(),
            &[post_table()],
            PullOptions {
                force: true,
                ..PullOptions::default()
            },
        )
        .unwrap();
        plan.execute(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();
        let schema = fs::read_to_string(src.join("schema.rs")).unwrap();
        assert!(
            !schema.contains("old_col"),
            "stale block must be replaced: {schema}"
        );
        assert!(
            !schema.contains("trailing brace"),
            "comment must be gone: {schema}"
        );
        assert!(schema.contains("title -> Text,"));
        assert_eq!(schema.matches("posts (id)").count(), 1);
        // No leftover/dangling brace: braces stay balanced.
        assert_eq!(
            schema.matches('{').count(),
            schema.matches('}').count(),
            "braces must be balanced: {schema}"
        );
    }

    #[test]
    fn plan_pull_without_force_errors_on_stale_schema_block() {
        let tmp = project();
        let src = tmp.path().join("src");
        // A stale block (extra/old column) that no longer matches the live table.
        fs::write(
            src.join("schema.rs"),
            "diesel::table! {\n    posts (id) {\n        id -> Int8,\n        old_col -> Text,\n    }\n}\n",
        )
        .unwrap();
        // Without --force, regenerating the model against this stale block would
        // emit uncompilable code, so the pull must error rather than "succeed".
        let err = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap_err();
        assert!(
            err.to_string().contains("differs from the live database"),
            "stale schema block must be reported: {err}"
        );
    }

    #[test]
    fn plan_pull_without_force_keeps_matching_schema_block() {
        let tmp = project();
        let src = tmp.path().join("src");
        // An existing block identical to the live table is fine without --force.
        let block = render_schema_block("posts", &post_table().columns);
        fs::write(src.join("schema.rs"), format!("{block}\n")).unwrap();
        let plan = plan_pull(tmp.path(), &[post_table()], PullOptions::default()).unwrap();
        plan.execute(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();
        let schema = fs::read_to_string(src.join("schema.rs")).unwrap();
        assert_eq!(
            schema.matches("posts (id)").count(),
            1,
            "an identical block must not be duplicated: {schema}"
        );
        assert!(schema.contains("title -> Text,"));
    }

    #[test]
    fn plan_pull_force_replaces_unqualified_table_block() {
        let tmp = project();
        let src = tmp.path().join("src");
        // Older / hand-written schemas use the unqualified `table!` re-export.
        fs::write(
            src.join("schema.rs"),
            "table! {\n    posts (id) {\n        id -> Int8,\n        old_col -> Text,\n    }\n}\n",
        )
        .unwrap();
        let plan = plan_pull(
            tmp.path(),
            &[post_table()],
            PullOptions {
                force: true,
                ..PullOptions::default()
            },
        )
        .unwrap();
        plan.execute(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();
        let schema = fs::read_to_string(src.join("schema.rs")).unwrap();
        assert!(
            !schema.contains("old_col"),
            "stale column must be replaced even for an unqualified table! block: {schema}"
        );
        assert!(schema.contains("title -> Text,"));
        assert!(
            !schema.contains("diesel::diesel::"),
            "the regenerated block must not be double-prefixed: {schema}"
        );
        assert_eq!(schema.matches("posts (id)").count(), 1);
    }

    #[test]
    fn plan_pull_rejects_constant_default_id() {
        let tmp = project();
        // `id BIGINT PRIMARY KEY DEFAULT 0`: has a default, but not a sequence or
        // identity, so inserts (which omit `#[id]`) would collide on the constant.
        let table = TableSchema {
            table: "things".to_owned(),
            columns: vec![
                Column {
                    name: "id".to_owned(),
                    kind: FieldKind::I64,
                    nullable: false,
                    is_pk: true,
                    has_default: true,
                    has_sequence_default: false,
                    is_generated: false,
                    is_identity: false,
                },
                col("label", FieldKind::String, false, false),
            ],
        };
        let err = plan_pull(
            tmp.path(),
            &[table],
            PullOptions {
                explicit: true,
                ..PullOptions::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("database-generated"),
            "a constant DEFAULT id must be rejected: {err}"
        );
    }

    // ── Round-trip property (AC4) ───────────────────────────────────────────

    #[test]
    fn round_trip_matches_greenfield_model_byte_for_byte() {
        use crate::generate::dsl::{Field, sql_type_to_field_kind};
        use crate::generate::model::render_model_file_for_test;

        // Forward: the model `autumn generate model Post title:String body:Text
        // published:bool` would render.
        let fields = vec![
            Field {
                name: "title".into(),
                kind: FieldKind::String,
                nullable: false,
                variants: Vec::new(),
            },
            Field {
                name: "body".into(),
                kind: FieldKind::Text,
                nullable: false,
                variants: Vec::new(),
            },
            Field {
                name: "published".into(),
                kind: FieldKind::Bool,
                nullable: false,
                variants: Vec::new(),
            },
        ];
        let greenfield = render_model_file_for_test("Post", "posts", &fields);

        // Inverse: synthesize the catalog rows that table produces, invert each
        // udt_name, and re-render via the introspection path.
        // (name, udt_name, nullable, is_pk, has_default)
        let udts = [
            ("id", "int8", false, true, false),
            ("title", "text", false, false, false),
            ("body", "text", false, false, false),
            ("published", "bool", false, false, false),
            ("created_at", "timestamp", false, false, true),
        ];
        let columns: Vec<Column> = udts
            .iter()
            .map(|(name, udt, nullable, is_pk, has_default)| Column {
                name: (*name).to_owned(),
                kind: sql_type_to_field_kind(udt).unwrap(),
                nullable: *nullable,
                is_pk: *is_pk,
                has_default: *has_default,
                // Irrelevant to render_model (byte-identity test); the id's
                // sequence default only gates plan_pull's PK acceptance.
                has_sequence_default: *is_pk,
                is_generated: false,
                is_identity: false,
            })
            .collect();
        let re_derived = render_model(&pascal(&singularize("posts")), "posts", &columns);

        assert_eq!(
            greenfield, re_derived,
            "introspected model must be byte-identical to greenfield"
        );
    }
}
