//! Field-type DSL parser for `autumn generate`.
//!
//! Turns command-line tokens like `title:String`, `tags:Vec<u8>`, or
//! `published:Option<bool>` into a structured [`Field`] that knows both its
//! Rust type (for the `#[model]` struct) and its SQL type (for the migration).

use super::GenerateError;
use super::naming;

/// The constraint modifiers a field carried in a trailing `{…}` block
/// (`title:String{min=3,max=120}`, `contact:String{email}`,
/// `age:i32{min=0,max=130}`, `post:references{label:title}`).
///
/// These fan out to BOTH a server-side `#[validate(…)]` rule (issue #1388,
/// see [`Field::validation_attrs`]) and a matching client-side HTML5
/// constraint on the generated form input, from a single declaration.
/// `label` is the odd one out: it's a `references`-only display-column
/// override (issue #1146) with no `#[validate]`/HTML5 fan-out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldConstraints {
    /// `min=N`. For `String`/`Text` this is a `length` minimum; for numeric
    /// fields it is a `range` minimum. Stored as the raw numeric token.
    pub min: Option<String>,
    /// `max=N`. `length` maximum (`String`/`Text`) or `range` maximum
    /// (numeric). Stored as the raw numeric token.
    pub max: Option<String>,
    /// `email` — `#[validate(email)]` + `type="email"`. `String`/`Text` only.
    pub email: bool,
    /// `url` — `#[validate(url)]` + `type="url"`. `String`/`Text` only.
    pub url: bool,
    /// `label:col` — the `references` display column an index/show view and a
    /// `belongs_to` `<select>` label render from (issue #1146). `references`
    /// only; never a `#[validate]`/HTML5 constraint.
    pub label: Option<String>,
}

impl FieldConstraints {
    /// True when no constraint modifier was declared.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.min.is_none() && self.max.is_none() && !self.email && !self.url && self.label.is_none()
    }
}

/// A single field parsed from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Column / struct field name (`snake_case`).
    pub name: String,
    /// Underlying type, ignoring `Option` wrapping.
    pub kind: FieldKind,
    /// True when the field was given as `Option<…>`.
    pub nullable: bool,
    /// The declared variant tokens (`snake_case`) for [`FieldKind::Enum`]
    /// fields, in declaration order. Empty for every other kind.
    pub variants: Vec<String>,
    /// True when the field was given a trailing `:unique` modifier
    /// (`email:String:unique`), or was later marked unique via `--unique`
    /// (see [`super::model::apply_unique_flags`]). Emits a `CREATE UNIQUE
    /// INDEX` in the migration instead of the plain, non-unique `--index`
    /// output (see [`super::schema_edit::unique_index_sql`]).
    pub unique: bool,
    /// Constraint modifiers from a trailing `{…}` block (issue #1388 /
    /// #1146). Empty ([`FieldConstraints::is_empty`]) for a bare `name:Type`.
    pub constraints: FieldConstraints,
}

impl Field {
    /// The Rust type for the `#[model]` struct.
    ///
    /// For [`FieldKind::Enum`], this is the generated enum's `PascalCase`
    /// name (see [`Field::enum_type_name`]) rather than [`FieldKind::rust_type`]'s
    /// `String` storage-representation fallback.
    #[must_use]
    pub fn rust_type(&self) -> String {
        let inner = self
            .enum_type_name()
            .unwrap_or_else(|| self.kind.rust_type().to_owned());
        if self.nullable {
            format!("Option<{inner}>")
        } else {
            inner
        }
    }

    /// For [`FieldKind::Enum`] fields, the `PascalCase` name of the generated
    /// Rust enum (`status` -> `Status`). `None` for every other field kind.
    #[must_use]
    pub fn enum_type_name(&self) -> Option<String> {
        self.kind.is_enum().then(|| naming::pascal(&self.name))
    }

    /// Returns `true` for a closed-set `enum{…}` field.
    #[must_use]
    pub const fn is_enum(&self) -> bool {
        self.kind.is_enum()
    }

    /// The Diesel `schema.rs` type token (always a single identifier).
    #[must_use]
    pub fn schema_type(&self) -> String {
        let inner = self.kind.schema_type();
        if self.nullable {
            format!("Nullable<{inner}>")
        } else {
            inner.to_string()
        }
    }

    /// The SQL column type, without nullability suffix.
    #[must_use]
    pub const fn sql_type(&self) -> &'static str {
        self.kind.sql_type()
    }

    /// The SQL column type used in `CREATE TABLE`/`ADD COLUMN`, including any
    /// type parameters. Identical to [`Field::sql_type`] for every kind
    /// except [`FieldKind::Decimal`], whose `precision`/`scale` can't be
    /// represented in `sql_type`'s `&'static str` return type — this method
    /// renders the full `NUMERIC(precision,scale)` instead.
    #[must_use]
    pub fn sql_column_type(&self) -> String {
        match self.kind {
            FieldKind::Decimal { precision, scale } => format!("NUMERIC({precision},{scale})"),
            _ => self.sql_type().to_owned(),
        }
    }

    /// `"NULL"` or `"NOT NULL"` to append in the migration.
    #[must_use]
    pub const fn sql_nullability(&self) -> &'static str {
        if self.nullable { "NULL" } else { "NOT NULL" }
    }

    /// The server-side `#[validate(…)]` argument list this field's
    /// constraint modifiers (issue #1388) fan out to — e.g.
    /// `["length(min = 3, max = 120)"]`, `["email"]`, `["range(min = 0, max = 130)"]`.
    ///
    /// `String`/`Text` fields map `min`/`max` to `length` and honor
    /// `email`/`url`; numeric fields map `min`/`max` to `range`. Float
    /// (`f32`/`f64`) range bounds are emitted with a decimal point so the
    /// generated comparison type-checks against the field's own type. Every
    /// other kind (and the `references`-only `label`) contributes nothing.
    /// Empty when the field declared no fan-out constraints.
    #[must_use]
    pub fn validation_attrs(&self) -> Vec<String> {
        let c = &self.constraints;
        let mut out = Vec::new();
        match self.kind {
            FieldKind::String | FieldKind::Text => {
                if c.min.is_some() || c.max.is_some() {
                    out.push(format!(
                        "length({})",
                        min_max_args(c.min.as_ref(), c.max.as_ref(), false)
                    ));
                }
                if c.email {
                    out.push("email".to_owned());
                }
                if c.url {
                    out.push("url".to_owned());
                }
            }
            FieldKind::I32 | FieldKind::I64 | FieldKind::F32 | FieldKind::F64 => {
                if c.min.is_some() || c.max.is_some() {
                    let is_float = matches!(self.kind, FieldKind::F32 | FieldKind::F64);
                    out.push(format!(
                        "range({})",
                        min_max_args(c.min.as_ref(), c.max.as_ref(), is_float)
                    ));
                }
            }
            _ => {}
        }
        out
    }

    /// For a [`FieldKind::References`] field, the referenced table name —
    /// the `_id` suffix is stripped from the column name and the remainder
    /// is pluralised via [`naming::pluralize`] (`post_id` -> `posts`).
    ///
    /// Returns `None` for every other field kind.
    #[must_use]
    pub fn reference_table(&self) -> Option<String> {
        if !self.kind.is_reference() {
            return None;
        }
        let base = self.name.strip_suffix("_id").unwrap_or(&self.name);
        Some(naming::pluralize(base))
    }
}

/// The supported field types. Mirrors the documented public surface in the
/// `autumn generate --help` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// `String` — `TEXT`.
    String,
    /// `Text` (alias for `String`) — `TEXT`.
    Text,
    /// `i32` — `INTEGER`.
    I32,
    /// `i64` — `BIGINT`.
    I64,
    /// `bool` — `BOOLEAN`.
    Bool,
    /// `f32` — `REAL`.
    F32,
    /// `f64` — `DOUBLE PRECISION`.
    F64,
    /// `Uuid` — `UUID`.
    Uuid,
    /// `NaiveDateTime` — `TIMESTAMP`.
    NaiveDateTime,
    /// `DateTime` — `TIMESTAMPTZ`.
    DateTime,
    /// `Vec<u8>` / `Bytea` — `BYTEA`.
    Bytea,
    /// `Blob` stored as `JSONB` — a file attachment with direct-upload support.
    ///
    /// Maps to a Postgres `JSONB` column that stores the `autumn_web::storage::Blob`
    /// metadata (key, content-type, byte-size, etag). The bytes themselves live
    /// in the configured storage backend (local disk or S3-compatible).
    ///
    /// Always emitted as `Option<autumn_web::storage::Blob>` in the model struct
    /// so the attachment is optional by default. Wrap in `Option<Attachment>` to
    /// be explicit, or leave as `Attachment` (equivalent: nullable is the default
    /// and safe choice for file fields).
    Attachment,
    /// `references` — a foreign-key column (`i64`/`BIGINT`), matching the
    /// default `i64` primary-key convention. The DSL rewrites the declared
    /// field name to end in `_id` (`post:references` -> `post_id`) and the
    /// referenced table is derived from the base name via [`naming::pluralize`]
    /// (`post` -> `posts`). See [`Field::reference_table`].
    References,
    /// `enum{a,b,c}` — a closed-set column. Stored as `TEXT` with a `CHECK`
    /// constraint enumerating the allowed values (see
    /// [`create_table_sql_with_metadata_and_id`](super::schema_edit::create_table_sql_with_metadata_and_id)),
    /// and rendered in the `#[model]` struct as a generated Rust enum rather
    /// than the bare `String` [`FieldKind::rust_type`] reports here — see
    /// [`Field::rust_type`] and [`Field::enum_type_name`], which override this
    /// storage-representation fallback with the real generated type name.
    Enum,
    /// `decimal{precision,scale}` (default `{12,2}` when the modifier is
    /// omitted) — an exact-precision `NUMERIC(precision,scale)` column.
    /// Unlike `f32`/`f64`, this never introduces binary-float rounding error,
    /// making it the correct choice for money and other exact-decimal values.
    /// Rendered in the `#[model]` struct as `rust_decimal::Decimal` (see
    /// [`FieldKind::rust_type`]) — the same decimal type the `autumn` runtime
    /// crate already re-exports and uses for its `number_to_currency` helper.
    Decimal {
        /// Total number of significant digits (`NUMERIC(precision, _)`).
        precision: u32,
        /// Number of digits after the decimal point (`NUMERIC(_, scale)`).
        scale: u32,
    },
}

