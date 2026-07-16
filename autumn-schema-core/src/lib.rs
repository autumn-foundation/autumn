//! Canonical, dialect-independent schema IR for the Autumn web framework.
//!
//! This crate is the single source of truth for the **shape of a database
//! table** and the **type mappings** between a logical column type and the two
//! SQL dialects Autumn targets (`PostgreSQL` and `SQLite`). It is deliberately a
//! *leaf* crate: it depends only on `serde` (for a serializable IR) and pulls in
//! neither `diesel`, `syn`, nor `autumn-web`, so both the proc-macro crate
//! (`autumn-macros`) and the CLI (`autumn-cli`) can depend on it in a later
//! slice without risking a dependency cycle.
//!
//! # Why this exists (the "declarative schema" wave)
//!
//! Today a table's shape is derived *transiently* from the CLI's
//! `generate::dsl::FieldKind` at generate time and then thrown away — the same
//! logical shape is re-expressed three times (the `#[model]` struct, the diesel
//! `schema.rs`, and the SQL migration) with no shared, inspectable
//! representation. This crate holds that shape as a canonical, serializable IR
//! plus the bidirectional PG/`SQLite` type mappings mirrored from
//! `autumn-cli/src/generate/dsl.rs`. Later slices (a `syn`-backed parser and a
//! checked-in schema snapshot) build on it.
//!
//! # Fidelity contract
//!
//! Every mapping here is mirrored **byte-for-byte** from `dsl::FieldKind` /
//! `dsl::IdType`. The parity unit tests inside `dsl.rs` lock the two together so
//! neither can drift silently. If you change a mapping here, the parity test
//! fails until `dsl.rs` agrees (and vice-versa).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// The SQL dialect a [`Schema`] / [`Table`] targets.
///
/// A schema carries its backend as a *dialect tag* (Decision 5): the same
/// logical column type renders to different DDL and diesel tokens per backend,
/// and a checked-in schema snapshot is therefore locked to the provider it was
/// generated against (a "provider-lock"). Mirrors `autumn_web::config::DatabaseBackend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    /// `PostgreSQL` — the fully wired runtime backend.
    Postgres,
    /// `SQLite` — the lightweight embedded backend (issue #1614).
    Sqlite,
}

/// A logical, dialect-independent column type.
///
/// This is the canonical vocabulary the IR speaks in; the concrete Rust type,
/// diesel `table!` token, and SQL DDL type are all *derived* from a
/// `ColumnType` via [`ColumnType::rust_type`], [`ColumnType::diesel_type`], and
/// [`ColumnType::sql_type`]. It mirrors the storage-representation axis of
/// `dsl::FieldKind` — but note that a foreign key is **not** a `ColumnType`
/// variant: a `references` column stores an [`Int64`](ColumnType::Int64) and
/// carries its foreign-key relationship as the [`Column::references`] property
/// instead (see [`ForeignKey`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnType {
    /// `String` — PG `TEXT` / `SQLite` `TEXT`. (The DSL's `Text` alias collapses
    /// here too — both render an identical column.)
    Text,
    /// `i32` — PG `INTEGER` / `SQLite` `INTEGER`.
    Int32,
    /// `i64` — PG `BIGINT` / `SQLite` `INTEGER`. Also the storage type of a
    /// foreign-key column (see [`Column::references`]).
    Int64,
    /// `bool` — PG `BOOLEAN` / `SQLite` `INTEGER` (`0`/`1`).
    Bool,
    /// `f32` — PG `REAL` / `SQLite` `REAL`.
    Float32,
    /// `f64` — PG `DOUBLE PRECISION` / `SQLite` `REAL`.
    Float64,
    /// `uuid::Uuid` — PG `UUID` / `SQLite` `TEXT` (canonical string form).
    Uuid,
    /// `chrono::NaiveDateTime` — PG `TIMESTAMP` / `SQLite` `TEXT` (ISO-8601).
    Timestamp,
    /// `chrono::DateTime<chrono::Utc>` — PG `TIMESTAMPTZ` / `SQLite` `TEXT` (RFC 3339).
    TimestampTz,
    /// `Vec<u8>` — PG `BYTEA` / `SQLite` `BLOB`.
    Bytes,
    /// A file attachment: `autumn_web::storage::Blob` metadata stored inline —
    /// PG `JSONB` / `SQLite` `TEXT`. Conventionally nullable (`Option<Blob>`); the
    /// bytes themselves live in the configured storage backend.
    Attachment,
    /// An exact-precision decimal — PG `NUMERIC(precision, scale)` / `SQLite`
    /// `TEXT` (`SQLite`'s `NUMERIC` affinity would coerce to a lossy float, so the
    /// value round-trips through `rust_decimal`'s text form instead). Defaults to
    /// `{12, 2}` (money-shaped) when the DSL modifier is omitted.
    Decimal {
        /// Total number of significant digits (`NUMERIC(precision, _)`).
        precision: u8,
        /// Number of digits after the decimal point (`NUMERIC(_, scale)`).
        scale: u8,
    },
    /// A closed-set column — stored as `TEXT` with a `CHECK` constraint
    /// enumerating the allowed values. The Rust type reported by
    /// [`ColumnType::rust_type`] is the storage-representation fallback `String`;
    /// the concrete generated enum-type name is a later-slice codegen concern
    /// and is intentionally not modelled here.
    Enum {
        /// The allowed variant labels, in declaration order.
        variants: Vec<String>,
    },
}

