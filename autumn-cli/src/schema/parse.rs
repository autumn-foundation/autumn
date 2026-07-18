//! `syn`-backed reader that turns an app's `#[model]` structs into the shared
//! [`autumn_schema_core`] schema IR — the **desired state** input a later slice
//! diffs against a checked-in snapshot and the live database.
//!
//! This is slice 2 of the declarative-schema wave (tracking issue #1975): it is
//! deliberately **read-only**. It parses Rust source with [`syn`], walks each
//! `#[model]` struct, and reconstructs the [`Table`] the generator would have
//! produced — reusing [`ColumnType::from_rust_type`] for the field-type mapping
//! and the CLI's own [`naming::pluralize`] / [`naming::pascal_to_snake`] for
//! table-name inference so the parser and the generator can never drift on those
//! two axes. It emits **no** migration, `schema.rs`, or codegen output; those are
//! later slices.
//!
//! # Recognized `#[model]` vocabulary
//!
//! Struct-level, on the `#[model(...)]` / `#[autumn_web::model(...)]` attribute:
//! - `table = "..."` — explicit table-name override (mirrors the macro's one
//!   real argument; see `autumn_macros`'s `parse_attr_args`).
//! - `managed` (also accepted as `schema = "managed"`) — the Decision-4 adoption
//!   marker recorded on [`Table::managed`]. NOTE: this is a **parser-recognized**
//!   marker for the declarative-schema wave; the runtime `#[model]` macro does
//!   not yet accept it as an argument. Any other nested argument is ignored
//!   (forward-compatible).
//!
//! Field-level:
//! - `#[id]` — primary-key column (macro PK resolution is mirrored: explicit
//!   `#[id]` fields win, else the first `i32`/`i64` field, else a reserved `id`).
//! - `#[indexed]` — a non-unique secondary index on the column.
//! - `#[default]` — a column default. Convention defaults the generator emits are
//!   recovered by [`convention_default`]: the reserved `created_at` column *marked
//!   `#[default]`* gets a `NOW()` default (→ [`ColumnDefault::Now`]) and a UUID
//!   primary key on Postgres gets `gen_random_uuid()` (→ [`ColumnDefault::Sql`]).
//!   The `created_at` `NOW()` convention requires the generator's `#[default]`
//!   marker — a hand-written `created_at` without `#[default]` gets no inferred
//!   default. Every other `#[default]` field carries no inferred DB default (their
//!   default, if any, lives in the migration). See the slice-3+ limitation below.
//! - `#[references]` / `#[references(table = "...")]` — a foreign-key column
//!   (forces [`ColumnType::Int64`], infers the target table from the column name
//!   via the generator's `<name>`-minus-`_id`-then-pluralize convention, and adds
//!   the same auto-index the generator emits for a reference column). This is a
//!   **parser-recognized** marker: real generator output today represents a
//!   foreign key as a plain `<name>_id: i64` field plus a struct-level
//!   `#[belongs_to(...)]` association, not a `#[references]` field attribute (see
//!   the slice-3+ limitation below).
//! - `#[unique]` — a `UNIQUE` column + unique index. Also **parser-recognized**:
//!   generator output records uniqueness only in the migration (a `CREATE UNIQUE
//!   INDEX`), not on the struct field (see the slice-3+ limitation below).
//!
//! Every other field attribute the `#[model]` macro accepts (`#[searchable]`,
//! `#[validate]`, `#[encrypted]`, `#[private]`, `#[normalize]`, `#[lock_version]`,
//! `#[state_machine]`, `#[factory_assoc]`) does not change a column's *shape* and
//! is ignored here — the column is still parsed from its Rust type.
//!
//! # Deliberate slice-3+ limitations (documented, not bugs)
//!
//! - **Enum variants + `CHECK` constraints**: a field whose type is a generated
//!   enum (`pub status: PostStatus`) cannot be resolved from the struct alone —
//!   [`ColumnType::from_rust_type`] returns `None` because the concrete enum type
//!   is not a known scalar and its variant set is not visible from the field.
//!   The parser records a [`SchemaDiagnostic`] and skips the column rather than
//!   guessing; it never fabricates a `CHECK` for an enum. Reading the enum
//!   `#[derive]` definitions (to recover the variant set and the closed-set
//!   `CHECK`) is a later slice.
//! - **Association-based foreign keys**: real generator output models a foreign
//!   key as `<name>_id: i64` + a struct-level `#[belongs_to(...)]`. Resolving a
//!   foreign key from that convention (or from the association attribute) requires
//!   cross-model resolution and is deferred; today only an explicit
//!   `#[references]` field attribute yields a [`ForeignKey`].
//! - **Non-convention `#[default]` fields**: only the two conventions
//!   [`convention_default`] knows about are recovered from the struct alone — the
//!   reserved `created_at` `NOW()` default (which requires the generator's
//!   `#[default]` marker — a hand-written `created_at` without `#[default]` gets no
//!   inferred default) and a Postgres UUID primary key's `gen_random_uuid()`
//!   default. Every other `#[default]` field carries no inferred DB default (their
//!   default, if any, lives in the migration). A soft-delete `deleted_at` carries
//!   `#[default]` solely to exclude it from the generated `New*`/`Update*`
//!   structs — the migration leaves its column default UNSET (new rows stay NULL),
//!   so the parser must not infer `Now` for it. A `#[default]` on any other field
//!   (an enum default `'draft'`, a numeric default) likewise does not carry its
//!   literal in the struct — the value lives in the migration/DSL. Recovering those
//!   from the migration is a later slice.
//! - **Unique-index name disambiguation**: the generator disambiguates a unique
//!   index name past Postgres's 63-byte identifier limit (see
//!   `schema_edit::unique_index_name`). The parser uses the plain
//!   `idx_<table>_<field>_unique` form; exact parity for pathological long names
//!   is a later refinement.

use std::path::Path;

use autumn_schema_core::{Backend, Column, ColumnDefault, ColumnType, ForeignKey, Index, Table};

use crate::generate::naming;