impl FieldKind {
    /// Rust type token used inside `#[model]` structs.
    ///
    /// For [`Attachment`](Self::Attachment), always returns the inner `Blob`
    /// type. Nullability wrapping (`Option<…>`) is applied by [`Field::rust_type`].
    #[must_use]
    pub const fn rust_type(self) -> &'static str {
        match self {
            // `Enum`'s "String" here is a storage-representation fallback
            // only — `Field::rust_type()` overrides it with the generated
            // enum's real type name.
            Self::String | Self::Text | Self::Enum => "String",
            Self::I32 => "i32",
            // `References` is always `i64`, matching the default `i64` PK convention.
            Self::I64 | Self::References => "i64",
            Self::Bool => "bool",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Uuid => "uuid::Uuid",
            Self::NaiveDateTime => "chrono::NaiveDateTime",
            Self::DateTime => "chrono::DateTime<chrono::Utc>",
            Self::Bytea => "Vec<u8>",
            Self::Attachment => "autumn_web::storage::Blob",
            Self::Decimal { .. } => "rust_decimal::Decimal",
        }
    }

    /// Diesel `table!` schema type token.
    #[must_use]
    pub const fn schema_type(self) -> &'static str {
        match self {
            Self::String | Self::Text | Self::Enum => "Text",
            Self::I32 => "Int4",
            Self::I64 | Self::References => "Int8",
            Self::Bool => "Bool",
            Self::F32 => "Float4",
            Self::F64 => "Float8",
            Self::Uuid => "Uuid",
            Self::NaiveDateTime => "Timestamp",
            Self::DateTime => "Timestamptz",
            Self::Bytea => "Bytea",
            Self::Attachment => "Jsonb",
            Self::Decimal { .. } => "Numeric",
        }
    }

    /// `PostgreSQL` column type, without `NOT NULL` / `NULL`.
    #[must_use]
    pub const fn sql_type(self) -> &'static str {
        match self {
            Self::String | Self::Text | Self::Enum => "TEXT",
            Self::I32 => "INTEGER",
            Self::I64 | Self::References => "BIGINT",
            Self::Bool => "BOOLEAN",
            Self::F32 => "REAL",
            Self::F64 => "DOUBLE PRECISION",
            Self::Uuid => "UUID",
            Self::NaiveDateTime => "TIMESTAMP",
            Self::DateTime => "TIMESTAMPTZ",
            Self::Bytea => "BYTEA",
            Self::Attachment => "JSONB",
            Self::Decimal { .. } => "NUMERIC",
        }
    }

    /// Returns `true` for field kinds that represent file attachments (blobs).
    ///
    /// Used by the scaffold generator to detect fields that need multipart
    /// upload handling instead of the standard form-encoded path.
    #[must_use]
    pub const fn is_attachment(self) -> bool {
        matches!(self, Self::Attachment)
    }

    /// Returns `true` for a foreign-key `references` field.
    #[must_use]
    pub const fn is_reference(self) -> bool {
        matches!(self, Self::References)
    }

    /// Returns `true` for a closed-set `enum{…}` field.
    #[must_use]
    pub const fn is_enum(self) -> bool {
        matches!(self, Self::Enum)
    }

    /// Returns `true` for an exact-precision `decimal` field.
    ///
    /// Used by the scaffold/model generators to know when the project needs
    /// the `rust_decimal` dependency wired in.
    #[must_use]
    pub const fn is_decimal(self) -> bool {
        matches!(self, Self::Decimal { .. })
    }
}

/// The supported primary-key types for `autumn generate model --id`.
///
/// Defaults to `BigSerial` (today's `BIGSERIAL`/`i64` behaviour). `Uuid`
/// opts the model into a `UUID PRIMARY KEY DEFAULT gen_random_uuid()` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdType {
    /// `BIGSERIAL PRIMARY KEY` — sequential auto-increment integer (default).
    #[default]
    BigSerial,
    /// `UUID PRIMARY KEY DEFAULT gen_random_uuid()` — non-enumerable.
    Uuid,
}

impl IdType {
    /// Rust type for the `#[id]` struct field.
    #[must_use]
    pub const fn rust_type(self) -> &'static str {
        match self {
            Self::BigSerial => "i64",
            Self::Uuid => "uuid::Uuid",
        }
    }

    /// Diesel `table!` schema type token.
    #[must_use]
    pub const fn schema_type(self) -> &'static str {
        match self {
            Self::BigSerial => "Int8",
            Self::Uuid => "Uuid",
        }
    }

    /// The SQL fragment that appears after the column name in `CREATE TABLE`.
    #[must_use]
    pub const fn pk_sql(self) -> &'static str {
        match self {
            Self::BigSerial => "BIGSERIAL PRIMARY KEY",
            Self::Uuid => "UUID PRIMARY KEY DEFAULT gen_random_uuid()",
        }
    }

    /// An optional migration comment documenting trade-offs. Only `Uuid`
    /// returns `Some`, pointing developers toward the `UUIDv7` upgrade path.
    #[must_use]
    pub const fn migration_comment(self) -> Option<&'static str> {
        match self {
            Self::BigSerial => None,
            Self::Uuid => Some(
                "-- gen_random_uuid() produces UUIDv4 (random, built into Postgres 13+).\n\
                 -- Trade-off: random UUIDs hurt B-tree insert locality on large tables.\n\
                 -- To switch to a time-ordered UUIDv7 (better locality, same privacy):\n\
                 --   1. Enable pgcrypto or use the pg_uuidv7 extension.\n\
                 --   2. Replace DEFAULT gen_random_uuid() with DEFAULT uuid_generate_v7().\n",
            ),
        }
    }

    /// Parse a CLI `--id` value into an [`IdType`].
    ///
    /// Accepts `uuid` (case-insensitive) and `bigint`/`bigserial`/`i64`.
    ///
    /// # Errors
    /// Returns [`GenerateError::Config`] for unknown values, with a message
    /// listing the accepted tokens (AC7).
    pub fn parse(s: &str) -> Result<Self, GenerateError> {
        match s.to_ascii_lowercase().as_str() {
            "uuid" => Ok(Self::Uuid),
            "bigint" | "bigserial" | "i64" => Ok(Self::BigSerial),
            other => Err(GenerateError::Config(format!(
                "unknown --id value '{other}'; accepted values are: uuid, bigint"
            ))),
        }
    }
}

/// Comma-separated list of supported types, for error messages and `--help`.
pub const SUPPORTED_TYPES: &str = "String, Text, i32, i64, bool, f32, f64, \
    Uuid, NaiveDateTime, DateTime, Vec<u8>, Bytea, Attachment, references, \
    enum{a,b,…}, decimal{precision,scale}, Option<…>, :unique";

/// Comma-separated list of supported Postgres column types (`udt_name`), for
/// the `db pull` introspection error message.
pub const SQL_SUPPORTED_TYPES: &str = "text, varchar, bpchar (-> String), int4 (-> i32), \
    int8 (-> i64), bool, float4 (-> f32), float8 (-> f64), uuid, timestamp, timestamptz, bytea";

/// Inverse of [`FieldKind::sql_type`] / [`FieldKind::schema_type`]: map a
/// Postgres `udt_name` (the concrete catalog type identifier such as `int4`,
/// `int8`, `text`, `timestamptz`) to the [`FieldKind`] the generators use.
///
/// This is the introspection direction used by `autumn db pull`. `text`,
/// `varchar`, and `bpchar` all collapse to the canonical [`FieldKind::String`]
/// (the DSL's `String`/`Text` aliases both render `String` in Rust and `Text`
/// in `schema.rs`, so the round-trip with a greenfield-generated model stays
/// byte-identical).
///
/// Returns `None` for types outside the documented surface so the caller can
/// fail loudly with a column-named error rather than silently dropping it.
#[must_use]
pub fn sql_type_to_field_kind(udt_name: &str) -> Option<FieldKind> {
    match udt_name {
        "text" | "varchar" | "bpchar" => Some(FieldKind::String),
        "int4" => Some(FieldKind::I32),
        "int8" => Some(FieldKind::I64),
        "bool" => Some(FieldKind::Bool),
        "float4" => Some(FieldKind::F32),
        "float8" => Some(FieldKind::F64),
        "uuid" => Some(FieldKind::Uuid),
        "timestamp" => Some(FieldKind::NaiveDateTime),
        "timestamptz" => Some(FieldKind::DateTime),
        "bytea" => Some(FieldKind::Bytea),
        // `jsonb` is intentionally unsupported: although the DSL maps
        // `Attachment` -> `JSONB`, the inverse is ambiguous. A brownfield
        // `jsonb` column is usually arbitrary application JSON, not an Autumn
        // `Blob`, and introspection cannot tell them apart (both report
        // `udt_name = jsonb`). Mapping it to `Attachment` would emit a
        // `Blob` field that fails to deserialize real JSON rows, so we leave
        // it unsupported rather than risk corrupting brownfield data.
        _ => None,
    }
}