impl ColumnType {
    /// The Rust type token used inside a `#[model]` struct, ignoring any
    /// `Option<…>` nullability wrapping (that is applied by the caller from
    /// [`Column::nullable`]).
    ///
    /// Mirrors `dsl::FieldKind::rust_type`. For [`Enum`](Self::Enum) this returns
    /// the storage fallback `String` — the concrete generated enum-type name is a
    /// codegen concern of a later slice, not part of the canonical IR.
    #[must_use]
    pub fn rust_type(&self) -> String {
        match self {
            Self::Text | Self::Enum { .. } => "String",
            Self::Int32 => "i32",
            Self::Int64 => "i64",
            Self::Bool => "bool",
            Self::Float32 => "f32",
            Self::Float64 => "f64",
            Self::Uuid => "uuid::Uuid",
            Self::Timestamp => "chrono::NaiveDateTime",
            Self::TimestampTz => "chrono::DateTime<chrono::Utc>",
            Self::Bytes => "Vec<u8>",
            Self::Attachment => "autumn_web::storage::Blob",
            Self::Decimal { .. } => "rust_decimal::Decimal",
        }
        .to_owned()
    }

    /// The diesel `table!` schema type token for `backend`.
    ///
    /// Mirrors `dsl::FieldKind::schema_type_for`. On `SQLite` the Postgres-only
    /// diesel sql-types (`Timestamptz`, `Jsonb`, `Uuid`, `Bytea`, `Numeric`) are
    /// remapped to types diesel's `SQLite` backend supports; see
    /// [`ColumnType::sqlite_has_diesel_conversion`] for which of those remappings
    /// actually compile in a generated app.
    #[must_use]
    pub const fn diesel_type(&self, backend: Backend) -> &'static str {
        match backend {
            Backend::Postgres => match self {
                Self::Text | Self::Enum { .. } => "Text",
                Self::Int32 => "Int4",
                Self::Int64 => "Int8",
                Self::Bool => "Bool",
                Self::Float32 => "Float4",
                Self::Float64 => "Float8",
                Self::Uuid => "Uuid",
                Self::Timestamp => "Timestamp",
                Self::TimestampTz => "Timestamptz",
                Self::Bytes => "Bytea",
                Self::Attachment => "Jsonb",
                Self::Decimal { .. } => "Numeric",
            },
            Backend::Sqlite => match self {
                Self::Text
                | Self::Uuid
                | Self::Attachment
                | Self::Decimal { .. }
                | Self::Enum { .. } => "Text",
                Self::Int32 => "Int4",
                Self::Int64 => "Int8",
                Self::Bool => "Bool",
                Self::Float32 => "Float4",
                Self::Float64 => "Float8",
                // `NaiveDateTime` -> core, ungated `Timestamp` (compiles).
                // `DateTime<Utc>` -> nominal `Timestamp` for documentation only;
                // it is rejected at generate time (see `sqlite_has_diesel_conversion`).
                Self::Timestamp | Self::TimestampTz => "Timestamp",
                Self::Bytes => "Binary",
            },
        }
    }

    /// The SQL DDL column type for `backend`, *including* any type parameters
    /// (so [`Decimal`](Self::Decimal) renders the full `NUMERIC(precision, scale)`
    /// on Postgres).
    ///
    /// Mirrors `dsl::Field::sql_column_type_for` — i.e. the exact string that
    /// reaches a `CREATE TABLE` / `ADD COLUMN`. On `SQLite`, `Decimal` collapses to
    /// `TEXT` with no `(p, s)` suffix (`SQLite` has no fixed-precision `NUMERIC`).
    #[must_use]
    pub fn sql_type(&self, backend: Backend) -> String {
        match backend {
            Backend::Postgres => match self {
                Self::Text | Self::Enum { .. } => "TEXT".to_owned(),
                Self::Int32 => "INTEGER".to_owned(),
                Self::Int64 => "BIGINT".to_owned(),
                Self::Bool => "BOOLEAN".to_owned(),
                Self::Float32 => "REAL".to_owned(),
                Self::Float64 => "DOUBLE PRECISION".to_owned(),
                Self::Uuid => "UUID".to_owned(),
                Self::Timestamp => "TIMESTAMP".to_owned(),
                Self::TimestampTz => "TIMESTAMPTZ".to_owned(),
                Self::Bytes => "BYTEA".to_owned(),
                Self::Attachment => "JSONB".to_owned(),
                Self::Decimal { precision, scale } => format!("NUMERIC({precision},{scale})"),
            },
            Backend::Sqlite => match self {
                Self::Text
                | Self::Uuid
                | Self::Timestamp
                | Self::TimestampTz
                | Self::Attachment
                | Self::Decimal { .. }
                | Self::Enum { .. } => "TEXT".to_owned(),
                Self::Int32 | Self::Int64 | Self::Bool => "INTEGER".to_owned(),
                Self::Float32 | Self::Float64 => "REAL".to_owned(),
                Self::Bytes => "BLOB".to_owned(),
            },
        }
    }

    /// Whether this type's rendered Rust model type has a working diesel
    /// `FromSql`/`ToSql` on diesel's `SQLite` backend in a generated app's feature
    /// set (diesel `sqlite` + `chrono`, without `uuid`/`numeric`).
    ///
    /// Mirrors `dsl::FieldKind::sqlite_has_diesel_conversion`: `false` for
    /// [`Uuid`](Self::Uuid), [`Attachment`](Self::Attachment),
    /// [`Decimal`](Self::Decimal), [`TimestampTz`](Self::TimestampTz), and
    /// [`Enum`](Self::Enum) (all rejected at generate time on `SQLite`, issue
    /// #1924); `true` for every other type — including [`Timestamp`](Self::Timestamp)
    /// via the core, ungated diesel `Timestamp` sql-type.
    #[must_use]
    pub const fn sqlite_has_diesel_conversion(&self) -> bool {
        !matches!(
            self,
            Self::Uuid
                | Self::Attachment
                | Self::Decimal { .. }
                | Self::TimestampTz
                | Self::Enum { .. }
        )
    }

    /// Inverse of the Postgres mapping: resolve a Postgres `udt_name` (the
    /// concrete catalog type identifier such as `int4`, `int8`, `timestamptz`)
    /// to a [`ColumnType`]. This is the `db pull` introspection direction.
    ///
    /// Mirrors `dsl::sql_type_to_field_kind`. `text`/`varchar`/`bpchar` all
    /// collapse to [`Text`](Self::Text). Returns `None` for types outside the
    /// documented surface so the caller can fail loudly with a column-named
    /// error rather than silently dropping a column. Two types are deliberately
    /// unsupported even though the forward mapping produces them:
    ///
    /// - **`jsonb` → `None`**: although [`Attachment`](Self::Attachment) forward-maps
    ///   to `JSONB`, the inverse is ambiguous — a brownfield `jsonb` column is
    ///   usually arbitrary application JSON, not an Autumn `Blob`, and
    ///   introspection cannot tell them apart.
    /// - **`numeric` → `None`**: a bare `numeric` `udt_name` carries no
    ///   precision/scale, so it cannot be reconstructed into a
    ///   [`Decimal`](Self::Decimal) without guessing.
    #[must_use]
    pub fn from_pg_udt(udt: &str) -> Option<Self> {
        match udt {
            "text" | "varchar" | "bpchar" => Some(Self::Text),
            "int4" => Some(Self::Int32),
            "int8" => Some(Self::Int64),
            "bool" => Some(Self::Bool),
            "float4" => Some(Self::Float32),
            "float8" => Some(Self::Float64),
            "uuid" => Some(Self::Uuid),
            "timestamp" => Some(Self::Timestamp),
            "timestamptz" => Some(Self::TimestampTz),
            "bytea" => Some(Self::Bytes),
            // `jsonb` (ambiguous Blob-vs-JSON) and `numeric` (missing
            // precision/scale) are intentionally unsupported — see the doc above.
            _ => None,
        }
    }

    /// Inverse of [`ColumnType::rust_type`]: resolve a Rust type token (as it
    /// would appear in a `#[model]` struct) back to a [`ColumnType`]. Intended
    /// for the slice-2 `syn`-backed parser, so it is **tolerant of leading path
    /// segments** — `chrono::NaiveDateTime`, `NaiveDateTime`, and
    /// `some::path::NaiveDateTime` all resolve identically.
    ///
    /// [`Decimal`](Self::Decimal) resolves to the money-shaped default `{12, 2}`
    /// (a Rust `rust_decimal::Decimal` carries no precision/scale to recover).
    /// There is **no unambiguous inverse for [`Enum`](Self::Enum)** — a generated
    /// enum renders as its concrete `PascalCase` type name, not `String`, and
    /// its variant set cannot be recovered from a bare type token — so an enum
    /// type resolves to `None`.
    #[must_use]
    pub fn from_rust_type(rust: &str) -> Option<Self> {
        // Normalise away all whitespace so `DateTime < Utc >` and
        // `chrono::DateTime<chrono::Utc>` are handled uniformly.
        let normalized: String = rust.chars().filter(|c| !c.is_whitespace()).collect();

        // Generic forms must be matched before the path-segment split, whose
        // `::` tail would otherwise mangle the `<…>` (`Utc>` etc.).
        if normalized.contains("DateTime<") {
            return Some(Self::TimestampTz);
        }

        // Take the final `::`-separated segment (path-tolerant); a token with no
        // path (`String`, `Vec<u8>`) is returned unchanged.
        let leaf = normalized
            .rsplit("::")
            .next()
            .unwrap_or(normalized.as_str());
        match leaf {
            "String" => Some(Self::Text),
            "i32" => Some(Self::Int32),
            "i64" => Some(Self::Int64),
            "bool" => Some(Self::Bool),
            "f32" => Some(Self::Float32),
            "f64" => Some(Self::Float64),
            "Uuid" => Some(Self::Uuid),
            "NaiveDateTime" => Some(Self::Timestamp),
            "Vec<u8>" => Some(Self::Bytes),
            "Blob" => Some(Self::Attachment),
            "Decimal" => Some(Self::Decimal {
                precision: 12,
                scale: 2,
            }),
            // `Enum` has no unambiguous inverse (see the doc above).
            _ => None,
        }
    }
}