/// An error that aborts a whole parse (a syntactically invalid source file or an
/// unreadable path). A single unsupported *field* does **not** abort — it is
/// recorded as a [`SchemaDiagnostic`] on the successful [`ParsedSchema`].
#[derive(Debug, thiserror::Error)]
pub enum SchemaParseError {
    /// The Rust source did not parse. Carries the `syn` message plus the 1-based
    /// line/column of the offending span (available because `autumn-cli` builds
    /// `proc-macro2` with `span-locations`).
    #[error("failed to parse Rust source at line {line}, column {column}: {message}")]
    Syntax {
        /// The underlying `syn` error message.
        message: String,
        /// 1-based line of the error span.
        line: usize,
        /// 1-based column of the error span.
        column: usize,
    },
    /// A models file or directory could not be read.
    #[error("failed to read {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

impl SchemaParseError {
    fn from_syn(err: &syn::Error) -> Self {
        let start = err.span().start();
        Self::Syntax {
            message: err.to_string(),
            line: start.line,
            column: start.column + 1,
        }
    }
}

/// A non-fatal parse diagnostic: a field the parser could not map to a
/// [`ColumnType`] (typically a generated-enum or association type). The column is
/// skipped — never silently mapped to a wrong type — and this record explains why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDiagnostic {
    /// The `#[model]` struct the field belongs to.
    pub model: String,
    /// The resolved table name for the model — the `#[model(table = "...")]`
    /// override when present, else the convention name
    /// (`pluralize(pascal_to_snake(model))`). This is the same name recorded on
    /// the emitted [`Table`], so a consumer can match a diagnostic to its table
    /// without re-deriving the convention name (which would miss a custom table
    /// name).
    pub table: String,
    /// The field name whose type could not be resolved.
    pub field: String,
    /// The unresolved Rust type token (as written in the struct).
    pub rust_type: String,
    /// A human-readable explanation.
    pub message: String,
}

/// The result of a successful parse: the reconstructed tables plus any
/// non-fatal per-field diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSchema {
    /// One [`Table`] per `#[model]` struct, in source order.
    pub tables: Vec<Table>,
    /// Fields that could not be resolved (see [`SchemaDiagnostic`]).
    pub diagnostics: Vec<SchemaDiagnostic>,
}

impl ParsedSchema {
    /// Wrap an already-materialized set of [`Table`]s as a desired-state
    /// [`ParsedSchema`] with no diagnostics.
    ///
    /// The `#[model]` parser is not the only producer of a desired state: the
    /// [`crate::schema::introspect`] database-introspection path builds
    /// [`Table`]s directly from a live Postgres catalog. Both feed the same
    /// [`crate::schema::diff::diff_schema`] engine (which takes a `&ParsedSchema`
    /// as its desired side), so this adapter lets an introspected table set be
    /// diffed against a snapshot baseline without inventing a fake model source.
    /// There are no parser diagnostics for an introspected schema (every column
    /// is resolved — an unmappable type becomes
    /// [`ColumnType::Opaque`](autumn_schema_core::ColumnType::Opaque) rather than
    /// being skipped), so the `diagnostics` vec is empty.
    #[must_use]
    pub const fn from_tables(tables: Vec<Table>) -> Self {
        Self {
            tables,
            diagnostics: Vec::new(),
        }
    }
}

/// Parse a single Rust source string, returning one [`Table`] per `#[model]`
/// struct plus any per-field diagnostics.
///
/// # Errors
///
/// Returns [`SchemaParseError::Syntax`] if `src` is not valid Rust.
pub fn parse_model_source(src: &str, backend: Backend) -> Result<ParsedSchema, SchemaParseError> {
    let file = syn::parse_file(src).map_err(|e| SchemaParseError::from_syn(&e))?;
    let mut out = ParsedSchema {
        tables: Vec::new(),
        diagnostics: Vec::new(),
    };
    for item in &file.items {
        if let syn::Item::Struct(item_struct) = item
            && let Some(model_attr) = find_model_attr(&item_struct.attrs)
        {
            let table = build_table(item_struct, model_attr, backend, &mut out.diagnostics);
            out.tables.push(table);
        }
    }
    Ok(out)
}

/// Parse every `*.rs` file directly under `dir` (non-recursive) and aggregate
/// the results in a stable, filename-sorted order.
///
/// # Errors
///
/// Returns [`SchemaParseError::Io`] if `dir` (or a file within it) cannot be
/// read, or [`SchemaParseError::Syntax`] if any file is not valid Rust.
pub fn parse_models_dir(dir: &Path, backend: Backend) -> Result<ParsedSchema, SchemaParseError> {
    let read = std::fs::read_dir(dir).map_err(|source| SchemaParseError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in read {
        let entry = entry.map_err(|source| SchemaParseError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        // Query the entry's type directly (one syscall, from the DirEntry) rather
        // than an extra `Path::is_file` metadata stat, and skip anything that is
        // not a regular file — e.g. a subdirectory named `something.rs`.
        let file_type = entry.file_type().map_err(|source| SchemaParseError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            paths.push(path);
        }
    }
    // Deterministic aggregation regardless of directory iteration order.
    paths.sort();

    let mut out = ParsedSchema {
        tables: Vec::new(),
        diagnostics: Vec::new(),
    };
    for path in paths {
        let src = std::fs::read_to_string(&path).map_err(|source| SchemaParseError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let mut parsed = parse_model_source(&src, backend)?;
        out.tables.append(&mut parsed.tables);
        out.diagnostics.append(&mut parsed.diagnostics);
    }
    Ok(out)
}

/// Parse the `#[model]` structs at `path`, dispatching on what the path is:
/// a DIRECTORY is walked via [`parse_models_dir`]; a `.rs` FILE is read and
/// parsed via [`parse_model_source`] (the single-file `src/models.rs` layout);
/// a path that is neither (does not exist) yields a clear error naming it.
///
/// This is the one file-vs-directory dispatch both `autumn schema parse` and
/// `autumn schema snapshot` route through, so the two commands accept the same
/// `src/models/` (directory) and single-file `src/models.rs` layouts and can
/// never drift.
///
/// # Errors
///
/// Returns [`SchemaParseError::Io`] if `path` does not exist or cannot be read,
/// or [`SchemaParseError::Syntax`] if a file is not valid Rust.
pub fn parse_models_path(path: &Path, backend: Backend) -> Result<ParsedSchema, SchemaParseError> {
    if path.is_dir() {
        parse_models_dir(path, backend)
    } else if path.is_file() {
        let src = std::fs::read_to_string(path).map_err(|source| SchemaParseError::Io {
            path: path.display().to_string(),
            source,
        })?;
        parse_model_source(&src, backend)
    } else {
        Err(SchemaParseError::Io {
            path: path.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "expected a `.rs` model file or a directory of model files",
            ),
        })
    }
}

/// Find the `#[model]` / `#[autumn_web::model]` attribute on a struct, if any.
/// The last path segment must be `model`, so both the bare and fully-qualified
/// spellings match.
fn find_model_attr(attrs: &[syn::Attribute]) -> Option<&syn::Attribute> {
    attrs.iter().find(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|seg| seg.ident == "model")
    })
}