/// Parse a single CLI token of the form `name:Type`.
///
/// # Errors
/// Returns [`GenerateError::InvalidField`] if the token is malformed or the
/// type is not in the supported set.
#[allow(
    clippy::too_many_lines,
    reason = "a linear name→modifier→type→constraint parse; the early-return \
              validation guards read more clearly inline than split across helpers"
)]
pub fn parse_field(token: &str) -> Result<Field, GenerateError> {
    let (name, rest) = token
        .split_once(':')
        .ok_or_else(|| GenerateError::InvalidField {
            token: token.to_owned(),
            reason: "expected `name:Type` (missing colon)".into(),
        })?;

    let name = name.trim();
    // A trailing `:unique` modifier marks the column for a `CREATE UNIQUE
    // INDEX` in the migration (issue #1032), e.g. `email:String:unique`.
    // Split it off the *end* first: a `references` `{label:col}` constraint
    // modifier (issue #1146) also contains a colon, but it lives inside the
    // trailing `{…}` block, so stripping a literal `:unique` suffix — never
    // the colon inside the braces — stays unambiguous. Any other stray
    // colon outside the braces is caught as an unknown modifier below.
    let rest = rest.trim();
    let (ty, unique) = match rest.rsplit_once(':') {
        // A trailing `:unique` (whitespace-tolerant) is the UNIQUE-index
        // modifier. Its colon is always the last one — a `references`
        // `{label:col}` colon sits *inside* the braces before it, so
        // `rsplit_once` never mistakes the label colon for `:unique`.
        Some((before, modifier)) if modifier.trim() == "unique" => (before.trim(), true),
        // Any other trailing `:segment` (including a label colon inside a
        // `{…}` block) is left on `ty` for the constraint/type parsing below
        // to interpret or reject — not an error yet.
        _ => (rest, false),
    };

    if name.is_empty() {
        return Err(GenerateError::InvalidField {
            token: token.to_owned(),
            reason: "field name is empty".into(),
        });
    }
    if !is_valid_ident(name) {
        return Err(GenerateError::InvalidField {
            token: token.to_owned(),
            reason: format!("'{name}' is not a valid snake_case identifier"),
        });
    }
    if is_rust_keyword(name) {
        return Err(GenerateError::InvalidField {
            token: token.to_owned(),
            reason: format!("'{name}' is a Rust keyword and cannot be used as a struct field name"),
        });
    }

    if let Some((variants, nullable)) =
        parse_enum_type(ty).map_err(|reason| GenerateError::InvalidField {
            token: token.to_owned(),
            reason,
        })?
    {
        return Ok(Field {
            name: name.to_owned(),
            kind: FieldKind::Enum,
            nullable,
            variants,
            unique,
            constraints: FieldConstraints::default(),
        });
    }

    if let Some((precision, scale, nullable)) =
        parse_decimal_type(ty).map_err(|reason| GenerateError::InvalidField {
            token: token.to_owned(),
            reason,
        })?
    {
        return Ok(Field {
            name: name.to_owned(),
            kind: FieldKind::Decimal { precision, scale },
            nullable,
            variants: Vec::new(),
            unique,
            constraints: FieldConstraints::default(),
        });
    }

    // Fall-through: a scalar / `references` type, optionally carrying a
    // trailing `{…}` constraint-modifier block (issue #1388 validation +
    // HTML5 constraints; issue #1146 `references` `{label:col}`). `enum{…}`
    // and `decimal{…}` were handled above — their braces are part of the
    // *type*, so they never reach this generic modifier split.
    let (base_ty, modifier_body) = split_constraint_modifier(ty);

    // A leftover `{` means an unbalanced brace block (e.g. a shell that
    // brace-expanded `String{min=3,max=120}` into two arguments, leaving a
    // fragment like `String{min=3`). Any leftover `:` outside the braces is a
    // stray trailing modifier (the only supported one, `:unique`, was already
    // consumed above; a `references` label colon lives inside the braces).
    if base_ty.contains('{') {
        return Err(GenerateError::InvalidField {
            token: token.to_owned(),
            reason: format!(
                "malformed constraint modifier in '{ty}' (unbalanced braces). If you typed \
                 this in bash or zsh, quote the whole token so the shell doesn't brace-expand \
                 it, e.g. 'title:String{{min=3,max=120}}'."
            ),
        });
    }
    if let Some((_, bad)) = base_ty.rsplit_once(':') {
        return Err(GenerateError::InvalidField {
            token: token.to_owned(),
            reason: format!(
                "unknown field modifier '{}'; the only bare modifier is 'unique' — other \
                 constraints go in a trailing `{{…}}` block (e.g. 'title:String{{min=3,max=120}}')",
                bad.trim()
            ),
        });
    }

    let (kind, nullable) = parse_type(base_ty).ok_or_else(|| GenerateError::InvalidField {
        token: token.to_owned(),
        reason: format!("unsupported type '{base_ty}'. Supported: {SUPPORTED_TYPES}"),
    })?;

    let constraints = match modifier_body {
        Some(body) => {
            parse_field_constraints(body, kind).map_err(|reason| GenerateError::InvalidField {
                token: token.to_owned(),
                reason,
            })?
        }
        None => FieldConstraints::default(),
    };

    // `references` fields always end in `_id` — `post:references` resolves to
    // the column `post_id`. Tolerate an already-suffixed name (`post_id:references`)
    // rather than doubling the suffix.
    let name = if kind == FieldKind::References && !name.ends_with("_id") {
        format!("{name}_id")
    } else {
        name.to_owned()
    };

    Ok(Field {
        name,
        kind,
        nullable,
        variants: Vec::new(),
        unique,
        constraints,
    })
}

/// Split a scalar/`references` type token into its base type and the body of a
/// trailing `{…}` constraint-modifier block, if present. `("String{min=3}",)`
/// -> `("String", Some("min=3"))`; a token with no (or an unbalanced) block
/// yields `(ty, None)` — the caller then rejects a leftover `{` as malformed.
fn split_constraint_modifier(ty: &str) -> (&str, Option<&str>) {
    let ty = ty.trim();
    if let Some(open) = ty.find('{')
        && ty.ends_with('}')
    {
        let close = ty.len() - 1;
        if close > open {
            return (ty[..open].trim_end(), Some(ty[open + 1..close].trim()));
        }
    }
    (ty, None)
}

/// Format the `min = …, max = …` argument list shared by `length(…)` and
/// `range(…)` `#[validate]` attributes. `float` range bounds are emitted with
/// a decimal point (`0` -> `0.0`) so the generated comparison type-checks
/// against an `f32`/`f64` field.
fn min_max_args(min: Option<&String>, max: Option<&String>, float: bool) -> String {
    let fmt = |raw: &str| -> String {
        if float && !raw.contains(['.', 'e', 'E']) {
            format!("{raw}.0")
        } else {
            raw.to_owned()
        }
    };
    let mut args = Vec::with_capacity(2);
    if let Some(min) = min {
        args.push(format!("min = {}", fmt(min)));
    }
    if let Some(max) = max {
        args.push(format!("max = {}", fmt(max)));
    }
    args.join(", ")
}

/// Parse the body of a `{…}` constraint-modifier block against the field's
/// `kind` (issue #1388 validation + HTML5 constraints; issue #1146
/// `references` `{label:col}`).
///
/// Supported per kind:
/// - `String`/`Text`: `min=N`, `max=N` (length bounds, non-negative
///   integers), `email`, `url`.
/// - `i32`/`i64`/`f32`/`f64`: `min=N`, `max=N` (range bounds).
/// - `references`: `label:col` (or `label=col`) — the display column.
///
/// Every other kind (and every unknown key/flag) is rejected with a message
/// naming the offending token, so a misspelling like `{maxx=5}` fails the
/// scaffold loudly rather than being silently dropped (issue #1388 AC5).
fn parse_field_constraints(body: &str, kind: FieldKind) -> Result<FieldConstraints, String> {
    let mut c = FieldConstraints::default();
    if body.is_empty() {
        return Err(
            "empty constraint block `{}` — remove the braces or add a constraint \
             (e.g. `{min=3,max=120}`, `{email}`, `{label:title}`)"
                .to_owned(),
        );
    }

    let string_like = matches!(kind, FieldKind::String | FieldKind::Text);
    let numeric = matches!(
        kind,
        FieldKind::I32 | FieldKind::I64 | FieldKind::F32 | FieldKind::F64
    );
    let is_reference = kind.is_reference();

    for raw in body.split(',') {
        let tok = raw.trim();
        if tok.is_empty() {
            return Err("empty constraint (stray comma) in the `{…}` block".to_owned());
        }
        // `label:col` (references display column) uses a colon; every other
        // key/value pair uses `=`. Bare tokens (`email`, `url`) have neither.
        if let Some((key, value)) = tok.split_once('=') {
            parse_constraint_kv(&mut c, key.trim(), value.trim(), kind)?;
        } else if let Some((key, value)) = tok.split_once(':') {
            if key.trim() == "label" {
                set_label_constraint(&mut c, value.trim(), is_reference)?;
            } else {
                return Err(unknown_constraint_message(key.trim(), kind));
            }
        } else {
            match tok {
                "email" | "url" => {
                    if !string_like {
                        return Err(format!(
                            "the `{tok}` constraint only applies to String/Text fields"
                        ));
                    }
                    if tok == "email" {
                        c.email = true;
                    } else {
                        c.url = true;
                    }
                }
                _ => return Err(unknown_constraint_message(tok, kind)),
            }
        }
    }

    // Cross-check the combination against the kind: length/range bounds need a
    // string or numeric field, and require min <= max when both are present.
    if (c.min.is_some() || c.max.is_some()) && !string_like && !numeric {
        return Err(format!(
            "min/max constraints are not supported for {} fields",
            kind.rust_type()
        ));
    }
    if let (Some(min), Some(max)) = (&c.min, &c.max) {
        let (lo, hi): (f64, f64) = (
            min.parse().map_err(|_| "min must be a number".to_owned())?,
            max.parse().map_err(|_| "max must be a number".to_owned())?,
        );
        if lo > hi {
            return Err(format!("min ({min}) cannot be greater than max ({max})"));
        }
    }

    Ok(c)
}

/// Handle a `key=value` constraint token (`min=`, `max=`, or a `=`-spelled
/// `label=`).
fn parse_constraint_kv(
    c: &mut FieldConstraints,
    key: &str,
    value: &str,
    kind: FieldKind,
) -> Result<(), String> {
    match key {
        "min" | "max" => {
            let bound = parse_bound(value, kind)?;
            if key == "min" {
                c.min = Some(bound);
            } else {
                c.max = Some(bound);
            }
            Ok(())
        }
        "label" => set_label_constraint(c, value, kind.is_reference()),
        _ => Err(unknown_constraint_message(key, kind)),
    }
}

/// Validate and normalize a `min=`/`max=` bound for `kind`: a length bound on
/// a `String`/`Text` field must be a non-negative integer; a range bound on a
/// numeric field must be a valid number.
fn parse_bound(value: &str, kind: FieldKind) -> Result<String, String> {
    match kind {
        FieldKind::String | FieldKind::Text => {
            value
                .parse::<u64>()
                .map_err(|_| format!("length bound '{value}' must be a non-negative integer"))?;
            Ok(value.to_owned())
        }
        FieldKind::I32 | FieldKind::I64 => {
            value
                .parse::<i64>()
                .map_err(|_| format!("range bound '{value}' must be an integer"))?;
            Ok(value.to_owned())
        }
        FieldKind::F32 | FieldKind::F64 => {
            value
                .parse::<f64>()
                .map_err(|_| format!("range bound '{value}' must be a number"))?;
            Ok(value.to_owned())
        }
        _ => Err(format!(
            "min/max constraints are not supported for {} fields",
            kind.rust_type()
        )),
    }
}