/// The primary-key strategy for a generated table.
///
/// Mirrors `dsl::IdType`. Defaults conceptually to [`BigSerial`](Self::BigSerial)
/// (today's `BIGSERIAL`/`i64` behaviour); [`Uuid`](Self::Uuid) opts into a
/// non-enumerable UUID primary key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdKind {
    /// `BIGSERIAL PRIMARY KEY` (PG) / `INTEGER PRIMARY KEY AUTOINCREMENT`
    /// (`SQLite`) — a sequential auto-increment integer.
    BigSerial,
    /// `UUID PRIMARY KEY DEFAULT gen_random_uuid()` (PG) / `TEXT PRIMARY KEY`
    /// (`SQLite`) — a non-enumerable UUID.
    Uuid,
}

impl IdKind {
    /// The Rust type for the `#[id]` struct field. Mirrors `dsl::IdType::rust_type`.
    #[must_use]
    pub const fn rust_type(self) -> &'static str {
        match self {
            Self::BigSerial => "i64",
            Self::Uuid => "uuid::Uuid",
        }
    }

    /// The diesel `table!` schema type token for `backend`. Mirrors
    /// `dsl::IdType::schema_type_for` (`SQLite` remaps `Uuid` → `Text`).
    #[must_use]
    pub const fn diesel_type(self, backend: Backend) -> &'static str {
        match (self, backend) {
            (Self::BigSerial, _) => "Int8",
            (Self::Uuid, Backend::Postgres) => "Uuid",
            (Self::Uuid, Backend::Sqlite) => "Text",
        }
    }

    /// The SQL fragment that appears after the column name in `CREATE TABLE`.
    /// Mirrors `dsl::IdType::pk_sql_for`.
    #[must_use]
    pub const fn pk_sql(self, backend: Backend) -> &'static str {
        match (self, backend) {
            (Self::BigSerial, Backend::Postgres) => "BIGSERIAL PRIMARY KEY",
            (Self::BigSerial, Backend::Sqlite) => "INTEGER PRIMARY KEY AUTOINCREMENT",
            (Self::Uuid, Backend::Postgres) => "UUID PRIMARY KEY DEFAULT gen_random_uuid()",
            (Self::Uuid, Backend::Sqlite) => "TEXT PRIMARY KEY",
        }
    }
}