/// Struct-level `#[model(...)]` arguments the parser recognizes.
#[derive(Default)]
struct ModelArgs {
    table: Option<String>,
    managed: bool,
}

/// Parse the recognized `#[model(...)]` arguments (`table = "..."`, `managed`,
/// `schema = "managed"`), leniently ignoring any other nested argument so a
/// future macro argument does not break the parser.
fn parse_model_args(attr: &syn::Attribute) -> ModelArgs {
    let mut args = ModelArgs::default();
    // A bare `#[model]` (path, no parens) carries no arguments.
    if matches!(attr.meta, syn::Meta::Path(_)) {
        return args;
    }
    // `parse_nested_meta` errors on the first argument whose value we do not
    // consume; to stay forward-compatible we consume-and-ignore unknown ones so
    // a future `#[model(...)]` argument never breaks parsing of the known ones.
    let _ = attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("table")
            && let Ok(value) = meta.value()
            && let Ok(lit) = value.parse::<syn::LitStr>()
        {
            args.table = Some(lit.value());
        } else if meta.path.is_ident("managed") {
            args.managed = true;
        } else if meta.path.is_ident("schema")
            && let Ok(value) = meta.value()
            && let Ok(lit) = value.parse::<syn::LitStr>()
            && lit.value() == "managed"
        {
            args.managed = true;
        } else if meta.input.peek(syn::token::Paren) {
            // Unknown nested list `key(...)` — consume it so parsing can
            // continue to the next comma-separated argument.
            let _ = meta.parse_nested_meta(|_| Ok(()));
        } else if let Ok(value) = meta.value() {
            // Unknown `key = value` — consume the value token so parsing can
            // continue to the next comma-separated argument.
            let _ = value.parse::<syn::Lit>();
        }
        Ok(())
    });
    args
}

/// How a field participates in a foreign-key relationship.
#[derive(Default, PartialEq, Eq)]
enum ReferenceSpec {
    /// Not a `#[references]` field.
    #[default]
    None,
    /// Bare `#[references]` — infer the target table from the column name.
    Inferred,
    /// `#[references(table = "...")]` — explicit target-table override.
    Explicit(String),
}

/// A field's parsed schema-relevant attributes.
#[allow(clippy::struct_excessive_bools)] // independent field markers, not a state machine
struct FieldAttrs {
    is_id: bool,
    is_indexed: bool,
    is_unique: bool,
    is_default: bool,
    reference: ReferenceSpec,
}

fn parse_field_attrs(field: &syn::Field) -> FieldAttrs {
    let mut out = FieldAttrs {
        is_id: false,
        is_indexed: false,
        is_unique: false,
        is_default: false,
        reference: ReferenceSpec::None,
    };
    for attr in &field.attrs {
        let Some(ident) = attr.path().get_ident() else {
            continue;
        };
        match ident.to_string().as_str() {
            "id" => out.is_id = true,
            "indexed" => out.is_indexed = true,
            "unique" => out.is_unique = true,
            // `#[default]` gates the `created_at` NOW() convention default (see
            // `convention_default`): the generator always marks its `created_at`
            // with `#[default]`, so a hand-written `created_at` WITHOUT the marker
            // gets no inferred NOW() default.
            "default" => out.is_default = true,
            "references" => {
                let mut target = None;
                if !matches!(attr.meta, syn::Meta::Path(_)) {
                    // Extract the known `table = "..."` override, but consume any
                    // other nested list / `key = value` pair so a future arg
                    // (e.g. `on_delete = "cascade"`) never aborts target
                    // extraction.
                    let _ = attr.parse_nested_meta(|meta| {
                        if meta.path.is_ident("table")
                            && let Ok(value) = meta.value()
                            && let Ok(lit) = value.parse::<syn::LitStr>()
                        {
                            target = Some(lit.value());
                        } else if meta.input.peek(syn::token::Paren) {
                            let _ = meta.parse_nested_meta(|_| Ok(()));
                        } else if let Ok(value) = meta.value() {
                            let _ = value.parse::<syn::Lit>();
                        }
                        Ok(())
                    });
                }
                out.reference = target.map_or(ReferenceSpec::Inferred, ReferenceSpec::Explicit);
            }
            _ => {}
        }
    }
    out
}

/// A field lifted out of the struct with its name, parsed attributes, and Rust
/// type (already split on `Option<…>`).
struct RawField {
    name: String,
    attrs: FieldAttrs,
    nullable: bool,
    rust_type: String,
}