/// Set the `references` display-column override, rejecting `label` on a
/// non-`references` field and a label that isn't a valid `snake_case` column.
fn set_label_constraint(
    c: &mut FieldConstraints,
    value: &str,
    is_reference: bool,
) -> Result<(), String> {
    if !is_reference {
        return Err("the `label` constraint only applies to `references` fields".to_owned());
    }
    if !is_valid_ident(value) {
        return Err(format!(
            "reference label column '{value}' is not a valid snake_case identifier"
        ));
    }
    c.label = Some(value.to_owned());
    Ok(())
}

/// A per-kind "unknown constraint" message that names the offending token and
/// lists what the kind *does* accept (issue #1388 AC5).
fn unknown_constraint_message(token: &str, kind: FieldKind) -> String {
    let accepted = match kind {
        FieldKind::String | FieldKind::Text => "min=N, max=N, email, url",
        FieldKind::I32 | FieldKind::I64 | FieldKind::F32 | FieldKind::F64 => "min=N, max=N",
        FieldKind::References => "label:col",
        _ => "(none — this field type takes no constraint modifiers)",
    };
    format!(
        "unknown constraint '{token}' for {} fields; supported: {accepted}",
        kind.rust_type()
    )
}

/// Parse an `enum{a,b,c}` (optionally `Option<enum{a,b,c}>`) type token.
///
/// Returns `Ok(None)` when `ty` isn't an enum token at all, so the caller
/// falls through to [`parse_type`]/[`atomic_type`] unchanged. Returns
/// `Err(reason)` when `ty` looks like an enum token but is malformed (bad
/// variant, too few variants, unbalanced braces, …) — every reason is an
/// actionable message, consistent with the field-name guarding above.
///
/// # Errors
/// See above.
fn parse_enum_type(ty: &str) -> Result<Option<(Vec<String>, bool)>, String> {
    let (body, nullable) =
        strip_wrapper(ty, "Option").map_or((ty, false), |inner| (inner.trim(), true));

    let Some(rest) = body.strip_prefix("enum") else {
        return Ok(None);
    };
    let rest = rest.trim_start();

    let Some(inner) = rest.strip_prefix('{') else {
        // Looks like an enum token (starts with `enum`) but has no opening
        // brace. The most common cause is bash/zsh brace-expanding an
        // unquoted `enum{a,b}` before the CLI ever sees it, turning
        // `status:enum{draft,published}` into two separate arguments whose
        // surviving fragment reads like `status:enumdraft` — but `ty` could
        // also just be an unrelated typo (e.g. `enumerable`), so the message
        // covers both: the quoting hint for the shell-expansion case, and
        // the full supported-types list for a genuine typo.
        return Err(format!(
            "expected enum{{variant1,variant2,…}}. If you typed this in bash or zsh, \
             quote the token so the shell doesn't brace-expand it, \
             e.g. 'status:enum{{draft,published}}'. Supported: {SUPPORTED_TYPES}"
        ));
    };
    let Some(body_inner) = inner.strip_suffix('}') else {
        return Err("expected enum{variant1,variant2,…} (missing closing brace)".to_owned());
    };

    let mut variants: Vec<String> = Vec::new();
    let mut seen_pascal = std::collections::HashSet::new();
    for raw in body_inner.split(',') {
        let variant = raw.trim();
        if variant.is_empty() {
            return Err("enum variants cannot be empty".to_owned());
        }
        if !is_valid_ident(variant) {
            return Err(format!("'{variant}' is not a valid snake_case identifier"));
        }
        if is_rust_keyword(variant) {
            return Err(format!(
                "'{variant}' is a Rust keyword and cannot be used as an enum variant"
            ));
        }
        let pascal = naming::pascal(variant);
        // `is_valid_ident` allows a leading/all-underscore variant (e.g.
        // `_2fa`, `__`), but `pascal()` strips leading underscores without
        // introducing a letter, which can leave an empty string or a result
        // starting with a digit — neither is a valid Rust identifier, so the
        // generated enum variant would fail to compile.
        if pascal.is_empty() || pascal.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(format!(
                "'{variant}' does not produce a valid Rust identifier once converted to \
                 PascalCase ('{pascal}'); rename the variant"
            ));
        }
        if !seen_pascal.insert(pascal.clone()) {
            return Err(format!(
                "duplicate enum variant '{pascal}' (variants must be distinct once converted to PascalCase)"
            ));
        }
        variants.push(variant.to_owned());
    }

    if variants.len() < 2 {
        return Err(
            "an enum needs at least two variants; use String for a free-form field".to_owned(),
        );
    }

    Ok(Some((variants, nullable)))
}

/// The maximum `decimal` precision (total significant digits) this DSL
/// accepts. Bounded by `rust_decimal::Decimal`'s own representable range —
/// a 96-bit mantissa, guaranteeing at most 28 significant digits — not by
/// Postgres's much larger `NUMERIC` limit (1000): a column declared wider
/// than `rust_decimal` can hold would compile and migrate cleanly, then fail
/// at runtime the first time a value actually needing that extra precision
/// is deserialized into the generated `#[model]` field.
const MAX_DECIMAL_PRECISION: u32 = 28;

/// Parse a `decimal` (optionally `Option<decimal>`) type token, with an
/// optional `{precision,scale}` modifier (`decimal{10,2}`). Accepts both
/// `decimal`/`Decimal` casings, consistent with `Attachment`/`attachment`.
/// Defaults to `{12,2}` — a money-shaped `NUMERIC(12,2)` — when the modifier
/// is omitted.
///
/// Returns `Ok(None)` when `ty` isn't a decimal token at all, so the caller
/// falls through to [`parse_type`]/[`atomic_type`] unchanged. Returns
/// `Err(reason)` when `ty` looks like a decimal token but is malformed
/// (missing/unbalanced braces, non-numeric precision/scale, scale exceeding
/// precision, precision outside `rust_decimal`'s representable range, …) —
/// every reason is an actionable message, consistent with the field-name
/// guarding above.
///
/// # Errors
/// See above.
fn parse_decimal_type(ty: &str) -> Result<Option<(u32, u32, bool)>, String> {
    let (body, nullable) =
        strip_wrapper(ty, "Option").map_or((ty, false), |inner| (inner.trim(), true));
    // Defensive: every caller of `parse_field` already trims the outer `ty`
    // before it reaches here, but trimming `body` again costs nothing and
    // guards against a caller that doesn't (e.g. a direct unit-test call, or
    // future refactor) — see the shell-brace-expansion hint below, which
    // depends on `body` ending exactly at `}` with no trailing whitespace.
    let body = body.trim();

    let Some(rest) = body
        .strip_prefix("decimal")
        .or_else(|| body.strip_prefix("Decimal"))
    else {
        return Ok(None);
    };
    let rest = rest.trim_start();

    if rest.is_empty() {
        // Bare `decimal`/`Decimal`: default to a money-shaped NUMERIC(12,2).
        return Ok(Some((12, 2, nullable)));
    }

    let Some(inner) = rest.strip_prefix('{') else {
        // Looks like a decimal token (starts with `decimal`/`Decimal` and
        // has more text after it) but has no opening brace. As with
        // `enum{...}`, the most common cause is bash/zsh brace-expanding an
        // unquoted `decimal{10,2}` before the CLI ever sees it, turning
        // `price:decimal{10,2}` into two separate arguments whose surviving
        // fragments read like `price:decimal10` and `price:decimal2` — but
        // `ty` could also just be an unrelated typo (e.g. `decimalize`), so
        // the message covers both: the quoting hint for the shell-expansion
        // case, and the full supported-types list for a genuine typo.
        return Err(format!(
            "expected decimal{{precision,scale}} (or bare `decimal` for the \
             default NUMERIC(12,2)). If you typed this in bash or zsh, quote \
             the token so the shell doesn't brace-expand it, e.g. \
             'price:decimal{{10,2}}'. Supported: {SUPPORTED_TYPES}"
        ));
    };
    let Some(body_inner) = inner.strip_suffix('}') else {
        return Err("expected decimal{precision,scale} (missing closing brace)".to_owned());
    };

    let parts: Vec<&str> = body_inner.split(',').map(str::trim).collect();
    let [precision_str, scale_str] = parts.as_slice() else {
        return Err(format!(
            "expected decimal{{precision,scale}} (exactly two comma-separated numbers), \
             got 'decimal{{{body_inner}}}'"
        ));
    };

    let precision: u32 = precision_str.parse().map_err(|_| {
        format!(
            "'{precision_str}' is not a valid decimal precision \
             (expected a positive integer)"
        )
    })?;
    let scale: u32 = scale_str.parse().map_err(|_| {
        format!("'{scale_str}' is not a valid decimal scale (expected a non-negative integer)")
    })?;

    if precision == 0 || precision > MAX_DECIMAL_PRECISION {
        return Err(format!(
            "decimal precision must be between 1 and {MAX_DECIMAL_PRECISION} \
             (rust_decimal's representable range), got {precision}"
        ));
    }
    if scale > precision {
        return Err(format!(
            "decimal scale ({scale}) cannot be greater than precision ({precision})"
        ));
    }

    Ok(Some((precision, scale, nullable)))
}

/// Parse a list of `name:Type` tokens.
///
/// # Errors
/// Bubbles up the first failed token, and rejects duplicate field names —
/// emitting two entries with the same column name would produce duplicate
/// struct members and duplicate SQL columns.
pub fn parse_fields(tokens: &[String]) -> Result<Vec<Field>, GenerateError> {
    let mut fields: Vec<Field> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let field = parse_field(token)?;
        if let Some(prev) = fields.iter().find(|f| f.name == field.name) {
            return Err(GenerateError::InvalidField {
                token: token.clone(),
                reason: format!(
                    "duplicate field name '{name}' (previously declared as '{name}:{prev_ty}')",
                    name = field.name,
                    prev_ty = prev.rust_type()
                ),
            });
        }
        fields.push(field);
    }
    Ok(fields)
}