/// A foreign-key relationship carried by a [`Column`] (see [`Column::references`]).
///
/// A `references` column stores an [`Int64`](ColumnType::Int64); its FK-ness is
/// this property rather than a distinct column type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    /// The referenced table name (e.g. `posts`).
    pub table: String,
    /// The referenced column name (e.g. `id`).
    pub column: String,
}

impl ForeignKey {
    /// Construct a foreign key pointing at `table`.`column`.
    pub fn new(table: impl Into<String>, column: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            column: column.into(),
        }
    }
}

/// A column-level default value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnDefault {
    /// The database's current-timestamp default (`now()` / `CURRENT_TIMESTAMP`).
    Now,
    /// A raw SQL default expression, emitted verbatim.
    Sql(String),
}

/// A single column in a [`Table`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    /// Column name (`snake_case`).
    pub name: String,
    /// The logical column type.
    pub ty: ColumnType,
    /// Whether the column admits `NULL` (renders `Option<…>` in the model).
    pub nullable: bool,
    /// Whether this column is (part of) the table's primary key.
    pub primary_key: bool,
    /// Whether the column carries a `UNIQUE` constraint / unique index.
    pub unique: bool,
    /// The column default, if any.
    pub default: Option<ColumnDefault>,
    /// The foreign-key relationship, if this column is a `references` column.
    pub references: Option<ForeignKey>,
}