/// Build a [`Table`] from one `#[model]` struct.
#[allow(clippy::too_many_lines)]
fn build_table(
    item: &syn::ItemStruct,
    model_attr: &syn::Attribute,
    backend: Backend,
    diagnostics: &mut Vec<SchemaDiagnostic>,
) -> Table {
    let model_name = item.ident.to_string();
    let args = parse_model_args(model_attr);
    let table_name = args
        .table
        .clone()
        .unwrap_or_else(|| naming::pluralize(&naming::pascal_to_snake(&model_name)));

    // Lift the named fields (a tuple/unit struct carries no columns).
    let mut raw_fields: Vec<RawField> = Vec::new();
    if let syn::Fields::Named(named) = &item.fields {
        for field in &named.named {
            let Some(ident) = &field.ident else { continue };
            let (nullable, inner_ty) = strip_option(&field.ty);
            raw_fields.push(RawField {
                name: ident.to_string(),
                attrs: parse_field_attrs(field),
                nullable,
                rust_type: type_to_string(inner_ty),
            });
        }
    }

    // Primary-key resolution, mirroring the `#[model]` macro:
    //   1. explicit `#[id]` fields win;
    //   2. else the first `i32`/`i64` field;
    //   3. else a synthesized reserved `id`.
    let pk_field_names = resolve_pk_field_names(&raw_fields);
    let needs_synthetic_id = pk_field_names.is_empty();

    let mut table = Table::new(table_name.clone(), backend);
    table.managed = args.managed;

    // (1) A synthesized reserved `id` PK leads the column list (BigSerial
    // convention: an `Int64` PK with no explicit default).
    if needs_synthetic_id {
        let mut id = Column::new("id", ColumnType::Int64);
        id.primary_key = true;
        table.columns.push(id);
        table.primary_key.push("id".to_owned());
    }

    // Collected index sources, mirroring the generator's emission order.
    let mut plain_index_fields: Vec<String> = Vec::new();
    let mut unique_index_fields: Vec<String> = Vec::new();
    let mut has_created_at = false;

    for raw in &raw_fields {
        if raw.name == "created_at" {
            has_created_at = true;
        }
        let is_pk = pk_field_names.contains(&raw.name);

        // Resolve the column type. A `#[references]` field is always an `Int64`
        // FK column regardless of the written Rust type (which is `i64` anyway).
        let is_reference = raw.attrs.reference != ReferenceSpec::None;
        let column_ty = if is_reference {
            Some(ColumnType::Int64)
        } else {
            ColumnType::from_rust_type(&raw.rust_type)
        };
        let Some(ty) = column_ty else {
            // Unknown/unsupported type (a generated enum, an association type):
            // record a diagnostic and skip — never fabricate a wrong mapping.
            diagnostics.push(SchemaDiagnostic {
                model: model_name.clone(),
                table: table_name.clone(),
                field: raw.name.clone(),
                rust_type: raw.rust_type.clone(),
                message: format!(
                    "unsupported field type `{}` on `{model_name}.{}`; column skipped \
                     (enum/CHECK + association resolution is a later slice)",
                    raw.rust_type, raw.name
                ),
            });
            continue;
        };

        let mut column = Column::new(raw.name.clone(), ty.clone());
        column.nullable = raw.nullable;
        column.primary_key = is_pk;
        column.unique = raw.attrs.is_unique;

        // Default (slice-3+): recover only the DB column defaults the generator
        // emits *by convention* (see `convention_default`) — the `#[default]`-marked
        // `created_at` NOW() default and a Postgres UUID primary key's
        // `gen_random_uuid()`. The convention is a fallback: it never overwrites an
        // already-parsed explicit default. A hand-written `created_at` without
        // `#[default]`, and other non-convention `#[default]` fields (a soft-delete
        // `deleted_at`, an enum/numeric literal), carry no inferred DB default —
        // their value lives in the migration, not the struct.
        column.default = column
            .default
            .or_else(|| convention_default(&raw.name, &ty, is_pk, raw.attrs.is_default, backend));

        // Foreign key: infer the target table from the generator's convention
        // (`<name>` minus a trailing `_id`, pluralized) unless overridden.
        match &raw.attrs.reference {
            ReferenceSpec::None => {}
            ReferenceSpec::Inferred => {
                let base = raw.name.strip_suffix("_id").unwrap_or(&raw.name);
                column.references = Some(ForeignKey::new(naming::pluralize(base), "id"));
            }
            ReferenceSpec::Explicit(target) => {
                column.references = Some(ForeignKey::new(target.clone(), "id"));
            }
        }

        // Index bookkeeping (deduped/retained below, mirroring the generator).
        if raw.attrs.is_unique {
            unique_index_fields.push(raw.name.clone());
        }
        if raw.attrs.is_indexed || is_reference {
            plain_index_fields.push(raw.name.clone());
        }

        // Record a struct-declared PK column name (a synthesized `id` was already
        // pushed above; an explicit PK is pushed here in declaration order).
        if is_pk {
            table.primary_key.push(raw.name.clone());
        }

        table.columns.push(column);
    }

    // (2) Synthesize `created_at TIMESTAMP DEFAULT NOW()` when the struct did not
    // declare it, matching what the generator always appends.
    if !has_created_at {
        let mut created = Column::new("created_at", ColumnType::Timestamp);
        created.default = Some(ColumnDefault::Now);
        table.columns.push(created);
    }

    // Index emission, mirroring `schema_edit::create_table_sql_*`:
    //  - every `references` column gets an auto-index (already folded into
    //    `plain_index_fields`);
    //  - a `unique` field's own unique index covers lookups, so it must not
    //    also get a redundant plain index (issue #1032).
    plain_index_fields.retain(|name| !unique_index_fields.contains(name));
    plain_index_fields.dedup();
    for field_name in &plain_index_fields {
        table.indexes.push(Index {
            name: format!("idx_{table_name}_{field_name}"),
            columns: vec![field_name.clone()],
            unique: false,
            definition: None,
            is_partial: false,
            key_columns: Vec::new(),
        });
    }
    for field_name in &unique_index_fields {
        table.indexes.push(Index {
            // Plain (un-disambiguated) form; 63-byte disambiguation is a later slice.
            name: format!("idx_{table_name}_{field_name}_unique"),
            columns: vec![field_name.clone()],
            unique: true,
            definition: None,
            is_partial: false,
            key_columns: Vec::new(),
        });
    }

    table
}