fn parse_type(ty: &str) -> Option<(FieldKind, bool)> {
    // A trailing `?` is a terser nullable marker than `Option<…>`, but is only
    // recognized for `references` (`post:references?`) — every other type
    // must use `Option<…>` for nullability, so this doesn't silently expand
    // the DSL's accepted grammar (e.g. `count:i64?` stays an error).
    if matches!(
        ty.strip_suffix('?').map(str::trim),
        Some("references" | "References")
    ) {
        return Some((FieldKind::References, true));
    }
    if let Some(inner) = strip_wrapper(ty, "Option") {
        let kind = atomic_type(inner.trim())?;
        Some((kind, true))
    } else {
        atomic_type(ty).map(|k| {
            // Attachment fields are always nullable: a file attachment is
            // almost universally optional (a post might not have a cover image),
            // and `Option<Blob>` is the idiomatic Rust representation.
            let nullable = matches!(k, FieldKind::Attachment);
            (k, nullable)
        })
    }
}

fn atomic_type(ty: &str) -> Option<FieldKind> {
    match ty {
        "String" => Some(FieldKind::String),
        "Text" => Some(FieldKind::Text),
        "i32" => Some(FieldKind::I32),
        "i64" => Some(FieldKind::I64),
        "bool" => Some(FieldKind::Bool),
        "f32" => Some(FieldKind::F32),
        "f64" => Some(FieldKind::F64),
        "Uuid" => Some(FieldKind::Uuid),
        "NaiveDateTime" => Some(FieldKind::NaiveDateTime),
        "DateTime" => Some(FieldKind::DateTime),
        "Bytea" => Some(FieldKind::Bytea),
        // Attachment / attachment: file-attachment blob stored as JSONB.
        // Accept both casing variants so `cover_image:Attachment` and
        // `cover_image:attachment` both work.
        "Attachment" | "attachment" => Some(FieldKind::Attachment),
        // References / references: foreign-key column, resolved to `_id` and
        // `BIGINT REFERENCES <table>(id)` by the callers that emit SQL.
        "References" | "references" => Some(FieldKind::References),
        _ => {
            // Allow `Vec<u8>` as a synonym for `Bytea`.
            strip_wrapper(ty, "Vec").and_then(|inner| {
                if inner.trim() == "u8" {
                    Some(FieldKind::Bytea)
                } else {
                    None
                }
            })
        }
    }
}

fn strip_wrapper<'a>(ty: &'a str, wrapper: &str) -> Option<&'a str> {
    let prefix = format!("{wrapper}<");
    let stripped = ty.strip_prefix(&prefix)?;
    stripped.strip_suffix('>')
}

