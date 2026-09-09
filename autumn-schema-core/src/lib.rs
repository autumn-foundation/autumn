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

/// A `SQLite` **type-affinity class** — the five storage-affinity buckets `SQLite`
/// assigns a column from its declared type (`SQLite` docs §3.1).
///
/// Because `SQLite` stores only the declared-type string, distinct IR
/// [`ColumnType`]s that the emitter renders to the same declared type share an
/// affinity class and are indistinguishable after a pull; the diff compares by this
/// class on the `SQLite` backend (see [`ColumnType::sqlite_affinity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteAffinity {
    /// Declared type contains `INT` (e.g. `INTEGER`, `BIGINT`).
    Integer,
    /// Declared type contains `CHAR`, `CLOB`, or `TEXT`.
    Text,
    /// Declared type contains `BLOB`, or is empty.
    Blob,
    /// Declared type contains `REAL`, `FLOA`, or `DOUB`.
    Real,
    /// Anything else (e.g. `NUMERIC`, `DECIMAL`) — the ambiguous catch-all.
    Numeric,
}

/// Compute the `SQLite` type-affinity class of a declared-type string.
///
/// Follows `SQLite`'s canonical affinity-determination algorithm (docs §3.1),
/// matched in order (case-insensitively): contains `INT` →
/// [`Integer`](SqliteAffinity::Integer); contains `CHAR`/`CLOB`/`TEXT` →
/// [`Text`](SqliteAffinity::Text); contains `BLOB` or empty →
/// [`Blob`](SqliteAffinity::Blob); contains `REAL`/`FLOA`/`DOUB` →
/// [`Real`](SqliteAffinity::Real); otherwise [`Numeric`](SqliteAffinity::Numeric).
#[must_use]
pub fn sqlite_affinity(declared_type: &str) -> SqliteAffinity {
    let t = declared_type.to_ascii_uppercase();
    if t.contains("INT") {
        SqliteAffinity::Integer
    } else if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        SqliteAffinity::Text
    } else if t.is_empty() || t.contains("BLOB") {
        SqliteAffinity::Blob
    } else if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") {
        SqliteAffinity::Real
    } else {
        SqliteAffinity::Numeric
    }
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
    /// Arbitrary structured data — PG `JSONB` / `SQLite` `TEXT` (issue #1341).
    /// Unlike [`Attachment`](Self::Attachment), maps directly to bare
    /// `serde_json::Value` (no wrapper struct): diesel itself already
    /// implements `FromSql`/`ToSql<Jsonb, Pg>` **and** `<Json, Sqlite>` for
    /// `serde_json::Value`, so no `autumn-web` conversion code is needed on
    /// either backend. On `SQLite` the column is `TEXT` via diesel's `Json`
    /// sql-type specifically — not diesel's `Jsonb` sql-type on `SQLite`,
    /// which uses a proprietary binary encoding rather than plain-text JSON.
    Json,
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
    /// A Postgres type outside Autumn's mapped surface, **preserved verbatim** by
    /// database introspection (`autumn schema pull`) so an unmappable column is
    /// never silently dropped from the snapshot IR.
    ///
    /// This variant is **introspection/snapshot-only**: it records the raw
    /// Postgres type name (e.g. `inet`, `citext`, `macaddr`) exactly as read from
    /// the catalog. [`sql_type`](Self::sql_type) emits `pg_type` verbatim (on both
    /// backends), so a pulled snapshot round-trips the raw type. It is deliberately
    /// **not** intended for Rust codegen in this slice — [`rust_type`](Self::rust_type)
    /// and [`diesel_type`](Self::diesel_type) return a safe `String`/`Text`
    /// sentinel rather than a faithful mapping (an opaque type has no known Rust
    /// representation), and there is no forward DSL / `#[model]` source that
    /// produces it.
    Opaque {
        /// The raw Postgres type name (`udt_name`), preserved exactly.
        pg_type: String,
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
            // `Opaque` is introspection-only and has no known Rust representation,
            // so it shares the `String` storage-fallback sentinel (it is not
            // intended for Rust codegen in this slice — never panics).
            Self::Text | Self::Enum { .. } | Self::Opaque { .. } => "String",
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
            Self::Json => "serde_json::Value",
            Self::Decimal { .. } => "rust_decimal::Decimal",
        }
        .to_owned()
    }

    /// [`rust_type`](Self::rust_type) for `backend` (issue #1924).
    ///
    /// Mirrors `dsl::FieldKind::rust_type_for`. Postgres is byte-for-byte
    /// [`rust_type`](Self::rust_type); on `SQLite`, [`Uuid`](Self::Uuid) and
    /// [`Decimal`](Self::Decimal) render `autumn-web`'s `TEXT`-backed newtypes,
    /// because `uuid::Uuid` and `rust_decimal::Decimal` are foreign to
    /// `autumn-web` and diesel blanket-implements `AsExpression` for every
    /// `Expression`, leaving no crate that could give them a `SQLite`
    /// conversion.
    #[must_use]
    pub fn rust_type_for(&self, backend: Backend) -> String {
        match backend {
            Backend::Postgres => self.rust_type(),
            Backend::Sqlite => match self {
                Self::Uuid => "autumn_web::db::sqlite_types::SqliteUuid".to_owned(),
                Self::Decimal { .. } => "autumn_web::db::sqlite_types::SqliteDecimal".to_owned(),
                _ => self.rust_type(),
            },
        }
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
                // `Opaque` shares the `Text` diesel sentinel (introspection-only,
                // not intended for diesel codegen).
                Self::Text | Self::Enum { .. } | Self::Opaque { .. } => "Text",
                Self::Int32 => "Int4",
                Self::Int64 => "Int8",
                Self::Bool => "Bool",
                Self::Float32 => "Float4",
                Self::Float64 => "Float8",
                Self::Uuid => "Uuid",
                Self::Timestamp => "Timestamp",
                Self::TimestampTz => "Timestamptz",
                Self::Bytes => "Bytea",
                Self::Attachment | Self::Json => "Jsonb",
                Self::Decimal { .. } => "Numeric",
            },
            Backend::Sqlite => match self {
                // `Opaque` shares the `Text` diesel sentinel (introspection-only).
                Self::Text
                | Self::Uuid
                | Self::Attachment
                | Self::Decimal { .. }
                | Self::Enum { .. }
                | Self::Opaque { .. } => "Text",
                Self::Int32 => "Int4",
                Self::Int64 => "Int8",
                Self::Bool => "Bool",
                Self::Float32 => "Float4",
                Self::Float64 => "Float8",
                // `NaiveDateTime` -> core, ungated `Timestamp` (compiles).
                Self::Timestamp => "Timestamp",
                // `DateTime<Utc>` -> diesel's SQLite `TimestamptzSqlite` (issue
                // #1924); its `sqlite`+`chrono` conversion resolves through the
                // app's `autumn-web` sqlite feature.
                Self::TimestampTz => "TimestamptzSqlite",
                Self::Bytes => "Binary",
                // Diesel's own `Json` sql-type — not `Text` (no built-in
                // `serde_json::Value` conversion) and not `Jsonb` (SQLite's
                // proprietary binary encoding). See the `Json` variant's doc.
                Self::Json => "Json",
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
                Self::Attachment | Self::Json => "JSONB".to_owned(),
                Self::Decimal { precision, scale } => format!("NUMERIC({precision},{scale})"),
                // Preserved verbatim: the raw Postgres type name round-trips.
                Self::Opaque { pg_type } => pg_type.clone(),
            },
            Backend::Sqlite => match self {
                Self::Text
                | Self::Uuid
                | Self::Timestamp
                | Self::TimestampTz
                | Self::Attachment
                | Self::Json
                | Self::Decimal { .. }
                | Self::Enum { .. } => "TEXT".to_owned(),
                Self::Int32 | Self::Int64 | Self::Bool => "INTEGER".to_owned(),
                Self::Float32 | Self::Float64 => "REAL".to_owned(),
                Self::Bytes => "BLOB".to_owned(),
                // Preserved verbatim (an `Opaque` is only ever produced by
                // Postgres introspection, but the arm is emitted on both backends
                // for exhaustiveness and never loses the raw type name).
                Self::Opaque { pg_type } => pg_type.clone(),
            },
        }
    }

    /// The `SQLite` **type-affinity class** of this column type, derived from the
    /// declared type the emitter renders on `SQLite` ([`sql_type`](Self::sql_type)
    /// with [`Backend::Sqlite`]) via [`sqlite_affinity`].
    ///
    /// Because `SQLite` stores only the declared-type STRING (and applies affinity
    /// rules), the emitter collapses several distinct IR types onto the same
    /// declared type — `Int32`/`Int64`/`Bool` → `INTEGER`, `Float32`/`Float64` →
    /// `REAL`, `Text`/`Uuid`/`Timestamp`/`TimestampTz`/`Decimal`/`Attachment`/`Enum`
    /// → `TEXT`, `Bytes` → `BLOB` — so a pulled `SQLite` snapshot cannot recover the
    /// original variant. The diff uses THIS class (not exact [`ColumnType`]
    /// equality) on the `SQLite` backend so a matching model↔pull round-trips clean
    /// while a genuine class change (e.g. `INTEGER`→`TEXT`) still drifts. Deriving it
    /// from the rendered declared type keeps it automatically consistent with
    /// whatever the emitter produces.
    #[must_use]
    pub fn sqlite_affinity(&self) -> SqliteAffinity {
        sqlite_affinity(&self.sql_type(Backend::Sqlite))
    }

    /// Whether this type's rendered Rust model type has a working diesel
    /// `FromSql`/`ToSql` on diesel's `SQLite` backend in a generated app's feature
    /// set (diesel `sqlite` + `chrono`, without `uuid`/`numeric`).
    ///
    /// Mirrors `dsl::FieldKind::sqlite_has_diesel_conversion`: `true` for every
    /// mapped type as of issue #1924 — [`Timestamp`](Self::Timestamp) via the
    /// core, ungated diesel `Timestamp` sql-type, [`TimestampTz`](Self::TimestampTz)
    /// via diesel's `SQLite` `TimestamptzSqlite`, [`Attachment`](Self::Attachment)
    /// via `autumn-web`'s local `Blob` `Text`/`Sqlite` conversion,
    /// [`Uuid`](Self::Uuid) and [`Decimal`](Self::Decimal) via `autumn-web`'s
    /// `TEXT`-backed newtypes (see [`ColumnType::rust_type_for`]),
    /// [`Enum`](Self::Enum) via the app-local `Text`/`Sqlite` impls the model
    /// generator emits, and [`Json`](Self::Json) via diesel's own
    /// `FromSql`/`ToSql<Json, Sqlite> for serde_json::Value` (issue #1341).
    ///
    /// Only [`Opaque`](Self::Opaque) is `false`: it is introspection-only and
    /// carries a raw Postgres type name with no known diesel conversion.
    #[must_use]
    pub const fn sqlite_has_diesel_conversion(&self) -> bool {
        !matches!(self, Self::Opaque { .. })
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
    /// - **`jsonb` → `None`**: although [`Attachment`](Self::Attachment) (and,
    ///   since issue #1341, [`Json`](Self::Json) too) forward-maps to `JSONB`,
    ///   the inverse is ambiguous — a brownfield `jsonb` column could be
    ///   arbitrary application JSON, an Autumn `Blob`, or a `Json` field, and
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

    /// The centralized database-introspection inverse used by `autumn schema
    /// pull`: resolve a Postgres `udt_name` plus its catalog `numeric_precision`
    /// / `numeric_scale` (only meaningful for `numeric`/`decimal`) to a
    /// [`ColumnType`] that is **always** produced — an unmappable type is
    /// preserved as [`Opaque`](Self::Opaque) rather than dropped.
    ///
    /// Resolution order:
    ///
    /// 0. A **length-limited character type** — `varchar` (`character varying`) or
    ///    `bpchar` (`character`/`char`) **with** a `character_maximum_length` — is
    ///    preserved as [`Opaque`](Self::Opaque) carrying `varchar(n)` / `char(n)`
    ///    so the DB-enforced length limit survives recreation rather than being
    ///    flattened to an unconstrained `TEXT`. An unbounded `varchar` (length
    ///    `None`, or `text`, which never carries a length) falls through to `Text`.
    /// 1. [`from_pg_udt`](Self::from_pg_udt) — the shared mapped surface
    ///    (`text`/`int4`/`int8`/`bool`/`float4`/`float8`/`uuid`/`timestamp`/
    ///    `timestamptz`/`bytea`).
    /// 2. `numeric` / `decimal` **with** an in-`u8`-range precision (and scale)
    ///    available → [`Decimal`](Self::Decimal). A bare `numeric` with no
    ///    precision carries no `(p, s)` to reconstruct, so it falls through to
    ///    `Opaque` rather than guessing; and an out-of-range precision/scale (a
    ///    valid Postgres `NUMERIC(1000, 0)` or a negative scale that does not fit
    ///    the `u8` fields) is likewise preserved as [`Opaque`](Self::Opaque) with a
    ///    faithfully-reconstructed `numeric(...)` type string rather than silently
    ///    clamped to `NUMERIC(255, 0)`.
    /// 3. `jsonb` → [`Attachment`](Self::Attachment). Autumn only ever emits
    ///    `jsonb` for an [`Attachment`](Self::Attachment) column, so a pulled
    ///    `jsonb` column is round-tripped as an attachment. (This is the
    ///    deliberate introspection counterpart to
    ///    [`from_pg_udt`](Self::from_pg_udt) returning `None` for `jsonb` — that
    ///    inverse is used by the model-scaffolding `db pull`, which must not
    ///    guess `Blob` for arbitrary brownfield JSON; the declarative-schema
    ///    `schema pull` instead prioritises a clean round-trip of Autumn-owned
    ///    tables.)
    /// 4. Anything else → [`Opaque`](Self::Opaque) carrying the raw `udt`, so the
    ///    column is never silently lost.
    #[must_use]
    pub fn from_pg_introspection(
        udt: &str,
        numeric_precision: Option<i32>,
        numeric_scale: Option<i32>,
        character_maximum_length: Option<i32>,
    ) -> Self {
        // Fail-closed floor for length-limited character types. `VARCHAR(32)` and
        // `CHAR(2)` report `udt_name` `varchar` / `bpchar`, which `from_pg_udt`
        // otherwise collapses to `Text`, silently dropping the DB-enforced length
        // limit (recreation would emit an unconstrained `TEXT`). When a length
        // modifier is present, preserve the column verbatim as `Opaque` carrying a
        // valid Postgres type string (`Opaque`'s `sql_type` emits `pg_type`
        // unchanged), checked BEFORE the `from_pg_udt` mapped return. An unbounded
        // `varchar` (or `text`, which never carries a length) has `None` here and
        // maps to `Text` exactly as before.
        if let Some(length) = character_maximum_length {
            match udt {
                "varchar" => {
                    return Self::Opaque {
                        pg_type: format!("varchar({length})"),
                    };
                }
                "bpchar" => {
                    return Self::Opaque {
                        pg_type: format!("char({length})"),
                    };
                }
                _ => {}
            }
        }
        if let Some(mapped) = Self::from_pg_udt(udt) {
            return mapped;
        }
        if matches!(udt, "numeric" | "decimal")
            && let Some(precision) = numeric_precision
        {
            // Map to `Decimal` ONLY when both precision and scale fit the `u8`
            // fields — never silently clamp an out-of-range `NUMERIC(1000, 0)` (or
            // a negative/oversized scale) down to `NUMERIC(255, 0)`. An out-of-
            // range value is instead preserved as `Opaque` with a faithfully-
            // reconstructed type string, so the down migration re-adds the true
            // type verbatim (`Opaque`'s `sql_type` emits `pg_type` unchanged).
            let precision_u8 = u8::try_from(precision).ok().filter(|&p| p >= 1);
            let scale_u8 = numeric_scale.map_or(Some(0), |scale| u8::try_from(scale).ok());
            if let (Some(precision), Some(scale)) = (precision_u8, scale_u8) {
                return Self::Decimal { precision, scale };
            }
            return Self::Opaque {
                pg_type: match numeric_scale {
                    None | Some(0) => format!("numeric({precision})"),
                    Some(scale) => format!("numeric({precision},{scale})"),
                },
            };
        }
        // Deliberately unchanged by issue #1341: a `json`/`jsonb` field ALSO
        // forward-maps to `jsonb` now, deepening rather than resolving this
        // ambiguity (see `from_pg_udt`'s doc). `schema pull` prioritises a
        // clean round-trip of the common case (an Autumn-managed table's
        // attachment column) over guessing at a brownfield column's intent;
        // resolving the ambiguity is out of this issue's forward-generation
        // scope.
        if udt == "jsonb" {
            return Self::Attachment;
        }
        Self::Opaque {
            pg_type: udt.to_owned(),
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
    ///
    /// [`Json`](Self::Json) is the one exception to the leaf-matching rule
    /// above: `Value` is common enough as a bare identifier (unlike the
    /// domain-specific `Blob`/`Decimal`/`Uuid`/`NaiveDateTime`) that an
    /// unrelated hand-written type sharing the name would otherwise be
    /// misclassified as JSON — and this function only ever sees the type
    /// token as written in the struct field, never the file's `use`
    /// declarations, so even a *bare* `Value` can't be safely resolved to a
    /// crate without that import context. Only the exact `serde_json::Value`
    /// path resolves to `Json` (an optional leading `::` — an absolute path —
    /// is tolerated); a bare `Value` (however it was imported) or any other
    /// qualified path (`domain::Value`, `my_crate::sub::Value`) resolves to
    /// `None`. The DSL/scaffold generator is unaffected — it always emits
    /// the fully-qualified `serde_json::Value` in generated model structs
    /// (see [`Self::rust_type`]), never a bare `Value`.
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
        // See the doc comment above: `Value` is too generic a bare name to
        // safely leaf-match through an arbitrary path — and with no `use`
        // context available here, even a bare `Value` can't be told apart
        // from an unrelated same-named type — so only the fully-qualified
        // `serde_json::Value` path is accepted, checked against the full
        // normalised string rather than falling through to the
        // `rsplit("::")` leaf split below. An optional leading `::` (an
        // absolute path, `::serde_json::Value`, sometimes written to avoid
        // shadowing by a local module of the same name) is stripped first —
        // this doesn't reopen the ambiguity the exact match exists for,
        // since `::domain::Value` still normalises to `domain::Value` and
        // correctly falls through to `None` below.
        let unprefixed = normalized.strip_prefix("::").unwrap_or(normalized.as_str());
        if unprefixed == "serde_json::Value" {
            return Some(Self::Json);
        }

        // Take the final `::`-separated segment (path-tolerant); a token with no
        // path (`String`, `Vec<u8>`) is returned unchanged.
        let leaf = normalized
            .rsplit("::")
            .next()
            .unwrap_or(normalized.as_str());
        match leaf {
            // `Translated` is a `#[translatable]` per-locale container (issue
            // #1384): its storage is a plain `TEXT` column holding a JSON
            // object, so the declarative lane manages it exactly like any other
            // text column. Without it here the parser skips the column and the
            // diff refuses to emit `CREATE TABLE` for the whole model.
            "String" | "Translated" => Some(Self::Text),
            "i32" => Some(Self::Int32),
            "i64" => Some(Self::Int64),
            "bool" => Some(Self::Bool),
            "f32" => Some(Self::Float32),
            "f64" => Some(Self::Float64),
            // `SqliteUuid`/`SqliteDecimal` are the `TEXT`-backed newtypes a
            // SQLite app's model renders instead of the foreign `uuid::Uuid` /
            // `rust_decimal::Decimal` (issue #1924). They are the same column,
            // so they must resolve to the same `ColumnType` — otherwise the
            // declarative lane skips the column and every snapshot, diff and
            // generated `CREATE TABLE` silently omits it.
            "Uuid" | "SqliteUuid" => Some(Self::Uuid),
            "NaiveDateTime" => Some(Self::Timestamp),
            "Vec<u8>" => Some(Self::Bytes),
            "Blob" => Some(Self::Attachment),
            // The declared precision and scale do not survive into the Rust
            // type on either backend, so both resolve to the same default the
            // DSL's bare `decimal` token uses.
            "Decimal" | "SqliteDecimal" => Some(Self::Decimal {
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
    /// (`SQLite`) — a sequential auto-increment 64-bit integer.
    BigSerial,
    /// `SERIAL PRIMARY KEY` (PG) / `INTEGER PRIMARY KEY AUTOINCREMENT`
    /// (`SQLite`) — a sequential auto-increment 32-bit integer. The model DSL
    /// never produces this (its `#[id]` is always `BigSerial` or `Uuid`); it
    /// exists so a **brownfield** `SERIAL PRIMARY KEY` (int4) column introspected
    /// by `schema pull` recreates as `SERIAL` (auto-increment) rather than a plain
    /// `INTEGER PRIMARY KEY` that silently loses the sequence.
    Serial,
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
            Self::Serial => "i32",
            Self::Uuid => "uuid::Uuid",
        }
    }

    /// The diesel `table!` schema type token for `backend`. Mirrors
    /// `dsl::IdType::schema_type_for` (`SQLite` remaps `Uuid` → `Text`).
    #[must_use]
    pub const fn diesel_type(self, backend: Backend) -> &'static str {
        match (self, backend) {
            (Self::BigSerial, _) => "Int8",
            (Self::Serial, _) => "Int4",
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
            (Self::Serial, Backend::Postgres) => "SERIAL PRIMARY KEY",
            (Self::BigSerial | Self::Serial, Backend::Sqlite) => {
                "INTEGER PRIMARY KEY AUTOINCREMENT"
            }
            (Self::Uuid, Backend::Postgres) => "UUID PRIMARY KEY DEFAULT gen_random_uuid()",
            (Self::Uuid, Backend::Sqlite) => "TEXT PRIMARY KEY",
        }
    }
}

/// Distinguishes an owned-sequence auto-increment integer primary key from a
/// plain, manually-assigned integer primary key of the same storage width.
///
/// Without this marker a `BIGINT PRIMARY KEY` (a plain, manually-assigned id) and
/// a `BIGSERIAL` (an owned-sequence auto-increment id) both land in the IR as
/// `Column { ty: Int64, primary_key: true, default: None }` — indistinguishable —
/// so `schema pull` / `schema diff` could not tell a brownfield plain-int PK from
/// a generated serial id. It is populated **symmetrically** by the model parser
/// (for a convention `BigSerial` id) and by database introspection (only when the
/// pulled column genuinely owns its sequence), so a model↔database diff of matching
/// schemas stays empty while a genuine plain-`BIGINT PK` vs `BIGSERIAL` mismatch
/// surfaces as drift. See [`Column::serial`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SerialKind {
    /// `SERIAL` (int4) — an owned-sequence auto-increment 32-bit id. Only ever
    /// produced by brownfield introspection (the model `#[id]` is always
    /// `BigSerial` or `Uuid`).
    Serial,
    /// `BIGSERIAL` (int8) — an owned-sequence auto-increment 64-bit id, the Autumn
    /// model `#[id]` convention. On `SQLite` this is an `INTEGER PRIMARY KEY
    /// AUTOINCREMENT` column.
    BigSerial,
    /// A **plain**, manually-assigned single-column integer primary key with **no**
    /// owned sequence — a genuine `INTEGER`/`BIGINT PRIMARY KEY` (Postgres) or a
    /// non-`AUTOINCREMENT` `INTEGER PRIMARY KEY` (`SQLite`). Emitted **only** by
    /// database introspection.
    ///
    /// It is deliberately distinct from `None`: `None` means the marker is *unknown*
    /// — a snapshot written before this field existed (serde default) — whereas
    /// `Plain` is an **explicit** "introspected, and it genuinely owns no sequence"
    /// signal. The diff treats `None` on either side as compatible (never drift), so
    /// a legacy snapshot keeps round-tripping clean; a `Plain` vs `BigSerial`
    /// mismatch (both explicit) still surfaces as real drift. The model parser never
    /// emits `Plain` (its `#[id]` is always `BigSerial` or `Uuid`).
    Plain,
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
    /// The auto-increment id-generation strategy of an owned-sequence integer
    /// primary key, distinguishing a generated `SERIAL`/`BIGSERIAL` id from a plain
    /// manually-assigned `INTEGER`/`BIGINT` primary key of the same storage width
    /// (see [`SerialKind`]). `None` for every non-serial column — including a
    /// plain-int PK with no owned sequence. Populated symmetrically by the model
    /// parser and by database introspection so a matching model↔database diff stays
    /// empty. Defaults to `None` and is skipped when serializing, so snapshots
    /// written before this field existed stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<SerialKind>,
    /// A preserved `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY` clause — the
    /// verbatim `identity_generation` (`"ALWAYS"` / `"BY DEFAULT"`) of a Postgres
    /// identity column — so an identity column round-trips through `schema pull`
    /// instead of flattening to a plain integer column. `None` for every
    /// non-identity column. Defaults to `None` and is skipped when serializing, so
    /// pre-existing snapshots stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
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
            serial: None,
            identity: None,
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
    /// The verbatim `CREATE [UNIQUE] INDEX …` statement for an index that
    /// cannot be represented by plain columns alone — an expression index
    /// (e.g. `lower(email)`) or a partial index (`WHERE …`). When `Some`, the
    /// emitter renders this definition verbatim instead of building
    /// `CREATE INDEX … (columns)`, and the diff compares indexes by this text
    /// rather than by `columns`. `None` for ordinary column indexes, which keeps
    /// their serialized JSON byte-identical to snapshots written before this
    /// field existed (see the `#[serde(default, skip_serializing_if)]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// Whether the index carries a `WHERE` predicate (a **partial** index). A
    /// partial unique index only enforces uniqueness for the rows matching its
    /// predicate, so it must NOT be treated as satisfying a model `#[unique]`
    /// (which demands table-wide uniqueness). Introspection sets this from
    /// `pg_index.indpred IS NOT NULL`; the model parser never emits a partial
    /// index. Defaults to `false` and is skipped when serializing so ordinary
    /// indexes stay byte-identical to pre-existing snapshots.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_partial: bool,
    /// The index's real **key** columns, in key order — the columns whose values
    /// uniqueness is enforced over — EXCLUDING any non-key `INCLUDE` columns, and
    /// **empty** when any key position is an expression (e.g. `lower(email)`). It
    /// is distinct from [`columns`](Self::columns): for a `definition`-carrying
    /// index `columns` is the full dependency set (key + `INCLUDE` + expression- +
    /// predicate-referenced columns, used for cascade detection), whereas
    /// `key_columns` is only what the index's uniqueness is keyed on. Populated by
    /// introspection for `definition`-carrying indexes; for a plain simple index it
    /// is left empty (its key columns are exactly `columns`, so recording them again
    /// would be redundant JSON noise). An empty `key_columns` on an
    /// expression/`definition` index deliberately signals "no plain key column set"
    /// so such an index cannot satisfy a model `#[unique]`. Defaults to empty and is
    /// skipped when serializing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_columns: Vec<String>,
}

/// `serde` `skip_serializing_if` helper: whether a bool is `false` (so a
/// default-`false` flag is omitted from the serialized form).
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
}