/// Recover the DB column default the generator emits *by convention*, so the
/// desired-state IR matches what `autumn generate` actually produced.
///
/// Known conventions:
/// - the reserved `created_at` (timestamp) column marked `#[default]` -> `NOW()`
///   / `CURRENT_TIMESTAMP`. The convention requires the generator's `#[default]`
///   marker: the generator ALWAYS marks its `created_at` with `#[default]`, so
///   gating on it matches generated output exactly. A hand-written `created_at`
///   timestamp WITHOUT `#[default]` gets no inferred default (its DDL is
///   `created_at TIMESTAMP NOT NULL` with no default clause). The default is
///   likewise not inferred for any other `#[default]` timestamp such as a
///   soft-delete `deleted_at`.
/// - a UUID `PRIMARY KEY` on Postgres -> `gen_random_uuid()` (the only primary
///   key that carries an explicit column `DEFAULT`; a `BIGSERIAL`/serial PK is
///   type-encoded with no `DEFAULT` clause, and on sqlite a UUID PK is `TEXT
///   PRIMARY KEY` with no default — indeed `uuid::Uuid` is a rejected type there).
///   A `#[id]` PK is not `#[default]`-marked, so this arm is NOT gated on
///   `is_default`.
///
/// Everything else is `None` — a non-convention default lives in the migration
/// and is recovered in a later slice. This is a *fallback*: the caller applies it
/// only when a column has no already-parsed explicit default.
fn convention_default(
    name: &str,
    ty: &ColumnType,
    is_primary_key: bool,
    is_default: bool,
    backend: Backend,
) -> Option<ColumnDefault> {
    if name == "created_at"
        && is_default
        && matches!(ty, ColumnType::Timestamp | ColumnType::TimestampTz)
    {
        return Some(ColumnDefault::Now);
    }
    if is_primary_key && *ty == ColumnType::Uuid && backend == Backend::Postgres {
        return Some(ColumnDefault::Sql("gen_random_uuid()".to_owned()));
    }
    None
}

/// Resolve the primary-key column name(s) from the lifted fields, mirroring the
/// `#[model]` macro: explicit `#[id]` fields win; else the first `i32`/`i64`
/// field; else empty (the caller synthesizes a reserved `id`).
fn resolve_pk_field_names(fields: &[RawField]) -> Vec<String> {
    let explicit: Vec<String> = fields
        .iter()
        .filter(|f| f.attrs.is_id)
        .map(|f| f.name.clone())
        .collect();
    if !explicit.is_empty() {
        return explicit;
    }
    fields
        .iter()
        .find(|f| !f.nullable && matches!(f.rust_type.as_str(), "i32" | "i64"))
        .map_or_else(Vec::new, |f| vec![f.name.clone()])
}

/// Split a `Option<T>` type into `(true, T)`; any other type is `(false, ty)`.
/// Tolerates `std::option::Option<T>` / `core::option::Option<T>` spellings.
fn strip_option(ty: &syn::Type) -> (bool, &syn::Type) {
    if let syn::Type::Path(type_path) = ty
        && type_path.qself.is_none()
        && let Some(seg) = type_path.path.segments.last()
        && seg.ident == "Option"
        && let syn::PathArguments::AngleBracketed(args) = &seg.arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return (true, inner);
    }
    (false, ty)
}