pub(super) fn is_valid_ident(s: &str) -> bool {
    // A bare `_` is the reserved wildcard, not a usable field/module name
    // (`pub _: T` does not compile), so reject it explicitly.
    if s == "_" {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Strict and reserved Rust keywords that cannot appear as a struct field name
/// or module name without raw-identifier syntax. Rather than emitting `r#type:`
/// we reject the input so the generator never produces broken code.
///
/// Public so the resource-name validator in [`super::model`] can share the same
/// list.
pub(super) const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "do", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod",
    "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true",
    "try", "type", "unsafe", "use", "where", "while", "yield", "abstract", "become", "box",
    "final", "macro", "override", "priv", "typeof", "unsized", "virtual",
];

pub(super) fn is_rust_keyword(s: &str) -> bool {
    RUST_KEYWORDS.contains(&s)
}

#[cfg(test)]
// Test inputs like `"title:String{min=3}"` / `"post:references{label:title}"`
// are literal DSL tokens passed to `parse_field`, not format strings — the
// `{…}` is the scaffold's own constraint-modifier syntax under test.
#[allow(clippy::literal_string_with_formatting_args)]
mod tests {
    use super::*;

    #[test]
    fn parse_string_field() {
        let f = parse_field("title:String").unwrap();
        assert_eq!(f.name, "title");
        assert_eq!(f.kind, FieldKind::String);
        assert!(!f.nullable);
        assert_eq!(f.rust_type(), "String");
        assert_eq!(f.sql_type(), "TEXT");
        assert_eq!(f.schema_type(), "Text");
    }

    #[test]
    fn parse_text_alias() {
        let f = parse_field("body:Text").unwrap();
        assert_eq!(f.kind, FieldKind::Text);
        assert_eq!(f.rust_type(), "String");
        assert_eq!(f.sql_type(), "TEXT");
    }

    #[test]
    fn parse_optional_field() {
        let f = parse_field("description:Option<String>").unwrap();
        assert_eq!(f.kind, FieldKind::String);
        assert!(f.nullable);
        assert_eq!(f.rust_type(), "Option<String>");
        assert_eq!(f.sql_nullability(), "NULL");
        assert_eq!(f.schema_type(), "Nullable<Text>");
    }

    #[test]
    fn parse_bytea_via_vec() {
        let f = parse_field("data:Vec<u8>").unwrap();
        assert_eq!(f.kind, FieldKind::Bytea);
        assert_eq!(f.rust_type(), "Vec<u8>");
        assert_eq!(f.sql_type(), "BYTEA");
    }

    #[test]
    fn parse_bytea_alias() {
        let f = parse_field("data:Bytea").unwrap();
        assert_eq!(f.kind, FieldKind::Bytea);
    }

    #[test]
    fn parse_uuid() {
        let f = parse_field("token:Uuid").unwrap();
        assert_eq!(f.rust_type(), "uuid::Uuid");
        assert_eq!(f.sql_type(), "UUID");
    }

    #[test]
    fn parse_datetime() {
        let f = parse_field("created_at:DateTime").unwrap();
        assert_eq!(f.rust_type(), "chrono::DateTime<chrono::Utc>");
        assert_eq!(f.schema_type(), "Timestamptz");
    }

    #[test]
    fn parse_naive_datetime() {
        let f = parse_field("created_at:NaiveDateTime").unwrap();
        assert_eq!(f.rust_type(), "chrono::NaiveDateTime");
        assert_eq!(f.schema_type(), "Timestamp");
    }

    #[test]
    fn parse_all_numeric_types() {
        assert_eq!(parse_field("a:i32").unwrap().sql_type(), "INTEGER");
        assert_eq!(parse_field("b:i64").unwrap().sql_type(), "BIGINT");
        assert_eq!(parse_field("c:f32").unwrap().sql_type(), "REAL");
        assert_eq!(parse_field("d:f64").unwrap().sql_type(), "DOUBLE PRECISION");
        assert_eq!(parse_field("e:bool").unwrap().sql_type(), "BOOLEAN");
    }

    #[test]
    fn unknown_type_rejected() {
        let err = parse_field("price:Money").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Money"));
        assert!(msg.contains("Supported:"));
    }

    #[test]
    fn missing_colon_rejected() {
        let err = parse_field("title").unwrap_err();
        assert!(err.to_string().contains("missing colon"));
    }

    #[test]
    fn empty_name_rejected() {
        let err = parse_field(":String").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn pascal_case_name_rejected() {
        let err = parse_field("Title:String").unwrap_err();
        assert!(err.to_string().contains("snake_case"));
    }

    #[test]
    fn rust_keyword_field_name_rejected() {
        // `pub type: String` would be a Rust syntax error.
        let err = parse_field("type:String").unwrap_err();
        assert!(err.to_string().contains("Rust keyword"));
    }

    #[test]
    fn other_keywords_also_rejected() {
        for kw in ["fn", "match", "struct", "self", "impl", "ref", "move"] {
            let token = format!("{kw}:String");
            assert!(
                parse_field(&token).is_err(),
                "expected '{kw}' to be rejected"
            );
        }
    }

    #[test]
    fn nested_option_is_unsupported() {
        // Option<Option<String>> is intentionally not part of the surface.
        let err = parse_field("x:Option<Option<String>>").unwrap_err();
        assert!(err.to_string().contains("unsupported type"));
    }

    #[test]
    fn vec_of_other_types_rejected() {
        let err = parse_field("xs:Vec<i32>").unwrap_err();
        assert!(err.to_string().contains("unsupported type"));
    }

    #[test]
    fn parse_multiple_fields() {
        let tokens = vec!["title:String".into(), "count:i64".into()];
        let fs = parse_fields(&tokens).unwrap();
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0].name, "title");
        assert_eq!(fs[1].name, "count");
    }

    #[test]
    fn duplicate_field_names_rejected() {
        // `title:String title:Text` would emit two `title` columns.
        let tokens = vec!["title:String".into(), "title:Text".into()];
        let err = parse_fields(&tokens).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate"),
            "expected duplicate error, got: {msg}"
        );
        assert!(msg.contains("title"));
    }

    #[test]
    fn whitespace_around_tokens_tolerated() {
        let f = parse_field(" name : String ").unwrap();
        assert_eq!(f.name, "name");
        assert_eq!(f.kind, FieldKind::String);
    }

    // ── RED: Attachment field kind ──────────────────────────────────────────

    #[test]
    fn parse_attachment_pascal() {
        let f = parse_field("cover_image:Attachment").unwrap();
        assert_eq!(f.kind, FieldKind::Attachment);
        assert!(f.nullable, "attachment fields must default to nullable");
    }

    #[test]
    fn parse_attachment_lowercase() {
        let f = parse_field("cover_image:attachment").unwrap();
        assert_eq!(f.kind, FieldKind::Attachment);
    }

    #[test]
    fn attachment_rust_type_is_blob() {
        let f = parse_field("cover_image:Attachment").unwrap();
        assert_eq!(f.rust_type(), "Option<autumn_web::storage::Blob>");
    }

    #[test]
    fn attachment_sql_type_is_jsonb() {
        let f = parse_field("cover_image:Attachment").unwrap();
        assert_eq!(f.sql_type(), "JSONB");
    }

    #[test]
    fn attachment_schema_type_is_jsonb() {
        let f = parse_field("cover_image:Attachment").unwrap();
        assert_eq!(f.schema_type(), "Nullable<Jsonb>");
    }

    #[test]
    fn attachment_is_attachment_returns_true() {
        assert!(FieldKind::Attachment.is_attachment());
        assert!(!FieldKind::String.is_attachment());
        assert!(!FieldKind::Uuid.is_attachment());
    }

    #[test]
    fn optional_attachment_parses() {
        let f = parse_field("avatar:Option<Attachment>").unwrap();
        assert_eq!(f.kind, FieldKind::Attachment);
        assert!(f.nullable);
        assert_eq!(f.rust_type(), "Option<autumn_web::storage::Blob>");
    }

    #[test]
    fn attachment_in_list_of_fields() {
        let tokens = vec![
            "title:String".into(),
            "cover_image:Attachment".into(),
            "count:i64".into(),
        ];
        let fields = parse_fields(&tokens).unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[1].kind, FieldKind::Attachment);
    }

    #[test]
    fn attachment_appears_in_supported_types_constant() {
        assert!(
            SUPPORTED_TYPES.contains("Attachment"),
            "SUPPORTED_TYPES must list Attachment"
        );
    }

    // ── Inverse mapping (db pull introspection, issue #975) ─────────────────

    #[test]
    fn sql_type_inverse_maps_all_supported_udt_names() {
        // (udt_name, expected kind, expected rust_type, expected schema_type)
        let cases: &[(&str, FieldKind, &str, &str)] = &[
            ("text", FieldKind::String, "String", "Text"),
            ("varchar", FieldKind::String, "String", "Text"),
            ("bpchar", FieldKind::String, "String", "Text"),
            ("int4", FieldKind::I32, "i32", "Int4"),
            ("int8", FieldKind::I64, "i64", "Int8"),
            ("bool", FieldKind::Bool, "bool", "Bool"),
            ("float4", FieldKind::F32, "f32", "Float4"),
            ("float8", FieldKind::F64, "f64", "Float8"),
            ("uuid", FieldKind::Uuid, "uuid::Uuid", "Uuid"),
            (
                "timestamp",
                FieldKind::NaiveDateTime,
                "chrono::NaiveDateTime",
                "Timestamp",
            ),
            (
                "timestamptz",
                FieldKind::DateTime,
                "chrono::DateTime<chrono::Utc>",
                "Timestamptz",
            ),
            ("bytea", FieldKind::Bytea, "Vec<u8>", "Bytea"),
        ];
        for (udt, kind, rust, schema) in cases {
            let mapped = sql_type_to_field_kind(udt)
                .unwrap_or_else(|| panic!("'{udt}' must map to a FieldKind"));
            assert_eq!(mapped, *kind, "kind mismatch for {udt}");
            assert_eq!(mapped.rust_type(), *rust, "rust_type mismatch for {udt}");
            assert_eq!(
                mapped.schema_type(),
                *schema,
                "schema_type mismatch for {udt}"
            );
        }
    }

    #[test]
    fn sql_type_inverse_preserves_i64_for_int8() {
        // i64 PKs must round-trip as i64 (AC3).
        assert_eq!(sql_type_to_field_kind("int8"), Some(FieldKind::I64));
        assert_eq!(sql_type_to_field_kind("int8").unwrap().rust_type(), "i64");
    }

    #[test]
    fn sql_type_inverse_rejects_unknown_types() {
        // Unmapped SQL types must be reported, never silently dropped (AC2).
        // `jsonb` is deliberately unsupported: the inverse of `Attachment ->
        // JSONB` is ambiguous (arbitrary JSON vs an Autumn `Blob`), so pulling
        // it must not silently produce a `Blob` field.
        for udt in ["numeric", "jsonb", "json", "inet", "money", "point"] {
            assert!(
                sql_type_to_field_kind(udt).is_none(),
                "'{udt}' is outside the documented surface and must not map"
            );
        }
    }

    #[test]
    fn bare_underscore_is_not_a_valid_ident() {
        // `pub _: T` is the reserved wildcard and does not compile.
        assert!(!is_valid_ident("_"));
        assert!(parse_field("_:String").is_err());
        // But `_`-prefixed names remain valid.
        assert!(is_valid_ident("_internal"));
    }

    #[test]
    fn sql_type_inverse_round_trips_forward_sql_types() {
        // Every non-Attachment FieldKind's forward sql_type() (lowercased,
        // base name) must invert back to an equivalent kind, guaranteeing the
        // db-pull inverse stays in lockstep with the generate forward map.
        for (kind, udt) in [
            (FieldKind::String, "text"),
            (FieldKind::I32, "int4"),
            (FieldKind::I64, "int8"),
            (FieldKind::Bool, "bool"),
            (FieldKind::F32, "float4"),
            (FieldKind::F64, "float8"),
            (FieldKind::Uuid, "uuid"),
            (FieldKind::NaiveDateTime, "timestamp"),
            (FieldKind::DateTime, "timestamptz"),
            (FieldKind::Bytea, "bytea"),
        ] {
            let back = sql_type_to_field_kind(udt).unwrap();
            assert_eq!(back.rust_type(), kind.rust_type());
            assert_eq!(back.schema_type(), kind.schema_type());
        }
    }

    // ── IdType (primary-key type, issue #1400) ─────────────────────────────

    #[test]
    fn id_type_default_is_bigserial() {
        assert_eq!(IdType::default(), IdType::BigSerial);
    }

    #[test]
    fn id_type_bigserial_mappings() {
        let id = IdType::BigSerial;
        assert_eq!(id.rust_type(), "i64");
        assert_eq!(id.schema_type(), "Int8");
        assert_eq!(id.pk_sql(), "BIGSERIAL PRIMARY KEY");
        assert!(id.migration_comment().is_none());
    }

    #[test]
    fn id_type_uuid_mappings() {
        let id = IdType::Uuid;
        assert_eq!(id.rust_type(), "uuid::Uuid");
        assert_eq!(id.schema_type(), "Uuid");
        assert_eq!(id.pk_sql(), "UUID PRIMARY KEY DEFAULT gen_random_uuid()");
        let comment = id
            .migration_comment()
            .expect("uuid should have a trade-off comment");
        assert!(
            comment.contains("UUIDv7"),
            "comment should mention UUIDv7: {comment}"
        );
    }

    #[test]
    fn id_type_parse_accepts_uuid_case_insensitive() {
        for token in ["uuid", "Uuid", "UUID"] {
            assert_eq!(
                IdType::parse(token).unwrap(),
                IdType::Uuid,
                "'{token}' should parse to Uuid"
            );
        }
    }

    #[test]
    fn id_type_parse_accepts_bigint_aliases() {
        for token in ["bigint", "bigserial", "i64", "BigInt"] {
            assert_eq!(
                IdType::parse(token).unwrap(),
                IdType::BigSerial,
                "'{token}' should parse to BigSerial"
            );
        }
    }

    // ── references field kind (issue #1026) ────────────────────────────────

    #[test]
    fn parse_references_resolves_column_name_to_id() {
        let f = parse_field("post:references").unwrap();
        assert_eq!(f.name, "post_id");
        assert_eq!(f.kind, FieldKind::References);
        assert!(!f.nullable);
    }

    #[test]
    fn parse_references_pascal_case_also_accepted() {
        let f = parse_field("post:References").unwrap();
        assert_eq!(f.name, "post_id");
        assert_eq!(f.kind, FieldKind::References);
    }

    #[test]
    fn parse_references_does_not_double_suffix_already_named_column() {
        let f = parse_field("post_id:references").unwrap();
        assert_eq!(f.name, "post_id");
    }

    #[test]
    fn parse_references_multi_word_base_name() {
        let f = parse_field("blog_post:references").unwrap();
        assert_eq!(f.name, "blog_post_id");
        assert_eq!(f.reference_table().as_deref(), Some("blog_posts"));
    }

    #[test]
    fn references_rust_type_is_i64() {
        let f = parse_field("post:references").unwrap();
        assert_eq!(f.rust_type(), "i64");
    }

    #[test]
    fn references_sql_type_is_bigint() {
        let f = parse_field("post:references").unwrap();
        assert_eq!(f.sql_type(), "BIGINT");
        assert_eq!(f.sql_nullability(), "NOT NULL");
    }

    #[test]
    fn references_schema_type_is_int8() {
        let f = parse_field("post:references").unwrap();
        assert_eq!(f.schema_type(), "Int8");
    }

    #[test]
    fn references_target_table_derived_via_pluralize() {
        let f = parse_field("post:references").unwrap();
        assert_eq!(f.reference_table().as_deref(), Some("posts"));
    }

    #[test]
    fn references_nullable_form_with_question_mark() {
        let f = parse_field("post:references?").unwrap();
        assert_eq!(f.name, "post_id");
        assert!(f.nullable);
        assert_eq!(f.rust_type(), "Option<i64>");
        assert_eq!(f.schema_type(), "Nullable<Int8>");
        assert_eq!(f.sql_nullability(), "NULL");
    }

    #[test]
    fn references_nullable_form_tolerates_internal_whitespace() {
        // `parse_field` trims the raw type text before `parse_type` sees it,
        // but a CLI token could still carry whitespace before the `?` (e.g.
        // `"post: references ?"` after the name/type split trims only the
        // outer edges); this must still resolve to a nullable reference
        // rather than "unsupported type".
        let f = parse_field("post: references ?").unwrap();
        assert_eq!(f.kind, FieldKind::References);
        assert!(f.nullable);
    }

    #[test]
    fn question_mark_suffix_is_not_a_general_nullability_marker() {
        // `?` is only recognized for `references` — every other type must use
        // `Option<…>`. Otherwise this silently expands the DSL's grammar
        // (undocumented and untested) for every field kind.
        for token in [
            "count:i64?",
            "flag:bool?",
            "title:String?",
            "data:Vec<u8>?",
            "id:Uuid?",
        ] {
            let err = parse_field(token).unwrap_err();
            assert!(
                err.to_string().contains("unsupported type"),
                "expected '{token}' to be rejected like before: {err}"
            );
        }
    }

    #[test]
    fn references_is_reference_predicate() {
        assert!(FieldKind::References.is_reference());
        assert!(!FieldKind::I64.is_reference());
    }

    #[test]
    fn non_reference_field_has_no_reference_table() {
        let f = parse_field("title:String").unwrap();
        assert_eq!(f.reference_table(), None);
    }

    #[test]
    fn references_appears_in_supported_types_constant() {
        assert!(
            SUPPORTED_TYPES.contains("references"),
            "SUPPORTED_TYPES must list references"
        );
    }

    // ── constraint modifiers (issue #1388 validation + #1146 label) ─────────

    #[test]
    fn parse_string_length_constraint() {
        let f = parse_field("title:String{min=3,max=120}").unwrap();
        assert_eq!(f.name, "title");
        assert_eq!(f.kind, FieldKind::String);
        assert_eq!(f.constraints.min.as_deref(), Some("3"));
        assert_eq!(f.constraints.max.as_deref(), Some("120"));
        assert_eq!(f.validation_attrs(), vec!["length(min = 3, max = 120)"]);
    }

    #[test]
    fn parse_string_email_constraint() {
        let f = parse_field("contact:String{email}").unwrap();
        assert!(f.constraints.email);
        assert_eq!(f.validation_attrs(), vec!["email"]);
    }

    #[test]
    fn parse_string_url_constraint() {
        let f = parse_field("homepage:String{url}").unwrap();
        assert!(f.constraints.url);
        assert_eq!(f.validation_attrs(), vec!["url"]);
    }

    #[test]
    fn parse_numeric_range_constraint() {
        let f = parse_field("age:i32{min=0,max=130}").unwrap();
        assert_eq!(f.kind, FieldKind::I32);
        assert_eq!(f.validation_attrs(), vec!["range(min = 0, max = 130)"]);
    }

    #[test]
    fn parse_float_range_constraint_emits_decimal_literals() {
        // An integer bound on an f64 field must be emitted with a decimal
        // point, or the generated `#[validate(range(...))]` comparison fails
        // to type-check against the f64 field.
        let f = parse_field("ratio:f64{min=0,max=1}").unwrap();
        assert_eq!(f.validation_attrs(), vec!["range(min = 0.0, max = 1.0)"]);
    }

    #[test]
    fn parse_min_only_constraint() {
        let f = parse_field("title:String{min=3}").unwrap();
        assert_eq!(f.constraints.min.as_deref(), Some("3"));
        assert_eq!(f.constraints.max, None);
        assert_eq!(f.validation_attrs(), vec!["length(min = 3)"]);
    }

    #[test]
    fn parse_reference_label_constraint() {
        let f = parse_field("post:references{label:title}").unwrap();
        assert_eq!(f.name, "post_id");
        assert_eq!(f.kind, FieldKind::References);
        assert_eq!(f.constraints.label.as_deref(), Some("title"));
        // `references` never fans out to a `#[validate]` rule.
        assert!(f.validation_attrs().is_empty());
    }

    #[test]
    fn reference_label_does_not_break_unique_detection() {
        // The colon inside `{label:title}` must not be mistaken for a
        // `:unique` modifier split.
        let f = parse_field("post:references{label:title}").unwrap();
        assert!(!f.unique);
    }

    #[test]
    fn constraint_composes_with_unique_modifier() {
        let f = parse_field("email:String{email}:unique").unwrap();
        assert!(f.unique);
        assert!(f.constraints.email);
    }

    #[test]
    fn bare_unique_modifier_still_parses() {
        let f = parse_field("email:String:unique").unwrap();
        assert!(f.unique);
        assert!(f.constraints.is_empty());
    }

    #[test]
    fn unknown_constraint_modifier_is_rejected_by_name() {
        // AC5: a misspelled modifier fails loudly, naming the token.
        let err = parse_field("title:String{maxx=5}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("maxx"), "must name the bad token: {msg}");
    }

    #[test]
    fn email_constraint_on_numeric_field_is_rejected() {
        let err = parse_field("age:i32{email}").unwrap_err();
        assert!(err.to_string().contains("email"));
    }

    #[test]
    fn label_constraint_on_non_reference_is_rejected() {
        let err = parse_field("title:String{label:foo}").unwrap_err();
        assert!(err.to_string().contains("references"));
    }

    #[test]
    fn min_greater_than_max_is_rejected() {
        let err = parse_field("title:String{min=10,max=3}").unwrap_err();
        assert!(err.to_string().contains("min"));
    }

    #[test]
    fn non_integer_length_bound_is_rejected() {
        let err = parse_field("title:String{max=abc}").unwrap_err();
        assert!(err.to_string().contains("abc"));
    }

    #[test]
    fn constraint_on_unconstrainable_kind_is_rejected() {
        // A bool field takes no `{…}` modifiers.
        let err = parse_field("active:bool{min=0}").unwrap_err();
        assert!(err.to_string().contains("not supported") || err.to_string().contains("bool"));
    }

    #[test]
    fn empty_constraint_block_is_rejected() {
        let err = parse_field("title:String{}").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn nullable_string_with_constraint_parses() {
        let f = parse_field("bio:Option<String>{max=200}").unwrap();
        assert!(f.nullable);
        assert_eq!(f.constraints.max.as_deref(), Some("200"));
        assert_eq!(f.validation_attrs(), vec!["length(max = 200)"]);
    }

    #[test]
    fn bare_field_has_empty_constraints() {
        let f = parse_field("title:String").unwrap();
        assert!(f.constraints.is_empty());
        assert!(f.validation_attrs().is_empty());
    }

    #[test]
    fn id_type_parse_rejects_unknown_with_accepted_values_listed() {
        for bad in ["guid", "serial4", "int", "ulid"] {
            let err = IdType::parse(bad).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(bad),
                "error must echo the bad value '{bad}': {msg}"
            );
            assert!(msg.contains("uuid"), "error must list 'uuid': {msg}");
            assert!(msg.contains("bigint"), "error must list 'bigint': {msg}");
        }
    }

    // ------------------------------------------------------------------
    // enum field kind (issue #1030)
    // ------------------------------------------------------------------

    #[test]
    fn parse_enum_field() {
        let f = parse_field("status:enum{draft,published,archived}").unwrap();
        assert_eq!(f.name, "status");
        assert_eq!(f.kind, FieldKind::Enum);
        assert_eq!(f.variants, vec!["draft", "published", "archived"]);
        assert!(!f.nullable);
        assert_eq!(f.rust_type(), "Status");
        assert_eq!(f.enum_type_name().as_deref(), Some("Status"));
        assert_eq!(f.schema_type(), "Text");
        assert_eq!(f.sql_type(), "TEXT");
        assert_eq!(f.sql_nullability(), "NOT NULL");
        assert!(f.is_enum());
        assert!(FieldKind::Enum.is_enum());
        assert!(!FieldKind::String.is_enum());
    }

    #[test]
    fn parse_enum_field_multiword_name_pascalizes_type() {
        let f = parse_field("review_state:enum{open,closed}").unwrap();
        assert_eq!(f.rust_type(), "ReviewState");
        assert_eq!(f.enum_type_name().as_deref(), Some("ReviewState"));
    }

    #[test]
    fn parse_enum_field_trims_variant_whitespace() {
        let f = parse_field("status:enum{ draft , published }").unwrap();
        assert_eq!(f.variants, vec!["draft", "published"]);
    }

    #[test]
    fn parse_nullable_enum_field() {
        let f = parse_field("status:Option<enum{draft,published}>").unwrap();
        assert_eq!(f.kind, FieldKind::Enum);
        assert!(f.nullable);
        assert_eq!(f.rust_type(), "Option<Status>");
        assert_eq!(f.schema_type(), "Nullable<Text>");
        assert_eq!(f.sql_nullability(), "NULL");
    }

    #[test]
    fn enum_rejects_non_ident_variant() {
        // `2fa` cannot become a Rust enum variant (`2Fa` is not an identifier).
        let err = parse_field("status:enum{2fa,ok}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2fa"), "must name the bad variant: {msg}");
        assert!(
            msg.contains("identifier"),
            "must explain the identifier rule: {msg}"
        );
    }

    #[test]
    fn enum_rejects_variant_pascalizing_to_leading_digit() {
        // `_2fa` passes `is_valid_ident` (leading `_` is allowed), but
        // `pascal("_2fa")` strips the leading underscore and capitalizes
        // nothing before the digit, producing `2fa` — not a valid Rust
        // identifier, so the generated enum variant fails to compile.
        let err = parse_field("status:enum{_2fa,ok}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("_2fa"), "must name the bad variant: {msg}");
        assert!(
            msg.contains("identifier"),
            "must explain the identifier rule: {msg}"
        );
    }

    #[test]
    fn enum_rejects_variant_pascalizing_to_empty() {
        // `__` passes `is_valid_ident` (all underscores are allowed chars),
        // but `pascal("__")` produces an empty string — not a valid Rust
        // identifier.
        let err = parse_field("status:enum{__,ok}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("__"), "must name the bad variant: {msg}");
        assert!(
            msg.contains("identifier"),
            "must explain the identifier rule: {msg}"
        );
    }

    #[test]
    fn enum_rejects_uppercase_variant() {
        let err = parse_field("status:enum{Draft,ok}").unwrap_err();
        assert!(err.to_string().contains("snake_case identifier"));
    }

    #[test]
    fn enum_rejects_keyword_variant() {
        // Consistent with field-name guarding: never emit code needing r#…
        let err = parse_field("status:enum{type,ok}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("type"), "must name the bad variant: {msg}");
        assert!(msg.contains("keyword"), "must explain why: {msg}");
    }

    #[test]
    fn enum_rejects_duplicate_variants() {
        let err = parse_field("status:enum{draft,draft}").unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn enum_rejects_pascal_colliding_variants() {
        // `in_review` and `in__review` both pascalize to `InReview`.
        let err = parse_field("status:enum{in_review,in__review}").unwrap_err();
        assert!(err.to_string().contains("InReview"));
    }

    #[test]
    fn enum_rejects_empty_body() {
        let err = parse_field("status:enum{}").unwrap_err();
        assert!(err.to_string().contains("variant"));
    }

    #[test]
    fn enum_rejects_single_variant() {
        let err = parse_field("status:enum{draft}").unwrap_err();
        assert!(err.to_string().contains("at least two"));
    }

    #[test]
    fn enum_rejects_unclosed_brace() {
        let err = parse_field("status:enum{draft,published").unwrap_err();
        assert!(err.to_string().contains("enum{"));
    }

    #[test]
    fn enum_error_hints_about_shell_brace_expansion() {
        // bash/zsh expand an unquoted `enum{a,b}` into `enuma enumb`, so the
        // token the CLI actually receives looks like `status:enumdraft`. Point
        // the user at quoting instead of a bare "unsupported type".
        let err = parse_field("status:enumdraft").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("quote"), "must suggest quoting: {msg}");
    }

    #[test]
    fn enum_prefixed_typo_still_lists_supported_types() {
        // A type name that happens to start with `enum` but isn't a genuine
        // shell-mangled enum token (e.g. a typo like `enumerable`) must still
        // see the full supported-types list, not just the brace-expansion
        // hint — the two audiences (a real shell-expansion victim and a
        // plain typo) get one message that serves both.
        let err = parse_field("status:enumerable").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("quote"), "got: {msg}");
        assert!(msg.contains("Supported:"), "got: {msg}");
        assert!(msg.contains("String"), "got: {msg}");
    }

    #[test]
    fn enum_appears_in_supported_types_constant() {
        assert!(
            SUPPORTED_TYPES.contains("enum{"),
            "SUPPORTED_TYPES must list enum{{…}}"
        );
    }

    // ── `unique` field modifier (issue #1032) ───────────────────────────────

    #[test]
    fn parse_unique_string_field() {
        let f = parse_field("email:String:unique").unwrap();
        assert_eq!(f.name, "email");
        assert_eq!(f.kind, FieldKind::String);
        assert!(f.unique);
        assert!(!f.nullable);
    }

    #[test]
    fn parse_non_unique_field_defaults_to_false() {
        let f = parse_field("title:String").unwrap();
        assert!(!f.unique);
    }

    #[test]
    fn parse_unique_option_field_keeps_nullable_and_unique() {
        let f = parse_field("nickname:Option<String>:unique").unwrap();
        assert!(f.nullable);
        assert!(f.unique);
    }

    #[test]
    fn parse_unique_i64_field() {
        let f = parse_field("external_id:i64:unique").unwrap();
        assert_eq!(f.kind, FieldKind::I64);
        assert!(f.unique);
    }

    #[test]
    fn parse_unique_enum_field() {
        let f = parse_field("slug:enum{a,b}:unique").unwrap();
        assert_eq!(f.kind, FieldKind::Enum);
        assert!(f.unique);
    }

    #[test]
    fn parse_unique_references_field() {
        let f = parse_field("profile:references:unique").unwrap();
        assert_eq!(f.name, "profile_id");
        assert_eq!(f.kind, FieldKind::References);
        assert!(f.unique);
    }

    #[test]
    fn parse_unknown_modifier_is_rejected() {
        let err = parse_field("email:String:bogus").unwrap_err();
        assert!(
            err.to_string().contains("bogus") && err.to_string().contains("unique"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_unique_trims_whitespace_around_modifier() {
        let f = parse_field("email:String: unique ").unwrap();
        assert!(f.unique);
    }

    #[test]
    fn unique_appears_in_supported_types_constant() {
        assert!(
            SUPPORTED_TYPES.contains("unique"),
            "SUPPORTED_TYPES must document the :unique modifier"
        );
    }

    // ── decimal field kind (issue #1038) ────────────────────────────────────

    #[test]
    fn parse_decimal_field_lowercase() {
        let f = parse_field("price:decimal").unwrap();
        assert_eq!(f.name, "price");
        assert_eq!(
            f.kind,
            FieldKind::Decimal {
                precision: 12,
                scale: 2
            }
        );
        assert!(!f.nullable);
    }

    #[test]
    fn parse_decimal_field_pascal_case() {
        let f = parse_field("price:Decimal").unwrap();
        assert_eq!(
            f.kind,
            FieldKind::Decimal {
                precision: 12,
                scale: 2
            }
        );
    }

    #[test]
    fn decimal_defaults_to_precision_12_scale_2() {
        let f = parse_field("price:decimal").unwrap();
        assert_eq!(f.sql_column_type(), "NUMERIC(12,2)");
    }

    #[test]
    fn decimal_custom_precision_and_scale() {
        let f = parse_field("price:decimal{10,2}").unwrap();
        assert_eq!(
            f.kind,
            FieldKind::Decimal {
                precision: 10,
                scale: 2
            }
        );
        assert_eq!(f.sql_column_type(), "NUMERIC(10,2)");
    }

    #[test]
    fn decimal_rust_type_is_rust_decimal() {
        let f = parse_field("price:decimal").unwrap();
        assert_eq!(f.rust_type(), "rust_decimal::Decimal");
    }

    #[test]
    fn decimal_schema_type_is_numeric() {
        let f = parse_field("price:decimal").unwrap();
        assert_eq!(f.schema_type(), "Numeric");
    }

    #[test]
    fn decimal_sql_type_is_numeric() {
        let f = parse_field("price:decimal").unwrap();
        assert_eq!(f.sql_type(), "NUMERIC");
    }

    #[test]
    fn optional_decimal_parses() {
        let f = parse_field("balance:Option<decimal>").unwrap();
        assert_eq!(
            f.kind,
            FieldKind::Decimal {
                precision: 12,
                scale: 2
            }
        );
        assert!(f.nullable);
        assert_eq!(f.rust_type(), "Option<rust_decimal::Decimal>");
        assert_eq!(f.schema_type(), "Nullable<Numeric>");
        assert_eq!(f.sql_nullability(), "NULL");
    }

    #[test]
    fn optional_decimal_with_precision_scale_parses() {
        let f = parse_field("balance:Option<decimal{10,2}>").unwrap();
        assert_eq!(
            f.kind,
            FieldKind::Decimal {
                precision: 10,
                scale: 2
            }
        );
        assert!(f.nullable);
        assert_eq!(f.sql_column_type(), "NUMERIC(10,2)");
    }

    #[test]
    fn unique_decimal_field_parses() {
        let f = parse_field("price:decimal:unique").unwrap();
        assert_eq!(
            f.kind,
            FieldKind::Decimal {
                precision: 12,
                scale: 2
            }
        );
        assert!(f.unique);
    }

    #[test]
    fn decimal_rejects_non_numeric_precision() {
        let err = parse_field("price:decimal{abc,2}").unwrap_err();
        assert!(err.to_string().contains("precision"));
    }

    #[test]
    fn decimal_rejects_non_numeric_scale() {
        let err = parse_field("price:decimal{10,xyz}").unwrap_err();
        assert!(err.to_string().contains("scale"));
    }

    #[test]
    fn decimal_rejects_scale_greater_than_precision() {
        let err = parse_field("price:decimal{2,10}").unwrap_err();
        assert!(err.to_string().contains("scale"));
    }

    #[test]
    fn decimal_rejects_zero_precision() {
        let err = parse_field("price:decimal{0,0}").unwrap_err();
        assert!(err.to_string().contains("precision"));
    }

    #[test]
    fn decimal_rejects_precision_over_rust_decimal_max() {
        // 29 exceeds rust_decimal::Decimal's 28-significant-digit range, even
        // though Postgres's own NUMERIC would happily hold it — the DSL caps
        // at what the generated struct field can actually represent, not
        // what the column type technically permits.
        let err = parse_field("price:decimal{29,2}").unwrap_err();
        assert!(err.to_string().contains("precision"));
    }

    #[test]
    fn decimal_accepts_precision_at_rust_decimal_max() {
        let f = parse_field("price:decimal{28,2}").unwrap();
        assert_eq!(
            f.kind,
            FieldKind::Decimal {
                precision: 28,
                scale: 2
            }
        );
    }

    #[test]
    fn decimal_rejects_missing_closing_brace() {
        let err = parse_field("price:decimal{10,2").unwrap_err();
        assert!(err.to_string().contains("decimal{"));
    }

    #[test]
    fn decimal_error_hints_about_shell_brace_expansion() {
        // bash/zsh expand an unquoted `decimal{10,2}` into two separate
        // words, so the token the CLI actually receives looks like
        // `price:decimal10` — point the user at quoting, same as `enum{...}`.
        let err = parse_field("price:decimal10").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("quote"), "must suggest quoting: {msg}");
        assert!(msg.contains("Supported:"), "got: {msg}");
    }

    #[test]
    fn decimal_rejects_wrong_number_of_brace_args() {
        let err = parse_field("price:decimal{10}").unwrap_err();
        assert!(err.to_string().contains("precision,scale"));
    }

    #[test]
    fn decimal_prefixed_typo_still_lists_supported_types() {
        // A type name that happens to start with `decimal` but isn't a
        // genuine shell-mangled decimal token (e.g. a typo like
        // `decimalize`) must still see the full supported-types list, not
        // just the brace-expansion hint — mirrors enum's
        // `enum_prefixed_typo_still_lists_supported_types` precedent.
        let err = parse_field("price:decimalize").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("quote"), "got: {msg}");
        assert!(msg.contains("Supported:"), "got: {msg}");
        assert!(msg.contains("String"), "got: {msg}");
    }

    #[test]
    fn parse_decimal_type_tolerates_untrimmed_input() {
        // Defensive-in-depth (PR review, gemini-code-assist): `parse_field`
        // already trims `ty` before any type parser sees it, so this isn't
        // reachable through the public `parse_field` API today — but
        // `parse_decimal_type` re-trims `body` itself too, so a direct call
        // (or a future caller that doesn't pre-trim) with trailing
        // whitespace after the closing brace still parses instead of
        // failing `strip_suffix('}')`.
        assert_eq!(
            parse_decimal_type("decimal{10,2} ").unwrap(),
            Some((10, 2, false))
        );
    }

    #[test]
    fn decimal_appears_in_supported_types_constant() {
        assert!(
            SUPPORTED_TYPES.contains("decimal"),
            "SUPPORTED_TYPES must list decimal{{p,s}}"
        );
    }

    #[test]
    fn decimal_in_list_of_fields() {
        let tokens = vec!["title:String".into(), "price:decimal".into()];
        let fields = parse_fields(&tokens).unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(
            fields[1].kind,
            FieldKind::Decimal {
                precision: 12,
                scale: 2
            }
        );
    }

    #[test]
    fn decimal_numeric_stays_unmapped_for_db_pull_inverse() {
        // Precision/scale can't be reconstructed from `numeric` udt_name alone
        // (same precedent as `jsonb`/Attachment), so `db pull` introspection
        // deliberately leaves it unsupported rather than guessing.
        assert!(sql_type_to_field_kind("numeric").is_none());
    }
}