impl Column {
    /// Construct a non-null, non-key, non-unique column of type `ty` with no
    /// default and no foreign key — the common case; set the remaining fields
    /// directly for anything richer.
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable: false,
            primary_key: false,
            unique: false,
            default: None,
            references: None,
        }
    }
}

/// A secondary index on a [`Table`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// The index name.
    pub name: String,
    /// The indexed columns, in order.
    pub columns: Vec<String>,
    /// Whether the index enforces uniqueness.
    pub unique: bool,
}

/// A `CHECK` constraint on a [`Table`] (e.g. the closed-set constraint an
/// [`Enum`](ColumnType::Enum) column generates).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckConstraint {
    /// The optional constraint name.
    pub name: Option<String>,
    /// The raw SQL boolean expression, emitted verbatim.
    pub expression: String,
}

/// The canonical shape of a single database table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    /// The table name (usually the pluralised model name).
    pub name: String,
    /// The table's columns, in declaration order.
    pub columns: Vec<Column>,
    /// The primary-key column name(s). A single-column integer/UUID PK is the
    /// common case; a composite key lists more than one name.
    pub primary_key: Vec<String>,
    /// Secondary indexes.
    pub indexes: Vec<Index>,
    /// `CHECK` constraints.
    pub checks: Vec<CheckConstraint>,
    /// The dialect this table's rendered DDL/diesel tokens target.
    pub backend: Backend,
    /// The **adoption marker** (Decision 4): whether Autumn owns this table's
    /// shape and may generate/alter it. A brownfield table introspected from an
    /// existing database can be represented as `managed = false` so tooling
    /// records its shape without claiming authority to migrate it.
    pub managed: bool,
}

impl Table {
    /// Construct an empty, Autumn-managed table named `name` targeting `backend`.
    pub fn new(name: impl Into<String>, backend: Backend) -> Self {
        Self {
            name: name.into(),
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            checks: Vec::new(),
            backend,
            managed: true,
        }
    }
}

/// The canonical shape of an entire schema — a set of [`Table`]s targeting one
/// [`Backend`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    /// The dialect this schema targets (its provider-lock; see [`Backend`]).
    pub backend: Backend,
    /// The tables, in a stable order.
    pub tables: Vec<Table>,
}