impl Index {
    /// Construct an ordinary column index (no raw `definition`), the common case
    /// for both the model parser and simple introspected indexes.
    #[must_use]
    pub fn new(name: impl Into<String>, columns: Vec<String>, unique: bool) -> Self {
        Self {
            name: name.into(),
            columns,
            unique,
            definition: None,
            is_partial: false,
            key_columns: Vec::new(),
        }
    }
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
            ColumnType::Json,
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
                // `Opaque` shares the `String` sentinel; `all_column_types()`
                // never yields it, but the match must stay exhaustive.
                ColumnType::Text | ColumnType::Enum { .. } | ColumnType::Opaque { .. } => "String",
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
                ColumnType::Json => "serde_json::Value",
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
            (ColumnType::Json, "Jsonb"),
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
            (ColumnType::TimestampTz, "TimestamptzSqlite"),
            (ColumnType::Bytes, "Binary"),
            (ColumnType::Attachment, "Text"),
            // Diesel's own `Json` sql-type — distinct from both `Text` (no
            // built-in `serde_json::Value` conversion) and `Jsonb` (SQLite's
            // proprietary binary encoding). See the `Json` variant's doc.
            (ColumnType::Json, "Json"),
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
            (ColumnType::Json, "JSONB"),
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
            (ColumnType::Json, "TEXT"),
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
    fn sqlite_affinity_of_declared_type_follows_the_canonical_algorithm() {
        for (decl, expected) in [
            ("INTEGER", SqliteAffinity::Integer),
            ("BIGINT", SqliteAffinity::Integer),
            ("TINYINT", SqliteAffinity::Integer),
            ("VARCHAR(255)", SqliteAffinity::Text),
            ("CLOB", SqliteAffinity::Text),
            ("TEXT", SqliteAffinity::Text),
            ("", SqliteAffinity::Blob),
            ("BLOB", SqliteAffinity::Blob),
            ("REAL", SqliteAffinity::Real),
            ("FLOAT", SqliteAffinity::Real),
            ("DOUBLE PRECISION", SqliteAffinity::Real),
            ("NUMERIC", SqliteAffinity::Numeric),
            ("DECIMAL(10,2)", SqliteAffinity::Numeric),
            ("BOOLEAN", SqliteAffinity::Numeric),
        ] {
            assert_eq!(sqlite_affinity(decl), expected, "affinity of {decl:?}");
        }
    }