/// Render a `syn::Type` to a string [`ColumnType::from_rust_type`] accepts. That
/// function normalizes whitespace and is path-tolerant, so the spacing `quote`
/// introduces (`Vec < u8 >`, `chrono :: NaiveDateTime`) is handled there.
fn type_to_string(ty: &syn::Type) -> String {
    quote::quote!(#ty).to_string()
}

#[cfg(test)]
// Fixtures use a uniform `r#"…"#` raw-string style (several embed `"`); keeping
// the rest bare would be inconsistent noise.
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;

    fn parse_one(src: &str) -> Table {
        let parsed = parse_model_source(src, Backend::Postgres).expect("parse");
        assert_eq!(parsed.tables.len(), 1, "expected exactly one table");
        parsed.tables.into_iter().next().unwrap()
    }

    fn col<'a>(table: &'a Table, name: &str) -> &'a Column {
        table
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("column `{name}` not found in {:?}", table.name))
    }

    #[test]
    fn basic_model_id_string_option_bool_created_at() {
        let src = r#"
            #[autumn_web::model]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                pub views: Option<i32>,
                pub published: bool,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(table.name, "posts");
        assert!(!table.managed, "unmarked model is not managed");
        assert_eq!(table.backend, Backend::Postgres);
        assert_eq!(table.primary_key, vec!["id".to_owned()]);

        let id = col(&table, "id");
        assert_eq!(id.ty, ColumnType::Int64);
        assert!(id.primary_key);

        let title = col(&table, "title");
        assert_eq!(title.ty, ColumnType::Text);
        assert!(!title.nullable);

        let views = col(&table, "views");
        assert_eq!(views.ty, ColumnType::Int32);
        assert!(views.nullable, "Option<i32> is nullable");

        let published = col(&table, "published");
        assert_eq!(published.ty, ColumnType::Bool);

        let created = col(&table, "created_at");
        assert_eq!(created.ty, ColumnType::Timestamp);
        assert_eq!(created.default, Some(ColumnDefault::Now));
    }

    #[test]
    fn full_basic_table_equals_hand_built() {
        let src = r#"
            #[autumn_web::model]
            pub struct Widget {
                #[id]
                pub id: i64,
                pub name: String,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);

        let mut expected = Table::new("widgets", Backend::Postgres);
        // An unmarked `#[model]` is not managed (Table::new defaults to true).
        expected.managed = false;
        let mut id = Column::new("id", ColumnType::Int64);
        id.primary_key = true;
        expected.columns.push(id);
        expected.columns.push(Column::new("name", ColumnType::Text));
        let mut created = Column::new("created_at", ColumnType::Timestamp);
        created.default = Some(ColumnDefault::Now);
        expected.columns.push(created);
        expected.primary_key.push("id".to_owned());

        assert_eq!(table, expected);
    }

    #[test]
    fn table_override_is_honored() {
        let src = r#"
            #[autumn_web::model(table = "people")]
            pub struct Person {
                #[id]
                pub id: i64,
                pub name: String,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(table.name, "people");
    }

    #[test]
    fn managed_marker_sets_managed_true() {
        let marked = parse_one(
            r#"
            #[model(managed)]
            pub struct Account {
                #[id]
                pub id: i64,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#,
        );
        assert!(marked.managed, "#[model(managed)] opts in");

        let via_schema = parse_one(
            r#"
            #[model(schema = "managed")]
            pub struct Account {
                #[id]
                pub id: i64,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#,
        );
        assert!(via_schema.managed, "#[model(schema = \"managed\")] opts in");

        let unmarked = parse_one(
            r#"
            #[model]
            pub struct Account {
                #[id]
                pub id: i64,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#,
        );
        assert!(!unmarked.managed, "unmarked model defaults to unmanaged");
    }

    #[test]
    fn references_field_yields_fk_int64_and_auto_index() {
        let src = r#"
            #[model]
            pub struct Comment {
                #[id]
                pub id: i64,
                #[references]
                pub post_id: i64,
                pub body: String,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        let post = col(&table, "post_id");
        assert_eq!(post.ty, ColumnType::Int64);
        assert_eq!(post.references, Some(ForeignKey::new("posts", "id")));

        // The reference column gets an auto-index, mirroring the generator.
        let idx = table
            .indexes
            .iter()
            .find(|i| i.columns == vec!["post_id".to_owned()])
            .expect("auto-index on the reference column");
        assert_eq!(idx.name, "idx_comments_post_id");
        assert!(!idx.unique);
    }

    #[test]
    fn references_table_override() {
        let src = r#"
            #[model]
            pub struct Comment {
                #[id]
                pub id: i64,
                #[references(table = "blog_posts")]
                pub post_id: i64,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(
            col(&table, "post_id").references,
            Some(ForeignKey::new("blog_posts", "id"))
        );
    }

    #[test]
    fn now_default_only_for_created_at_not_other_timestamps() {
        // A `#[default]` on a non-`created_at` timestamp field must NOT infer a
        // NOW() default: the generator gives a DB `DEFAULT NOW()` only to the
        // reserved `created_at` column. A non-`created_at` `#[default]`'s DB
        // default (if any) lives in the migration, not the struct.
        let src = r#"
            #[model]
            pub struct Event {
                #[id]
                pub id: i64,
                #[default]
                pub occurred_at: chrono::NaiveDateTime,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(col(&table, "occurred_at").default, None);
        assert_eq!(col(&table, "created_at").default, Some(ColumnDefault::Now));
    }

    #[test]
    fn hand_written_created_at_without_default_marker_has_no_inferred_default() {
        // The `created_at` NOW() convention is gated on the generator's
        // `#[default]` marker. A hand-written model that declares a `created_at`
        // timestamp WITHOUT `#[default]` has DDL `created_at TIMESTAMP NOT NULL`
        // with NO default — so the parser must report `default == None`, or a
        // later snapshot/diff would spuriously try to ADD a NOW() default.
        let src = r#"
            #[autumn_web::model]
            pub struct Event {
                #[id]
                pub id: i64,
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        let created = col(&table, "created_at");
        assert_eq!(created.ty, ColumnType::Timestamp);
        assert_eq!(created.default, None);
    }

    #[test]
    fn hand_written_agg_event_created_at_has_no_inferred_default() {
        // An inline copy of `AggEvent` from
        // `autumn/tests/integration/repository_grouped_aggregates.rs`, a real
        // hand-written model whose `created_at` carries no `#[default]` and whose
        // DDL is `created_at TIMESTAMP NOT NULL` (no default). The parser must not
        // over-infer a NOW() default for it.
        let src = r#"
            #[autumn_web::model(table = "test_agg_events")]
            pub struct AggEvent {
                #[id]
                pub id: i64,
                pub post_id: i64,
                pub score: i32,
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(table.name, "test_agg_events");
        assert_eq!(col(&table, "created_at").default, None);
    }

    #[test]
    fn soft_delete_deleted_at_has_no_inferred_default() {
        // The generator emits a nullable `deleted_at` with `#[default]` ONLY to
        // exclude it from the `New*`/`Update*` insert structs — the migration
        // leaves its column default UNSET, so new rows stay NULL. The parser must
        // report `default == None` (never `Some(Now)`), or a later snapshot/diff
        // would read a spurious "add DEFAULT NOW()" change that, if applied, would
        // make newly inserted rows appear soft-deleted.
        let src = r#"
            #[model]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                #[default]
                pub deleted_at: Option<chrono::NaiveDateTime>,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        let deleted_at = col(&table, "deleted_at");
        assert_eq!(deleted_at.default, None);
        assert!(deleted_at.nullable);
        // `created_at` still recovers its NOW() default.
        assert_eq!(col(&table, "created_at").default, Some(ColumnDefault::Now));
    }

    #[test]
    fn uuid_primary_key_recovers_gen_random_uuid_default_on_postgres() {
        // `autumn generate model --id uuid` emits `#[id] pub id: uuid::Uuid`, and
        // its Postgres migration declares the PK as
        // `id UUID PRIMARY KEY DEFAULT gen_random_uuid()`. The parser must recover
        // that convention default, or a later snapshot/diff would read the live
        // DB's `gen_random_uuid()` default as drift and try to REMOVE it.
        let src = r#"
            #[autumn_web::model]
            pub struct Session {
                #[id]
                pub id: uuid::Uuid,
                pub token: String,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        let id = col(&table, "id");
        assert_eq!(id.ty, ColumnType::Uuid);
        assert!(id.primary_key);
        assert_eq!(
            id.default,
            Some(ColumnDefault::Sql("gen_random_uuid()".to_owned()))
        );
    }

    #[test]
    fn non_pk_uuid_field_has_no_inferred_default() {
        // The `gen_random_uuid()` convention is PRIMARY-KEY-only: a plain
        // `uuid::Uuid` column carries no DB default (its value, if any, lives in
        // the migration), so the parser must report `default == None` for it.
        let src = r#"
            #[autumn_web::model]
            pub struct ApiKey {
                #[id]
                pub id: i64,
                pub external_ref: uuid::Uuid,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        let external = col(&table, "external_ref");
        assert_eq!(external.ty, ColumnType::Uuid);
        assert!(!external.primary_key);
        assert_eq!(external.default, None);
    }

    #[test]
    fn bigserial_primary_key_has_no_inferred_default() {
        // A `BIGSERIAL PRIMARY KEY` is type-encoded — the serial type IS the
        // default, there is no separate `DEFAULT` clause — so an `i64` `#[id]` PK
        // must NOT pick up `gen_random_uuid()` (which is UUID-PK-only).
        let src = r#"
            #[autumn_web::model]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        let id = col(&table, "id");
        assert_eq!(id.ty, ColumnType::Int64);
        assert!(id.primary_key);
        assert_eq!(id.default, None);
    }

    #[test]
    fn indexed_field_produces_index() {
        let src = r#"
            #[model]
            pub struct User {
                #[id]
                pub id: i64,
                #[indexed]
                pub slug: String,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        let idx = table
            .indexes
            .iter()
            .find(|i| i.columns == vec!["slug".to_owned()])
            .expect("index on the #[indexed] column");
        assert_eq!(idx.name, "idx_users_slug");
        assert!(!idx.unique);
    }

    #[test]
    fn unique_field_produces_unique_index_and_no_plain_index() {
        let src = r#"
            #[model]
            pub struct Account {
                #[id]
                pub id: i64,
                #[unique]
                #[indexed]
                pub email: String,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert!(col(&table, "email").unique);

        let email_indexes: Vec<&Index> = table
            .indexes
            .iter()
            .filter(|i| i.columns == vec!["email".to_owned()])
            .collect();
        assert_eq!(
            email_indexes.len(),
            1,
            "a unique field must not also get a redundant plain index"
        );
        assert!(email_indexes[0].unique);
        assert_eq!(email_indexes[0].name, "idx_accounts_email_unique");
    }

    #[test]
    fn each_scalar_type_maps_correctly() {
        let src = r#"
            #[model]
            pub struct Kitchen {
                #[id]
                pub id: i64,
                pub a: String,
                pub b: i32,
                pub c: i64,
                pub d: bool,
                pub e: f32,
                pub f: f64,
                pub g: uuid::Uuid,
                pub h: chrono::NaiveDateTime,
                pub i: chrono::DateTime<chrono::Utc>,
                pub j: Vec<u8>,
                pub k: autumn_web::storage::Blob,
                pub l: rust_decimal::Decimal,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(col(&table, "a").ty, ColumnType::Text);
        assert_eq!(col(&table, "b").ty, ColumnType::Int32);
        assert_eq!(col(&table, "c").ty, ColumnType::Int64);
        assert_eq!(col(&table, "d").ty, ColumnType::Bool);
        assert_eq!(col(&table, "e").ty, ColumnType::Float32);
        assert_eq!(col(&table, "f").ty, ColumnType::Float64);
        assert_eq!(col(&table, "g").ty, ColumnType::Uuid);
        assert_eq!(col(&table, "h").ty, ColumnType::Timestamp);
        assert_eq!(col(&table, "i").ty, ColumnType::TimestampTz);
        assert_eq!(col(&table, "j").ty, ColumnType::Bytes);
        assert_eq!(col(&table, "k").ty, ColumnType::Attachment);
        assert_eq!(
            col(&table, "l").ty,
            ColumnType::Decimal {
                precision: 12,
                scale: 2
            }
        );
    }

    #[test]
    fn unknown_field_type_produces_diagnostic_not_wrong_mapping() {
        let src = r#"
            #[model]
            pub struct Post {
                #[id]
                pub id: i64,
                pub title: String,
                pub status: PostStatus,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let parsed = parse_model_source(src, Backend::Postgres).expect("parse");
        let table = &parsed.tables[0];
        // The unresolved column is skipped, not fabricated.
        assert!(table.columns.iter().all(|c| c.name != "status"));
        assert_eq!(parsed.diagnostics.len(), 1);
        let diag = &parsed.diagnostics[0];
        assert_eq!(diag.model, "Post");
        // Convention table name recorded on the diagnostic.
        assert_eq!(diag.table, "posts");
        assert_eq!(diag.field, "status");
        assert!(diag.rust_type.contains("PostStatus"));
    }

    #[test]
    fn diagnostic_records_custom_table_name() {
        // A `#[model(table = "app_users")]` model with an unresolved field must
        // record the RESOLVED custom table name (`app_users`), not the convention
        // name (`users`) — so a downstream consumer (the diff engine's
        // parser-gap suppression) can match the diagnostic to the actual baseline
        // table and never diff a present-but-unmodelled column as a drop.
        let src = r#"
            #[model(table = "app_users")]
            pub struct User {
                #[id]
                pub id: i64,
                pub name: String,
                pub role: UserRole,
            }
        "#;
        let parsed = parse_model_source(src, Backend::Postgres).expect("parse");
        assert_eq!(parsed.tables[0].name, "app_users");
        assert_eq!(parsed.diagnostics.len(), 1);
        let diag = &parsed.diagnostics[0];
        assert_eq!(diag.model, "User");
        assert_eq!(diag.table, "app_users");
        assert_eq!(diag.field, "role");
    }

    #[test]
    fn synthesizes_reserved_id_and_created_at_when_absent() {
        // No `#[id]`, no i32/i64 field, no `created_at`.
        let src = r#"
            #[model]
            pub struct Note {
                pub body: String,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(table.primary_key, vec!["id".to_owned()]);
        let id = col(&table, "id");
        assert_eq!(id.ty, ColumnType::Int64);
        assert!(id.primary_key);
        // Synthesized id leads, created_at trails.
        assert_eq!(table.columns.first().unwrap().name, "id");
        let created = col(&table, "created_at");
        assert_eq!(created.ty, ColumnType::Timestamp);
        assert_eq!(created.default, Some(ColumnDefault::Now));
    }

    #[test]
    fn pk_falls_back_to_first_integer_field_without_id_attr() {
        let src = r#"
            #[model]
            pub struct Legacy {
                pub legacy_key: i64,
                pub name: String,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(table.primary_key, vec!["legacy_key".to_owned()]);
        assert!(col(&table, "legacy_key").primary_key);
        // No synthesized `id` when a fallback PK field exists.
        assert!(table.columns.iter().all(|c| c.name != "id"));
    }

    #[test]
    fn non_model_structs_are_ignored() {
        let src = r#"
            pub struct NotAModel {
                pub x: i64,
            }
            #[model]
            pub struct Real {
                #[id]
                pub id: i64,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let parsed = parse_model_source(src, Backend::Postgres).expect("parse");
        assert_eq!(parsed.tables.len(), 1);
        assert_eq!(parsed.tables[0].name, "reals");
    }

    #[test]
    fn syntax_error_is_reported_with_location() {
        let err = parse_model_source("pub struct { ", Backend::Postgres).unwrap_err();
        assert!(matches!(err, SchemaParseError::Syntax { .. }));
    }

    #[test]
    fn parses_a_real_repo_model_file() {
        // A real generated model in the checked-in `bookmarks` example. If the
        // example is ever moved, skip rather than fail this crate's tests.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../examples/bookmarks/src/models/bookmark.rs");
        let Ok(src) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: {} not found", path.display());
            return;
        };
        let parsed = parse_model_source(&src, Backend::Postgres).expect("real model parses");
        assert_eq!(parsed.tables.len(), 1);
        let table = &parsed.tables[0];
        assert_eq!(table.name, "bookmarks");
        assert_eq!(table.primary_key, vec!["id".to_owned()]);
        assert_eq!(col(table, "url").ty, ColumnType::Text);
        assert_eq!(col(table, "alive").ty, ColumnType::Bool);
        assert_eq!(col(table, "created_at").default, Some(ColumnDefault::Now));
        // `#[indexed]` on `url` and `tag` produces two plain indexes.
        assert!(
            table
                .indexes
                .iter()
                .any(|i| i.name == "idx_bookmarks_url" && !i.unique)
        );
        assert!(
            table
                .indexes
                .iter()
                .any(|i| i.name == "idx_bookmarks_tag" && !i.unique)
        );
    }

    #[test]
    fn sqlite_backend_is_carried_through() {
        let src = r#"
            #[model]
            pub struct Thing {
                #[id]
                pub id: i64,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let parsed = parse_model_source(src, Backend::Sqlite).expect("parse");
        assert_eq!(parsed.tables[0].backend, Backend::Sqlite);
    }

    #[test]
    fn model_unknown_kv_arg_is_ignored_and_known_args_survive() {
        // A future `key = value` arg must not abort parsing of `table`.
        let src = r#"
            #[model(table = "widgets", future_unknown = "x")]
            pub struct Widget {
                #[id]
                pub id: i64,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(table.name, "widgets");
    }

    #[test]
    fn model_unknown_nested_list_arg_is_consumed_and_managed_survives() {
        // A future nested-list arg `key(...)` must be consumed, not abort the
        // `managed` marker that precedes it.
        let src = r#"
            #[model(managed, future_list(a = 1, b = 2))]
            pub struct Account {
                #[id]
                pub id: i64,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert!(table.managed, "unknown nested list must not drop `managed`");
    }

    #[test]
    fn references_unknown_kv_arg_is_ignored_and_target_survives() {
        // A future `#[references(table = "...", on_delete = "...")]` arg must
        // not abort extraction of the `table` target.
        let src = r#"
            #[model]
            pub struct Comment {
                #[id]
                pub id: i64,
                #[references(table = "users", on_delete = "cascade")]
                pub author_id: i64,
                #[default]
                pub created_at: chrono::NaiveDateTime,
            }
        "#;
        let table = parse_one(src);
        assert_eq!(
            col(&table, "author_id").references,
            Some(ForeignKey::new("users", "id"))
        );
    }

    const SINGLE_FILE_MODEL: &str = r#"
        #[model]
        pub struct Post {
            #[id]
            pub id: i64,
            pub body: String,
        }
    "#;

    #[test]
    fn parse_models_path_reads_a_single_rs_file() {
        // The single-file `src/models.rs` layout: `--from` points at a `.rs`
        // file, not a directory.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("models.rs");
        std::fs::write(&file, SINGLE_FILE_MODEL).expect("write model file");

        let parsed = parse_models_path(&file, Backend::Postgres).expect("parse single file");
        assert_eq!(parsed.tables.len(), 1);
        assert_eq!(parsed.tables[0].name, "posts");
    }

    #[test]
    fn parse_models_path_reads_a_directory() {
        // The `src/models/` directory layout with one `*.rs` file.
        let dir = tempfile::tempdir().expect("tempdir");
        let models_dir = dir.path().join("models");
        std::fs::create_dir(&models_dir).expect("mkdir");
        std::fs::write(models_dir.join("post.rs"), SINGLE_FILE_MODEL).expect("write model file");

        let parsed = parse_models_path(&models_dir, Backend::Postgres).expect("parse dir");
        assert_eq!(parsed.tables.len(), 1);
        assert_eq!(parsed.tables[0].name, "posts");
    }

    #[test]
    fn parse_models_path_missing_path_is_a_clear_io_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does_not_exist.rs");

        let err = parse_models_path(&missing, Backend::Postgres).unwrap_err();
        match err {
            SchemaParseError::Io { path, .. } => {
                assert!(path.contains("does_not_exist.rs"), "path names the input");
            }
            other @ SchemaParseError::Syntax { .. } => panic!("expected Io error, got {other:?}"),
        }
    }
}