impl Schema {
    /// Construct an empty schema targeting `backend`.
    #[must_use]
    pub const fn new(backend: Backend) -> Self {
        Self {
            backend,
            tables: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every logical column type, for exhaustive table-driven assertions. Kept
    /// in one place so a newly added [`ColumnType`] variant that is not listed
    /// here is easy to spot in review.
    fn all_column_types() -> Vec<ColumnType> {
        vec![
            ColumnType::Text,
            ColumnType::Int32,
            ColumnType::Int64,
            ColumnType::Bool,
            ColumnType::Float32,
            ColumnType::Float64,
            ColumnType::Uuid,
            ColumnType::Timestamp,
            ColumnType::TimestampTz,
            ColumnType::Bytes,
            ColumnType::Attachment,
            ColumnType::Decimal {
                precision: 12,
                scale: 2,
            },
            ColumnType::Enum {
                variants: vec!["a".into(), "b".into(), "c".into()],
            },
        ]
    }

    #[test]
    fn rust_type_mapping_is_exhaustive_and_exact() {
        for ct in all_column_types() {
            let expected = match &ct {
                ColumnType::Text | ColumnType::Enum { .. } => "String",
                ColumnType::Int32 => "i32",
                ColumnType::Int64 => "i64",
                ColumnType::Bool => "bool",
                ColumnType::Float32 => "f32",
                ColumnType::Float64 => "f64",
                ColumnType::Uuid => "uuid::Uuid",
                ColumnType::Timestamp => "chrono::NaiveDateTime",
                ColumnType::TimestampTz => "chrono::DateTime<chrono::Utc>",
                ColumnType::Bytes => "Vec<u8>",
                ColumnType::Attachment => "autumn_web::storage::Blob",
                ColumnType::Decimal { .. } => "rust_decimal::Decimal",
            };
            assert_eq!(ct.rust_type(), expected, "rust_type for {ct:?}");
        }
    }

    #[test]
    fn diesel_type_pg_mapping() {
        let cases = [
            (ColumnType::Text, "Text"),
            (ColumnType::Int32, "Int4"),
            (ColumnType::Int64, "Int8"),
            (ColumnType::Bool, "Bool"),
            (ColumnType::Float32, "Float4"),
            (ColumnType::Float64, "Float8"),
            (ColumnType::Uuid, "Uuid"),
            (ColumnType::Timestamp, "Timestamp"),
            (ColumnType::TimestampTz, "Timestamptz"),
            (ColumnType::Bytes, "Bytea"),
            (ColumnType::Attachment, "Jsonb"),
            (
                ColumnType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                "Numeric",
            ),
            (
                ColumnType::Enum {
                    variants: vec!["a".into()],
                },
                "Text",
            ),
        ];
        for (ct, expected) in cases {
            assert_eq!(
                ct.diesel_type(Backend::Postgres),
                expected,
                "pg diesel_type for {ct:?}"
            );
        }
    }

    #[test]
    fn diesel_type_sqlite_mapping() {
        let cases = [
            (ColumnType::Text, "Text"),
            (ColumnType::Int32, "Int4"),
            (ColumnType::Int64, "Int8"),
            (ColumnType::Bool, "Bool"),
            (ColumnType::Float32, "Float4"),
            (ColumnType::Float64, "Float8"),
            (ColumnType::Uuid, "Text"),
            (ColumnType::Timestamp, "Timestamp"),
            (ColumnType::TimestampTz, "Timestamp"),
            (ColumnType::Bytes, "Binary"),
            (ColumnType::Attachment, "Text"),
            (
                ColumnType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                "Text",
            ),
            (
                ColumnType::Enum {
                    variants: vec!["a".into()],
                },
                "Text",
            ),
        ];
        for (ct, expected) in cases {
            assert_eq!(
                ct.diesel_type(Backend::Sqlite),
                expected,
                "sqlite diesel_type for {ct:?}"
            );
        }
    }

    #[test]
    fn sql_type_pg_mapping() {
        let cases = [
            (ColumnType::Text, "TEXT"),
            (ColumnType::Int32, "INTEGER"),
            (ColumnType::Int64, "BIGINT"),
            (ColumnType::Bool, "BOOLEAN"),
            (ColumnType::Float32, "REAL"),
            (ColumnType::Float64, "DOUBLE PRECISION"),
            (ColumnType::Uuid, "UUID"),
            (ColumnType::Timestamp, "TIMESTAMP"),
            (ColumnType::TimestampTz, "TIMESTAMPTZ"),
            (ColumnType::Bytes, "BYTEA"),
            (ColumnType::Attachment, "JSONB"),
            (
                ColumnType::Enum {
                    variants: vec!["a".into()],
                },
                "TEXT",
            ),
        ];
        for (ct, expected) in cases {
            assert_eq!(
                ct.sql_type(Backend::Postgres),
                expected,
                "pg sql_type for {ct:?}"
            );
        }
    }

    #[test]
    fn sql_type_sqlite_mapping() {
        let cases = [
            (ColumnType::Text, "TEXT"),
            (ColumnType::Int32, "INTEGER"),
            (ColumnType::Int64, "INTEGER"),
            (ColumnType::Bool, "INTEGER"),
            (ColumnType::Float32, "REAL"),
            (ColumnType::Float64, "REAL"),
            (ColumnType::Uuid, "TEXT"),
            (ColumnType::Timestamp, "TEXT"),
            (ColumnType::TimestampTz, "TEXT"),
            (ColumnType::Bytes, "BLOB"),
            (ColumnType::Attachment, "TEXT"),
            (
                ColumnType::Enum {
                    variants: vec!["a".into()],
                },
                "TEXT",
            ),
        ];
        for (ct, expected) in cases {
            assert_eq!(
                ct.sql_type(Backend::Sqlite),
                expected,
                "sqlite sql_type for {ct:?}"
            );
        }
    }

    #[test]
    fn decimal_renders_precision_and_scale_on_pg_only() {
        let d = ColumnType::Decimal {
            precision: 12,
            scale: 2,
        };
        assert_eq!(d.sql_type(Backend::Postgres), "NUMERIC(12,2)");
        assert_eq!(d.sql_type(Backend::Sqlite), "TEXT");
        // A non-default shape carries through too.
        let d = ColumnType::Decimal {
            precision: 8,
            scale: 4,
        };
        assert_eq!(d.sql_type(Backend::Postgres), "NUMERIC(8,4)");
    }

    #[test]
    fn sqlite_diesel_conversion_flags() {
        for ct in all_column_types() {
            let expected = !matches!(
                ct,
                ColumnType::Uuid
                    | ColumnType::Attachment
                    | ColumnType::Decimal { .. }
                    | ColumnType::TimestampTz
                    | ColumnType::Enum { .. }
            );
            assert_eq!(
                ct.sqlite_has_diesel_conversion(),
                expected,
                "sqlite conversion flag for {ct:?}"
            );
        }
    }

    #[test]
    fn from_pg_udt_happy_cases() {
        assert_eq!(ColumnType::from_pg_udt("text"), Some(ColumnType::Text));
        assert_eq!(ColumnType::from_pg_udt("varchar"), Some(ColumnType::Text));
        assert_eq!(ColumnType::from_pg_udt("bpchar"), Some(ColumnType::Text));
        assert_eq!(ColumnType::from_pg_udt("int4"), Some(ColumnType::Int32));
        assert_eq!(ColumnType::from_pg_udt("int8"), Some(ColumnType::Int64));
        assert_eq!(ColumnType::from_pg_udt("bool"), Some(ColumnType::Bool));
        assert_eq!(ColumnType::from_pg_udt("float4"), Some(ColumnType::Float32));
        assert_eq!(ColumnType::from_pg_udt("float8"), Some(ColumnType::Float64));
        assert_eq!(ColumnType::from_pg_udt("uuid"), Some(ColumnType::Uuid));
        assert_eq!(
            ColumnType::from_pg_udt("timestamp"),
            Some(ColumnType::Timestamp)
        );
        assert_eq!(
            ColumnType::from_pg_udt("timestamptz"),
            Some(ColumnType::TimestampTz)
        );
        assert_eq!(ColumnType::from_pg_udt("bytea"), Some(ColumnType::Bytes));
    }

    #[test]
    fn from_pg_udt_ambiguous_types_are_none() {
        assert_eq!(ColumnType::from_pg_udt("jsonb"), None);
        assert_eq!(ColumnType::from_pg_udt("numeric"), None);
        assert_eq!(ColumnType::from_pg_udt("wat"), None);
    }

    #[test]
    fn from_rust_type_happy_and_path_tolerant() {
        // Bare tokens.
        assert_eq!(ColumnType::from_rust_type("String"), Some(ColumnType::Text));
        assert_eq!(ColumnType::from_rust_type("i32"), Some(ColumnType::Int32));
        assert_eq!(ColumnType::from_rust_type("i64"), Some(ColumnType::Int64));
        assert_eq!(ColumnType::from_rust_type("bool"), Some(ColumnType::Bool));
        assert_eq!(ColumnType::from_rust_type("f32"), Some(ColumnType::Float32));
        assert_eq!(ColumnType::from_rust_type("f64"), Some(ColumnType::Float64));
        assert_eq!(
            ColumnType::from_rust_type("Vec<u8>"),
            Some(ColumnType::Bytes)
        );
        // Path-tolerant tokens (as they appear in a real `#[model]` struct).
        assert_eq!(
            ColumnType::from_rust_type("uuid::Uuid"),
            Some(ColumnType::Uuid)
        );
        assert_eq!(
            ColumnType::from_rust_type("chrono::NaiveDateTime"),
            Some(ColumnType::Timestamp)
        );
        assert_eq!(
            ColumnType::from_rust_type("chrono::DateTime<chrono::Utc>"),
            Some(ColumnType::TimestampTz)
        );
        assert_eq!(
            ColumnType::from_rust_type("autumn_web::storage::Blob"),
            Some(ColumnType::Attachment)
        );
        assert_eq!(
            ColumnType::from_rust_type("rust_decimal::Decimal"),
            Some(ColumnType::Decimal {
                precision: 12,
                scale: 2
            })
        );
        // Whitespace tolerance around the generic.
        assert_eq!(
            ColumnType::from_rust_type("chrono::DateTime < chrono::Utc >"),
            Some(ColumnType::TimestampTz)
        );
    }

    #[test]
    fn from_rust_type_unknown_and_enum_are_none() {
        assert_eq!(ColumnType::from_rust_type("Status"), None);
        assert_eq!(ColumnType::from_rust_type("SomeOtherType"), None);
    }

    #[test]
    fn id_kind_rust_type() {
        assert_eq!(IdKind::BigSerial.rust_type(), "i64");
        assert_eq!(IdKind::Uuid.rust_type(), "uuid::Uuid");
    }

    #[test]
    fn id_kind_pk_sql_both_kinds_and_backends() {
        assert_eq!(
            IdKind::BigSerial.pk_sql(Backend::Postgres),
            "BIGSERIAL PRIMARY KEY"
        );
        assert_eq!(
            IdKind::BigSerial.pk_sql(Backend::Sqlite),
            "INTEGER PRIMARY KEY AUTOINCREMENT"
        );
        assert_eq!(
            IdKind::Uuid.pk_sql(Backend::Postgres),
            "UUID PRIMARY KEY DEFAULT gen_random_uuid()"
        );
        assert_eq!(IdKind::Uuid.pk_sql(Backend::Sqlite), "TEXT PRIMARY KEY");
    }

    #[test]
    fn id_kind_diesel_type_both_backends() {
        assert_eq!(IdKind::BigSerial.diesel_type(Backend::Postgres), "Int8");
        assert_eq!(IdKind::BigSerial.diesel_type(Backend::Sqlite), "Int8");
        assert_eq!(IdKind::Uuid.diesel_type(Backend::Postgres), "Uuid");
        assert_eq!(IdKind::Uuid.diesel_type(Backend::Sqlite), "Text");
    }

    #[test]
    fn schema_json_round_trips() {
        let mut table = Table::new("posts", Backend::Postgres);
        let mut id = Column::new("id", ColumnType::Int64);
        id.primary_key = true;
        table.primary_key.push("id".to_owned());
        let mut author = Column::new("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "id"));
        let mut created = Column::new("created_at", ColumnType::TimestampTz);
        created.default = Some(ColumnDefault::Now);
        table.columns.push(id);
        table.columns.push(author);
        table.columns.push(created);
        table.columns.push(Column::new("body", ColumnType::Text));
        table.columns.push(Column::new(
            "status",
            ColumnType::Enum {
                variants: vec!["draft".into(), "live".into()],
            },
        ));
        table.indexes.push(Index {
            name: "posts_author_idx".to_owned(),
            columns: vec!["author_id".to_owned()],
            unique: false,
        });
        table.checks.push(CheckConstraint {
            name: Some("posts_status_check".to_owned()),
            expression: "status IN ('draft','live')".to_owned(),
        });

        let schema = Schema {
            backend: Backend::Postgres,
            tables: vec![table],
        };

        let json = serde_json::to_string(&schema).expect("serialize");
        let back: Schema = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(schema, back);
    }

    #[test]
    fn constructors_apply_expected_defaults() {
        let col = Column::new("name", ColumnType::Text);
        assert!(!col.nullable && !col.primary_key && !col.unique);
        assert!(col.default.is_none() && col.references.is_none());

        let table = Table::new("widgets", Backend::Sqlite);
        assert!(table.managed);
        assert_eq!(table.backend, Backend::Sqlite);
        assert!(table.columns.is_empty());

        let schema = Schema::new(Backend::Sqlite);
        assert!(schema.tables.is_empty());
    }
}
