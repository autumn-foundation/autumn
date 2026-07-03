//! Field-type DSL parser for `autumn generate`.
//!
//! Turns command-line tokens like `title:String`, `tags:Vec<u8>`, or
//! `published:Option<bool>` into a structured [`Field`] that knows both its
//! Rust type (for the `#[model]` struct) and its SQL type (for the migration).

use super::GenerateError;
use super::naming;

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

    /// `"NULL"` or `"NOT NULL"` to append in the migration.
    #[must_use]
    pub const fn sql_nullability(&self) -> &'static str {
        if self.nullable { "NULL" } else { "NOT NULL" }
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
    enum{a,b,…}, Option<…>";

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
pub fn parse_field(token: &str) -> Result<Field, GenerateError> {
    let (name, ty) = token
        .split_once(':')
        .ok_or_else(|| GenerateError::InvalidField {
            token: token.to_owned(),
            reason: "expected `name:Type` (missing colon)".into(),
        })?;

    let name = name.trim();
    let ty = ty.trim();

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
        });
    }

    let (kind, nullable) = parse_type(ty).ok_or_else(|| GenerateError::InvalidField {
        token: token.to_owned(),
        reason: format!("unsupported type '{ty}'. Supported: {SUPPORTED_TYPES}"),
    })?;

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
    })
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
        let err = parse_field("price:Decimal").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Decimal"));
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
}