    #[test]
    fn column_type_sqlite_affinity_class_collapses_the_emitter_groups() {
        use SqliteAffinity::{Blob, Integer, Real, Text};
        for (ct, expected) in [
            (ColumnType::Int32, Integer),
            (ColumnType::Int64, Integer),
            (ColumnType::Bool, Integer),
            (ColumnType::Float32, Real),
            (ColumnType::Float64, Real),
            (ColumnType::Text, Text),
            (ColumnType::Uuid, Text),
            (ColumnType::Timestamp, Text),
            (ColumnType::TimestampTz, Text),
            (ColumnType::Bytes, Blob),
        ] {
            assert_eq!(ct.sqlite_affinity(), expected, "affinity class of {ct:?}");
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

    /// Issue #1924 gave `Uuid`, `Decimal` and `Enum` working `SQLite`
    /// conversions, so only the introspection-only `Opaque` lacks one.
    #[test]
    fn sqlite_diesel_conversion_flags() {
        for ct in all_column_types() {
            let expected = !matches!(ct, ColumnType::Opaque { .. });
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
    fn from_pg_introspection_maps_the_shared_surface() {
        // Every type the shared `from_pg_udt` maps resolves identically here.
        for (udt, expected) in [
            ("text", ColumnType::Text),
            ("varchar", ColumnType::Text),
            ("bpchar", ColumnType::Text),
            ("int4", ColumnType::Int32),
            ("int8", ColumnType::Int64),
            ("bool", ColumnType::Bool),
            ("float4", ColumnType::Float32),
            ("float8", ColumnType::Float64),
            ("uuid", ColumnType::Uuid),
            ("timestamp", ColumnType::Timestamp),
            ("timestamptz", ColumnType::TimestampTz),
            ("bytea", ColumnType::Bytes),
        ] {
            assert_eq!(
                ColumnType::from_pg_introspection(udt, None, None, None),
                expected,
                "from_pg_introspection for {udt}"
            );
        }
    }

    #[test]
    fn from_pg_introspection_numeric_with_precision_is_decimal() {
        assert_eq!(
            ColumnType::from_pg_introspection("numeric", Some(12), Some(2), None),
            ColumnType::Decimal {
                precision: 12,
                scale: 2
            }
        );
        // `decimal` alias, and a scale that defaults to 0 when NULL.
        assert_eq!(
            ColumnType::from_pg_introspection("decimal", Some(8), None, None),
            ColumnType::Decimal {
                precision: 8,
                scale: 0
            }
        );
        // A bare `numeric` with no precision cannot be reconstructed → Opaque.
        assert_eq!(
            ColumnType::from_pg_introspection("numeric", None, None, None),
            ColumnType::Opaque {
                pg_type: "numeric".to_owned()
            }
        );
    }

    #[test]
    fn from_pg_introspection_out_of_range_numeric_is_opaque_not_clamped() {
        // A valid Postgres `NUMERIC(1000, 0)` does not fit the `u8` Decimal fields;
        // it must be preserved as `Opaque` carrying the true type string, NOT
        // silently clamped to `NUMERIC(255, 0)`.
        assert_eq!(
            ColumnType::from_pg_introspection("numeric", Some(1000), Some(0), None),
            ColumnType::Opaque {
                pg_type: "numeric(1000)".to_owned()
            }
        );
        // An out-of-range precision with a non-zero scale keeps both.
        assert_eq!(
            ColumnType::from_pg_introspection("numeric", Some(1000), Some(500), None),
            ColumnType::Opaque {
                pg_type: "numeric(1000,500)".to_owned()
            }
        );
        // A negative scale does not fit `u8` → preserved verbatim, not clamped.
        assert_eq!(
            ColumnType::from_pg_introspection("numeric", Some(10), Some(-2), None),
            ColumnType::Opaque {
                pg_type: "numeric(10,-2)".to_owned()
            }
        );
        // In-range values are unaffected (round-trip parity).
        assert_eq!(
            ColumnType::from_pg_introspection("numeric", Some(12), Some(2), None),
            ColumnType::Decimal {
                precision: 12,
                scale: 2
            }
        );
    }

    #[test]
    fn from_pg_introspection_length_limited_char_types_are_opaque() {
        // `VARCHAR(32)` / `CHAR(2)` report `udt_name` `varchar` / `bpchar`, which
        // `from_pg_udt` would otherwise flatten to `Text`, dropping the length
        // limit. With a length modifier present they are preserved verbatim as
        // `Opaque` carrying valid Postgres DDL, checked before the mapped return.
        assert_eq!(
            ColumnType::from_pg_introspection("varchar", None, None, Some(32)),
            ColumnType::Opaque {
                pg_type: "varchar(32)".to_owned()
            }
        );
        assert_eq!(
            ColumnType::from_pg_introspection("bpchar", None, None, Some(2)),
            ColumnType::Opaque {
                pg_type: "char(2)".to_owned()
            }
        );
        // An unbounded `varchar` (length `None`) and `text` (never length-carrying)
        // behave exactly as before → `Text`.
        assert_eq!(
            ColumnType::from_pg_introspection("varchar", None, None, None),
            ColumnType::Text
        );
        assert_eq!(
            ColumnType::from_pg_introspection("text", None, None, None),
            ColumnType::Text
        );
    }

    #[test]
    fn from_pg_introspection_jsonb_is_attachment() {
        assert_eq!(
            ColumnType::from_pg_introspection("jsonb", None, None, None),
            ColumnType::Attachment
        );
    }

    #[test]
    fn from_pg_introspection_unknown_types_are_preserved_opaque() {
        for udt in ["inet", "citext", "macaddr", "tsvector", "point"] {
            assert_eq!(
                ColumnType::from_pg_introspection(udt, None, None, None),
                ColumnType::Opaque {
                    pg_type: udt.to_owned()
                },
                "unmapped {udt} must be preserved as Opaque"
            );
        }
    }

    #[test]
    fn opaque_sql_type_round_trips_the_raw_name_verbatim() {
        let ct = ColumnType::from_pg_introspection("inet", None, None, None);
        // The raw Postgres type name is emitted verbatim on both backends, so a
        // pulled snapshot never loses the type.
        assert_eq!(ct.sql_type(Backend::Postgres), "inet");
        assert_eq!(ct.sql_type(Backend::Sqlite), "inet");
        // The codegen sentinels are safe (introspection-only, never panics).
        assert_eq!(ct.rust_type(), "String");
        assert_eq!(ct.diesel_type(Backend::Postgres), "Text");
        assert!(!ct.sqlite_has_diesel_conversion());
    }

    /// The `SQLite` newtypes must resolve to the same `ColumnType` as the types
    /// they wrap (issue #1924). Without this the declarative lane drops every
    /// `Uuid`/`decimal` column of a `SQLite` app from its snapshots and diffs.
    #[test]
    fn from_rust_type_maps_the_sqlite_newtypes_like_the_types_they_wrap() {
        for (wrapper, plain) in [
            ("autumn_web::db::sqlite_types::SqliteUuid", "uuid::Uuid"),
            (
                "autumn_web::db::sqlite_types::SqliteDecimal",
                "rust_decimal::Decimal",
            ),
        ] {
            assert_eq!(
                ColumnType::from_rust_type(wrapper),
                ColumnType::from_rust_type(plain),
                "`{wrapper}` must resolve like `{plain}`"
            );
            assert!(ColumnType::from_rust_type(wrapper).is_some());
        }
    }

    #[test]
    fn from_rust_type_happy_and_path_tolerant() {
        // Bare tokens.
        assert_eq!(ColumnType::from_rust_type("String"), Some(ColumnType::Text));
        // #1384: a translatable container is TEXT storage, path-tolerant.
        assert_eq!(
            ColumnType::from_rust_type("Translated"),
            Some(ColumnType::Text)
        );
        assert_eq!(
            ColumnType::from_rust_type("autumn_web::i18n::Translated"),
            Some(ColumnType::Text)
        );
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
        assert_eq!(
            ColumnType::from_rust_type("serde_json::Value"),
            Some(ColumnType::Json)
        );
        // Whitespace tolerance around the generic.
        assert_eq!(
            ColumnType::from_rust_type("chrono::DateTime < chrono::Utc >"),
            Some(ColumnType::TimestampTz)
        );
    }

    #[test]
    fn from_rust_type_value_leaf_match_is_scoped_to_serde_json() {
        // Unlike `Blob`/`Decimal`/`Uuid`, `Value` is not leaf-matched through an
        // arbitrary path — only the exact `serde_json::Value` path resolves to
        // `Json`. An unrelated hand-written type sharing the name must not be
        // misclassified as JSON (Codex review finding on #1341).
        assert_eq!(ColumnType::from_rust_type("domain::Value"), None);
        assert_eq!(ColumnType::from_rust_type("my_crate::sub::Value"), None);
        assert_eq!(ColumnType::from_rust_type("crate::Value"), None);
        // A bare `Value` is ALSO rejected now (a follow-up Codex finding):
        // this function only sees the type token, never the file's `use`
        // declarations, so a bare `Value` imported via `use domain::Value;`
        // is indistinguishable from one imported via `use serde_json::Value;`.
        // Only the fully-qualified path is unambiguous.
        assert_eq!(ColumnType::from_rust_type("Value"), None);
    }

    #[test]
    fn from_rust_type_accepts_an_absolute_serde_json_value_path() {
        // `::serde_json::Value` (a leading `::`, an absolute path — sometimes
        // written defensively to avoid shadowing by a local module of the
        // same name) is valid Rust and must still resolve to `Json`. This
        // doesn't reopen the `Value` collision risk: `::domain::Value` still
        // normalises to `domain::Value`, which correctly stays `None`
        // (Codex review finding on #1341).
        assert_eq!(
            ColumnType::from_rust_type("::serde_json::Value"),
            Some(ColumnType::Json)
        );
        assert_eq!(ColumnType::from_rust_type("::domain::Value"), None);
    }

    #[test]
    fn from_rust_type_unknown_and_enum_are_none() {
        assert_eq!(ColumnType::from_rust_type("Status"), None);
        assert_eq!(ColumnType::from_rust_type("SomeOtherType"), None);
    }

    #[test]
    fn id_kind_rust_type() {
        assert_eq!(IdKind::BigSerial.rust_type(), "i64");
        assert_eq!(IdKind::Serial.rust_type(), "i32");
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
            IdKind::Serial.pk_sql(Backend::Postgres),
            "SERIAL PRIMARY KEY"
        );
        assert_eq!(
            IdKind::Serial.pk_sql(Backend::Sqlite),
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
        assert_eq!(IdKind::Serial.diesel_type(Backend::Postgres), "Int4");
        assert_eq!(IdKind::Serial.diesel_type(Backend::Sqlite), "Int4");
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
            definition: None,
            is_partial: false,
            key_columns: Vec::new(),
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
        // The serial / identity markers default to absent (a plain column).
        assert!(col.serial.is_none() && col.identity.is_none());

        let table = Table::new("widgets", Backend::Sqlite);
        assert!(table.managed);
        assert_eq!(table.backend, Backend::Sqlite);
        assert!(table.columns.is_empty());

        let schema = Schema::new(Backend::Sqlite);
        assert!(schema.tables.is_empty());
    }

    #[test]
    fn serial_and_identity_markers_are_serde_backward_compatible() {
        // A plain column (both markers absent) serializes WITHOUT the new keys, so
        // snapshots written before the fields existed stay byte-identical.
        let plain = Column::new("id", ColumnType::Int64);
        let json = serde_json::to_string(&plain).expect("serialize");
        assert!(
            !json.contains("serial") && !json.contains("identity"),
            "absent markers must be omitted from the serialized form: {json}"
        );

        // A pre-existing snapshot JSON with neither key deserializes to `None`.
        let legacy = r#"{"name":"id","ty":"Int64","nullable":false,"primary_key":true,"unique":false,"default":null,"references":null}"#;
        let back: Column = serde_json::from_str(legacy).expect("deserialize legacy");
        assert!(back.serial.is_none() && back.identity.is_none());

        // A populated marker round-trips.
        let mut serial_id = Column::new("id", ColumnType::Int64);
        serial_id.primary_key = true;
        serial_id.serial = Some(SerialKind::BigSerial);
        let mut identity_col = Column::new("n", ColumnType::Int64);
        identity_col.identity = Some("ALWAYS".to_owned());
        for col in [&serial_id, &identity_col] {
            let json = serde_json::to_string(col).expect("serialize");
            let back: Column = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*col, back);
        }
        assert!(
            serde_json::to_string(&serial_id)
                .unwrap()
                .contains("BigSerial")
        );
    }
}
