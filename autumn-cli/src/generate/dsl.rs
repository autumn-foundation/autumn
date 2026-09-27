//! Field-type DSL parser for `autumn generate`.
//!
//! Turns command-line tokens like `title:String`, `tags:Vec<u8>`, or
//! `published:Option<bool>` into a structured [`Field`] that knows both its
//! Rust type (for the `#[model]` struct) and its SQL type (for the migration).

use autumn_web::config::DatabaseBackend;

use super::GenerateError;
use super::naming;

/// Rust type a `{translatable}` column lowers to (issue #1384).
const TRANSLATED_RUST_TYPE: &str = "autumn_web::i18n::Translated";

/// SQL literal a translatable column defaults to: the empty JSON container.
const EMPTY_TRANSLATION_CONTAINER: &str = "'{}'";

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
    /// `from:col` — the source field a `slug` field auto-derives from on
    /// create when the submitted value is blank (issue #1260), e.g.
    /// `slug:slug{from:title}`. `slug` only; never a `#[validate]`/HTML5
    /// constraint.
    pub from: Option<String>,
    /// `encrypted` / `encrypted:deterministic` — at-rest attribute encryption
    /// (issue #1340), re-emitted as an `#[encrypted(…)]` attribute on the
    /// generated model field. `String`/`Text` only, and never nullable — see
    /// [`EncryptedMode`]. `None` for an ordinary plaintext column.
    pub encrypted: Option<EncryptedMode>,
    /// `scope:col` — the parent foreign-key column a `position` field's
    /// contiguous ordering is scoped to (issue #1358), e.g.
    /// `rank:position{scope:board_id}`. `position` only; `None` means the
    /// position is a single sequence over the whole table. Never a
    /// `#[validate]`/HTML5 constraint.
    pub scope: Option<String>,
    /// `translatable` — per-locale content storage (issue #1384), re-emitted
    /// as a `#[translatable]` attribute on the generated model field and an
    /// `autumn_web::i18n::Translated` Rust type. `String`/`Text` only, never
    /// nullable, and never combined with `{encrypted}`, `:unique`, or a state
    /// machine. `false` for an ordinary monolingual column.
    pub translatable: bool,
}

impl FieldConstraints {
    /// True when no modifier that fans out to a `#[validate(...)]` rule was
    /// declared (issue #1388's `min`/`max`/`email`/`url`).
    #[must_use]
    pub const fn validation_attrs_are_empty(&self) -> bool {
        self.min.is_none() && self.max.is_none() && !self.email && !self.url
    }

    /// True when no constraint modifier was declared.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.min.is_none()
            && self.max.is_none()
            && !self.email
            && !self.url
            && self.label.is_none()
            && self.from.is_none()
            && self.encrypted.is_none()
            && self.scope.is_none()
            && !self.translatable
    }
}

/// The at-rest encryption mode a field's `{encrypted}` modifier declares
/// (issue #1340). Mirrors `autumn_web::encryption::Mode` and the
/// `#[encrypted(...)]` attribute the `#[model]` macro parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptedMode {
    /// `{encrypted}` (or the explicit `{encrypted:randomized}`) — randomized
    /// AEAD, a fresh nonce per write. The safe default: equal plaintexts
    /// produce different ciphertext, so nothing leaks through the column — at
    /// the cost of making equality lookups (`WHERE col = ?`, `find_by_<col>`,
    /// a `UNIQUE` index) impossible.
    Randomized,
    /// `{encrypted:deterministic}` — the nonce is derived from the plaintext,
    /// so equal plaintexts produce equal ciphertext. Equality lookups and
    /// `UNIQUE` indexes work; the tradeoff is that an observer of the database
    /// can tell which rows share a value. An explicit opt-in by design.
    Deterministic,
}

impl EncryptedMode {
    /// Parse the value half of an `encrypted:<mode>` modifier.
    ///
    /// `admin_visible` and `versioned_ciphertext` are real `#[encrypted(...)]`
    /// options but deliberately not DSL-expressible: the point of generator
    /// wiring is that a scaffolded encrypted column is redacted in the admin
    /// and kept out of version history by default, so opting back out is a
    /// deliberate hand-edit. They get their own message rather than being
    /// lumped in with a typo.
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "deterministic" => Ok(Self::Deterministic),
            "randomized" => Ok(Self::Randomized),
            opt @ ("admin_visible" | "versioned_ciphertext") => Err(format!(
                "the `{opt}` encryption option is not expressible in the field DSL — a \
                 generated encrypted column is redacted in the admin and kept out of version \
                 history by default. Declare `{{encrypted}}` here and add \
                 `#[encrypted({opt})]` to the generated model field by hand if you need it."
            )),
            other => Err(format!(
                "unknown encryption mode '{other}'; supported: deterministic, randomized \
                 (the default). Use `{{encrypted:deterministic}}` when the column needs \
                 `find_by`/`exists_by` equality lookups or a UNIQUE index."
            )),
        }
    }
}

/// A single allowed edge in a field's state machine (issue #1326).
///
/// Mirrors the `from -> to[: guard]` grammar the `#[state_machine(transitions(…))]`
/// attribute macro accepts (see `autumn-macros`): `from`/`to` are bare state
/// identifiers and `guard`, when present, is the plain name of a `&self -> bool`
/// method that must return `true` for the edge to be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTransition {
    /// The state this edge starts from.
    pub from: String,
    /// The state this edge ends at.
    pub to: String,
    /// Optional guard: the name of a `&self` bool method gating the edge.
    pub guard: Option<String>,
}

/// The parsed state machine declared on a field via the DSL's trailing
/// `:states(…)` modifier (issue #1326). Carries the ordered set of allowed
/// [`StateTransition`] edges, which the model generator re-emits as a
/// `#[state_machine(transitions(…))]` attribute on the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateMachine {
    /// The allowed transitions, in declaration order.
    pub transitions: Vec<StateTransition>,
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
    /// The state machine declared via a trailing `:states(…)` modifier
    /// (issue #1326), or `None` for an ordinary field. Only valid on a
    /// non-nullable `String`/`Text` field — the model generator re-emits it
    /// as a `#[state_machine(transitions(…))]` attribute.
    pub state_machine: Option<StateMachine>,
}

impl Field {
    /// The Rust type for the `#[model]` struct.
    ///
    /// For [`FieldKind::Enum`], this is the generated enum's `PascalCase`
    /// name (see [`Field::enum_type_name`]) rather than [`FieldKind::rust_type`]'s
    /// `String` storage-representation fallback.
    #[must_use]
    pub fn rust_type(&self) -> String {
        // #1384: a `{translatable}` column is a per-locale container in Rust,
        // not a `String`. The SQL type is unchanged (`TEXT`) — the container
        // is stored as a JSON object — so only the Rust type moves.
        if self.constraints.translatable {
            return TRANSLATED_RUST_TYPE.to_owned();
        }
        let inner = self
            .enum_type_name()
            .unwrap_or_else(|| self.kind.rust_type().to_owned());
        if self.nullable {
            format!("Option<{inner}>")
        } else {
            inner
        }
    }

    /// [`Field::rust_type`] for the target `backend` (issue #1924).
    ///
    /// Postgres is byte-for-byte [`Field::rust_type`]. On `SQLite`, `Uuid` and
    /// `Decimal` render `autumn-web`'s `TEXT`-backed newtypes — see
    /// [`FieldKind::rust_type_for`].
    #[must_use]
    pub fn rust_type_for(&self, backend: DatabaseBackend) -> String {
        if backend == DatabaseBackend::Postgres {
            return self.rust_type();
        }
        if self.constraints.translatable {
            return TRANSLATED_RUST_TYPE.to_owned();
        }
        let inner = self
            .enum_type_name()
            .unwrap_or_else(|| self.kind.rust_type_for(backend).to_owned());
        if self.nullable {
            format!("Option<{inner}>")
        } else {
            inner
        }
    }

    /// Whether this column stores per-locale content (issue #1384) — declared
    /// with a `{translatable}` modifier, re-emitted as `#[translatable]`.
    #[must_use]
    pub const fn is_translatable(&self) -> bool {
        self.constraints.translatable
    }

    /// The SQL `DEFAULT` clause this field's storage needs, without the
    /// `DEFAULT` keyword, or `None` for a column with no default.
    ///
    /// Only `{translatable}` uses it today: the container column is `NOT NULL`
    /// and defaults to the empty JSON object, which is what makes the
    /// `ADD COLUMN` an ordinary *safe* migration (`autumn migrate check`
    /// classifies `ADD COLUMN … NOT NULL DEFAULT <constant>` as safe) rather
    /// than a table-rewriting backfill.
    #[must_use]
    pub const fn sql_default(&self) -> Option<&'static str> {
        if self.constraints.translatable {
            Some(EMPTY_TRANSLATION_CONTAINER)
        } else {
            None
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

    /// The `CREATE TABLE`/`ADD COLUMN` column type for the target `backend`
    /// (issue #1614). Identical to [`Field::sql_column_type`] for Postgres.
    /// On `SQLite`, `Decimal` maps to `TEXT` (see [`FieldKind::sqlite_sql_type`])
    /// — its `precision`/`scale` are not carried into the column type because
    /// `SQLite` has no fixed-precision `NUMERIC`; the exact value round-trips
    /// through `rust_decimal`'s text representation instead.
    #[must_use]
    pub fn sql_column_type_for(&self, backend: DatabaseBackend) -> String {
        // Postgres stays byte-for-byte identical to `sql_column_type`, including
        // the exact `NUMERIC(precision,scale)` rendering for decimals. Mirrors
        // `schema_type_for`'s structure.
        if backend == DatabaseBackend::Postgres {
            return self.sql_column_type();
        }
        // On SQLite every kind maps to the plain per-backend column type;
        // `Decimal` collapses to `TEXT` (see `FieldKind::sqlite_sql_type`) with
        // no `(p,s)` suffix, since SQLite has no fixed-precision NUMERIC.
        self.kind.sql_type_for(backend).to_owned()
    }

    /// The Diesel `schema.rs` type token for the target `backend` (issue
    /// #1614), including any `Nullable<…>` wrapping. Identical to
    /// [`Field::schema_type`] for Postgres.
    #[must_use]
    pub fn schema_type_for(&self, backend: DatabaseBackend) -> String {
        // Postgres stays byte-for-byte identical to `schema_type`.
        if backend == DatabaseBackend::Postgres {
            return self.schema_type();
        }
        let inner = self.kind.schema_type_for(backend);
        if self.nullable {
            format!("Nullable<{inner}>")
        } else {
            inner.to_string()
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
            // `RichText` shares `Text`'s length rules; its `email`/`url`
            // constraints are rejected at parse time, so the flags below are
            // always false for it.
            FieldKind::String | FieldKind::Text | FieldKind::RichText => {
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
            FieldKind::I32 | FieldKind::I64 | FieldKind::F32 | FieldKind::F64
                if c.min.is_some() || c.max.is_some() =>
            {
                let is_float = matches!(self.kind, FieldKind::F32 | FieldKind::F64);
                out.push(format!(
                    "range({})",
                    min_max_args(c.min.as_ref(), c.max.as_ref(), is_float)
                ));
            }
            _ => {}
        }
        out
    }

    /// The at-rest encryption mode declared via a `{encrypted}` /
    /// `{encrypted:deterministic}` modifier (issue #1340), or `None` for an
    /// ordinary plaintext column.
    #[must_use]
    pub const fn encrypted_mode(&self) -> Option<EncryptedMode> {
        self.constraints.encrypted
    }

    /// Whether this column is stored encrypted at rest, in either mode.
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        self.constraints.encrypted.is_some()
    }

    /// Whether this column is *randomized*-encrypted — the mode that makes
    /// equality lookups (`find_by_<col>`, `WHERE col = ?`, a `UNIQUE` index)
    /// impossible, because a fresh nonce per write means equal plaintexts never
    /// produce equal ciphertext. The generators refuse to emit those lookups
    /// for such a column (issue #1340 AC6), mirroring the runtime's
    /// `EncryptionError::RandomizedEqualityLookup`.
    #[must_use]
    pub const fn is_randomized_encrypted(&self) -> bool {
        matches!(self.constraints.encrypted, Some(EncryptedMode::Randomized))
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
    /// `richtext` — user-submitted Markdown, stored as `TEXT` (issue #1255).
    ///
    /// Storage-identical to [`FieldKind::Text`]: the column holds the Markdown
    /// **source**, never rendered HTML, so the Rust type is `String`, the
    /// diesel token is `Text`, and the SQL column type is `TEXT` on both
    /// backends. The distinction is entirely in the generated UI:
    ///
    /// - the form renders `autumn_web::form::rich_text_area_htmx_with_token_field`
    ///   — a Markdown editor with a no-JavaScript syntax toolbar and an htmx
    ///   live preview — instead of a bare `<textarea>`;
    /// - the `show` view renders the value through
    ///   `autumn_web::markdown::render_user_content`, which disables raw-HTML
    ///   passthrough and applies an allowlist sanitizer, instead of emitting
    ///   the source as escaped text;
    /// - the scaffold enables `autumn-web`'s `markdown` feature so both of
    ///   those resolve.
    ///
    /// The `{email}`/`{url}` format constraints are rejected on this kind
    /// (a Markdown body cannot satisfy a single-line format validator). The
    /// `{min}`/`{max}` length bounds are accepted and emit the same server-side
    /// `#[validate(length(…))]` rule as `Text`; unlike `Text` they emit no
    /// client-side `minlength`/`maxlength`, because the editor is rendered by
    /// `rich_text_area`, which takes no HTML5 constraint attributes.
    RichText,
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
    /// `json` (alias `jsonb`) — arbitrary structured data stored as a Postgres
    /// `JSONB` column (issue #1341). Unlike [`Attachment`](Self::Attachment),
    /// this maps directly to bare `serde_json::Value` — no wrapper struct —
    /// because diesel itself already implements `FromSql`/`ToSql<Jsonb, Pg>`
    /// **and** `<Json, Sqlite>` for `serde_json::Value` (diesel 2.3+, behind
    /// the `serde_json` + `sqlite` cargo features, both already
    /// unconditionally enabled by this workspace's `db` feature). So — unlike
    /// `Decimal`/`Uuid`, whose Rust types are foreign to `autumn-web` and
    /// whose `SQLite` conversion is genuinely blocked by the orphan rule — no
    /// custom `autumn-web` conversion code is needed on either backend.
    ///
    /// Ordinary nullable convention applies (unlike `Attachment`, this is
    /// **not** implicitly nullable): a bare `json` field is `NOT NULL`;
    /// `Option<json>` is nullable. On `SQLite` the column is `TEXT` storing
    /// plain-text JSON via diesel's `Json` sql-type — deliberately **not**
    /// diesel's `Jsonb` sql-type on `SQLite`, which uses a proprietary binary
    /// encoding unrelated to the human-readable "TEXT storing JSON" this
    /// field promises.
    Json,
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
    /// `slug{from:col}` — a human-readable, URL-safe routing key auto-derived
    /// from another field (issue #1260), e.g. `slug:slug{from:title}`.
    /// Storage-identical to [`FieldKind::String`] (`TEXT`, `String`), but
    /// always [`Field::unique`] (so it falls into the existing `unique`-field
    /// `UNIQUE INDEX` and `find_by_slug` repository machinery from issue
    /// #1032 for free) and never [`Field::nullable`] — a record with no slug
    /// would have no URL. The source field it derives from lives in
    /// [`FieldConstraints::from`], parsed from the mandatory `{from:...}`
    /// modifier. On create, a blank submitted value is auto-derived via
    /// [`autumn_web::slug::slugify`] and made unique with a deterministic
    /// `-2`, `-3`, ... suffix on collision; the scaffold's `show`/`edit`/
    /// `update`/`delete` routes resolve the record by this field instead of
    /// `id`.
    Slug,
    /// `position` (optionally `position{scope:col}`) — a server-managed
    /// contiguous ordering column (issue #1358), e.g. `rank:position` or,
    /// scoped to a parent, `rank:position{scope:board_id}`. Storage-wise a
    /// plain `i64`/`BIGINT` column (matching the default `i64` primary-key
    /// convention), but never nullable — every row belongs somewhere in its
    /// ordering — and never hand-assigned: the generated repository assigns
    /// the next contiguous value on insert, compacts on delete, and exposes
    /// `move_to`/`move_before`/`move_after`/`move_up`/`move_down` helpers, so
    /// the field is excluded from the generated `New*`/`Update*` structs the
    /// same way `id`/timestamps are. The optional [`FieldConstraints::scope`]
    /// keeps a separate contiguous `0..len-1` sequence per distinct value of
    /// the named column (typically a `references` foreign key). At most one
    /// `position` field is allowed per model.
    Position,
    /// `commentable` (e.g. `comments:commentable`) — threaded, **polymorphic**
    /// comments on this model (issue #1367).
    ///
    /// Storage-wise this is the counter-cache *source* column: a plain
    /// `i64`/`BIGINT`, never nullable (the maintenance is `c = c + 1`, and
    /// `NULL + 1` is `NULL`), never hand-assigned, and — like
    /// [`Position`](Self::Position) — excluded from the generated
    /// `New*`/`Update*` structs and the create/edit form, because the
    /// framework maintains it inside each comment's own transaction. The
    /// declared field name is normalised to `{singular}_count`
    /// (`comments:commentable` → `comment_count`), matching `#[model]`'s
    /// `{snake(child)}_count` convention.
    ///
    /// The rest of the feature is not a column at all: the token additionally
    /// emits the shared `comments(commentable_type, commentable_id, parent_id,
    /// …)` migration — once per project, since the table is shared — and
    /// `#[commentable(...)]` on the generated `#[model]`, which is what brings
    /// the repository's `add_comment`/`comment_thread`/`delete_comment`
    /// helpers into existence. At most one `commentable` field per model.
    Commentable,
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
            Self::String | Self::Text | Self::RichText | Self::Enum | Self::Slug => "String",
            Self::I32 => "i32",
            // `References` and `Position` are always `i64`, matching the
            // default `i64` PK convention.
            Self::I64 | Self::References | Self::Position | Self::Commentable => "i64",
            Self::Bool => "bool",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Uuid => "uuid::Uuid",
            Self::NaiveDateTime => "chrono::NaiveDateTime",
            Self::DateTime => "chrono::DateTime<chrono::Utc>",
            Self::Bytea => "Vec<u8>",
            Self::Attachment => "autumn_web::storage::Blob",
            Self::Json => "serde_json::Value",
            Self::Decimal { .. } => "rust_decimal::Decimal",
        }
    }

    /// [`FieldKind::rust_type`] for the target `backend` (issue #1924).
    ///
    /// Postgres is byte-for-byte [`FieldKind::rust_type`]. On `SQLite`, `Uuid`
    /// and `Decimal` render `autumn-web`'s `TEXT`-backed newtypes instead of
    /// `uuid::Uuid` / `rust_decimal::Decimal`: both wrapped types are foreign
    /// to `autumn-web`, and diesel blanket-implements `AsExpression` for every
    /// `Expression`, so no crate but `uuid` / `rust_decimal` itself can give
    /// them a `SQLite` conversion. The newtypes are `Copy`, deref to the
    /// wrapped type, convert with `From`/`Into`, and are `#[serde(transparent)]`,
    /// so `Display`, `FromStr` and JSON match the wrapped type exactly.
    #[must_use]
    pub const fn rust_type_for(self, backend: DatabaseBackend) -> &'static str {
        match backend {
            DatabaseBackend::Postgres => self.rust_type(),
            DatabaseBackend::Sqlite => match self {
                Self::Uuid => SQLITE_UUID_RUST_TYPE,
                Self::Decimal { .. } => SQLITE_DECIMAL_RUST_TYPE,
                _ => self.rust_type(),
            },
        }
    }

    /// Diesel `table!` schema type token.
    #[must_use]
    pub const fn schema_type(self) -> &'static str {
        match self {
            Self::String | Self::Text | Self::RichText | Self::Enum | Self::Slug => "Text",
            Self::I32 => "Int4",
            Self::I64 | Self::References | Self::Position | Self::Commentable => "Int8",
            Self::Bool => "Bool",
            Self::F32 => "Float4",
            Self::F64 => "Float8",
            Self::Uuid => "Uuid",
            Self::NaiveDateTime => "Timestamp",
            Self::DateTime => "Timestamptz",
            Self::Bytea => "Bytea",
            Self::Attachment | Self::Json => "Jsonb",
            Self::Decimal { .. } => "Numeric",
        }
    }

    /// `PostgreSQL` column type, without `NOT NULL` / `NULL`.
    #[must_use]
    pub const fn sql_type(self) -> &'static str {
        match self {
            Self::String | Self::Text | Self::RichText | Self::Enum | Self::Slug => "TEXT",
            Self::I32 => "INTEGER",
            Self::I64 | Self::References | Self::Position | Self::Commentable => "BIGINT",
            Self::Bool => "BOOLEAN",
            Self::F32 => "REAL",
            Self::F64 => "DOUBLE PRECISION",
            Self::Uuid => "UUID",
            Self::NaiveDateTime => "TIMESTAMP",
            Self::DateTime => "TIMESTAMPTZ",
            Self::Bytea => "BYTEA",
            Self::Attachment | Self::Json => "JSONB",
            Self::Decimal { .. } => "NUMERIC",
        }
    }

    /// `SQLite` column type, without `NOT NULL` / `NULL` (`SQLite` foundation,
    /// issue #1614). `SQLite`'s storage classes are few and forgiving, so every
    /// DSL kind maps to a working column type — nothing is rejected at
    /// generate time on this axis (see [`FieldKind::sql_type_for`]). Notable
    /// differences from the Postgres mapping:
    ///
    /// - `bool` -> `INTEGER` (`SQLite` has no boolean type; `0`/`1`).
    /// - `f32`/`f64` -> `REAL` (`SQLite` `REAL` is always 8-byte double).
    /// - `Uuid` -> `TEXT` (no native `uuid`; stored as its canonical string).
    /// - `NaiveDateTime`/`DateTime` -> `TEXT` (ISO-8601 / RFC 3339 string;
    ///   `SQLite` has no dedicated timestamp type and its date functions operate
    ///   on ISO-8601 text).
    /// - `Bytea` -> `BLOB`.
    /// - `Attachment` -> `TEXT` (the `Blob` metadata JSON, stored as text
    ///   rather than Postgres `JSONB`).
    /// - `json`/`jsonb` -> `TEXT` (issue #1341) — plain-text JSON, via
    ///   diesel's own `Json` sql-type (see [`FieldKind::sqlite_schema_type`]),
    ///   not Postgres `JSONB`.
    /// - `Decimal` -> `TEXT` (`SQLite` `NUMERIC` affinity coerces to REAL/INTEGER
    ///   and would lose exactness; `rust_decimal` round-trips losslessly through
    ///   text).
    #[must_use]
    #[allow(
        clippy::match_same_arms,
        reason = "every FieldKind is listed explicitly to document the complete SQLite \
                  column mapping (AC #4), even where several kinds share a storage type"
    )]
    pub const fn sqlite_sql_type(self) -> &'static str {
        match self {
            Self::String | Self::Text | Self::RichText | Self::Enum | Self::Slug => "TEXT",
            Self::I32
            | Self::I64
            | Self::References
            | Self::Position
            | Self::Commentable
            | Self::Bool => "INTEGER",
            Self::F32 | Self::F64 => "REAL",
            Self::Uuid => "TEXT",
            Self::NaiveDateTime | Self::DateTime => "TEXT",
            Self::Bytea => "BLOB",
            Self::Attachment | Self::Json => "TEXT",
            Self::Decimal { .. } => "TEXT",
        }
    }

    /// The SQL column type for the target `backend` (issue #1614). Postgres
    /// keeps [`FieldKind::sql_type`] byte-for-byte; `SQLite` uses
    /// [`FieldKind::sqlite_sql_type`].
    #[must_use]
    pub const fn sql_type_for(self, backend: DatabaseBackend) -> &'static str {
        match backend {
            DatabaseBackend::Postgres => self.sql_type(),
            DatabaseBackend::Sqlite => self.sqlite_sql_type(),
        }
    }

    /// Diesel `table!` schema type token for a `SQLite` target (issue #1614).
    ///
    /// The Postgres-only diesel sql-types (`Timestamptz`, `Jsonb`, `Uuid`,
    /// `Bytea`, `Numeric`) are not implemented for diesel's `SQLite` backend, so
    /// they are remapped to types `SQLite` does support: `Uuid`/`Attachment`/
    /// `Decimal` -> `Text`, `NaiveDateTime` -> `Timestamp`, `DateTime<Utc>` ->
    /// `TimestamptzSqlite`, `Bytea` -> `Binary`.
    ///
    /// `NaiveDateTime` maps to the core `diesel::sql_types::Timestamp` — a
    /// backend-agnostic sql-type that is exported without diesel's `sqlite`
    /// feature and for which diesel implements `FromSql`/`ToSql<Timestamp,
    /// Sqlite>` targeting `NaiveDateTime`, so it compiles in a generated app.
    ///
    /// `DateTime<Utc>` maps to diesel's `TimestamptzSqlite` sql-type (issue
    /// #1924). diesel implements `FromSql`/`ToSql<TimestamptzSqlite, Sqlite>`
    /// for `DateTime<Utc>` under its `sqlite` + `chrono` features. A generated
    /// `SQLite` app depends on `autumn-web` with the `sqlite` feature, which
    /// pulls in `diesel/sqlite` (and `diesel/chrono`) by cargo feature
    /// unification, so both `TimestamptzSqlite` and the conversion resolve — the
    /// value round-trips as an RFC 3339 UTC string (`SQLite` `TEXT` affinity),
    /// which sorts and compares chronologically.
    ///
    /// `Attachment` (`autumn_web::storage::Blob`) maps to `Text` and stores the
    /// `Blob` metadata as its JSON body; `autumn-web` provides
    /// `FromSql`/`ToSql<Text, Sqlite>` + `AsExpression`/`FromSqlRow<Text>` for
    /// `Blob` under its `sqlite` feature (issue #1924).
    ///
    /// `json`/`jsonb` (issue #1341) maps to diesel's own `Json` sql-type —
    /// deliberately distinct from both `Text` (no `serde_json::Value`
    /// conversion) and `Jsonb` (diesel's `SQLite` `Jsonb` uses a proprietary
    /// binary encoding, not plain text). diesel itself implements
    /// `FromSql`/`ToSql<Json, Sqlite> for serde_json::Value` (diesel 2.3+,
    /// `serde_json` + `sqlite` features, both already unconditionally on via
    /// this workspace's `db` feature), storing plain-text JSON as `TEXT` — no
    /// `autumn-web` conversion code needed, unlike `Attachment`.
    ///
    /// `Uuid`, `Decimal` and `Enum` map to `Text` (issue #1924). `Uuid` and
    /// `Decimal` reach it through `autumn-web`'s `TEXT`-backed newtypes, which
    /// [`FieldKind::rust_type_for`] renders in place of the foreign
    /// `uuid::Uuid` / `rust_decimal::Decimal`; a generated `Enum` is local to
    /// the app, so `render_enum_decl` emits its `Text`/`Sqlite` impls directly.
    #[must_use]
    #[allow(
        clippy::match_same_arms,
        reason = "every FieldKind is listed explicitly to document the complete SQLite \
                  diesel-type mapping (AC #4), even where several kinds share a type"
    )]
    pub const fn sqlite_schema_type(self) -> &'static str {
        match self {
            Self::String | Self::Text | Self::RichText | Self::Enum | Self::Slug => "Text",
            Self::I32 => "Int4",
            Self::I64 | Self::References | Self::Position | Self::Commentable => "Int8",
            Self::Bool => "Bool",
            Self::F32 => "Float4",
            Self::F64 => "Float8",
            Self::Uuid => "Text",
            // `NaiveDateTime` -> core `Timestamp` (ungated, compiles).
            Self::NaiveDateTime => "Timestamp",
            // `DateTime<Utc>` -> `TimestamptzSqlite` (diesel's sqlite+chrono
            // conversion, available via the app's `autumn-web` sqlite feature).
            Self::DateTime => "TimestamptzSqlite",
            Self::Bytea => "Binary",
            Self::Attachment => "Text",
            // Diesel's own `Json` sql-type — NOT `Text` (no built-in
            // `serde_json::Value` conversion) and NOT `Jsonb` (SQLite's
            // proprietary binary encoding). See this method's doc comment.
            Self::Json => "Json",
            Self::Decimal { .. } => "Text",
        }
    }

    /// Whether this kind's rendered Rust model type has a working diesel
    /// `FromSql`/`ToSql` on diesel's `SQLite` backend in a generated app's
    /// feature set (diesel `sqlite` + `chrono`, without `uuid`/`numeric`) —
    /// determined empirically (issue #1614 AC #4; conversions wired in #1924).
    ///
    /// Supported on `SQLite` (issue #1924):
    /// - `NaiveDateTime` via the core, ungated `Timestamp` sql-type.
    /// - `DateTime<Utc>` via diesel's `TimestamptzSqlite` sql-type — its
    ///   `sqlite`+`chrono` conversion resolves through the app's `autumn-web`
    ///   `sqlite` feature (RFC 3339 UTC text, chronologically sortable).
    /// - `Attachment` (`autumn_web::storage::Blob`) — `autumn-web` provides
    ///   `FromSql`/`ToSql<Text, Sqlite>` + `AsExpression`/`FromSqlRow<Text>` for
    ///   `Blob` under its `sqlite` feature (`Blob` is a local type, so these
    ///   impls are orphan-rule-legal), storing the metadata JSON as `TEXT`.
    /// - `json`/`jsonb` (issue #1341) — `serde_json::Value` is a *foreign*
    ///   type, but diesel itself (not `autumn-web`) already implements
    ///   `FromSql`/`ToSql<Json, Sqlite>` for it, so there is no orphan-rule
    ///   obstacle at all — unlike `Uuid`/`Decimal` below, which are genuinely
    ///   blocked because neither diesel nor `autumn-web` provides the impl.
    ///
    /// - `Uuid` and `Decimal` via `autumn-web`'s `TEXT`-backed newtypes, which
    ///   [`FieldKind::rust_type_for`] renders in place of the foreign
    ///   `uuid::Uuid` / `rust_decimal::Decimal`.
    /// - `Enum` — the generated enum is local to the app, so `render_enum_decl`
    ///   emits its `ToSql`/`FromSql<Text, Sqlite>` impls directly.
    ///
    /// Every kind now converts, so nothing is rejected. The gate stays as the
    /// decision a NEW kind must make: return `false` here rather than emit code
    /// that cannot compile, and `reject_sqlite_unsupported_field_kinds` reports
    /// it at generate time.
    #[must_use]
    pub const fn sqlite_has_diesel_conversion(self) -> bool {
        match self {
            Self::String
            | Self::Text
            | Self::RichText
            | Self::Slug
            | Self::I32
            | Self::I64
            | Self::References
            | Self::Position
            | Self::Commentable
            | Self::Bool
            | Self::F32
            | Self::F64
            | Self::Uuid
            | Self::NaiveDateTime
            | Self::DateTime
            | Self::Bytea
            | Self::Attachment
            | Self::Json
            | Self::Enum
            | Self::Decimal { .. } => true,
        }
    }

    /// The diesel `table!` schema type token for the target `backend`
    /// (issue #1614). Postgres keeps [`FieldKind::schema_type`] byte-for-byte;
    /// `SQLite` uses [`FieldKind::sqlite_schema_type`].
    #[must_use]
    pub const fn schema_type_for(self, backend: DatabaseBackend) -> &'static str {
        match backend {
            DatabaseBackend::Postgres => self.schema_type(),
            DatabaseBackend::Sqlite => self.sqlite_schema_type(),
        }
    }

    /// The DSL token a user writes for this kind, for error messages that need
    /// to name the field's *declared* type rather than its Rust storage type.
    ///
    /// [`FieldKind::rust_type`] is the wrong thing to report in a diagnostic
    /// about the DSL: `richtext`, `enum{…}` and `slug{…}` all render as
    /// `String` in Rust, so "only applies to `String` fields … got a `String`
    /// field" is what a user would see for `body:richtext{encrypted}`.
    #[must_use]
    pub const fn dsl_token(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Text => "Text",
            Self::RichText => "richtext",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::Bool => "bool",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Uuid => "Uuid",
            Self::NaiveDateTime => "NaiveDateTime",
            Self::DateTime => "DateTime",
            Self::Bytea => "Bytea",
            Self::Attachment => "Attachment",
            Self::Json => "json",
            Self::References => "references",
            Self::Enum => "enum{…}",
            Self::Decimal { .. } => "decimal{…}",
            Self::Slug => "slug{…}",
            Self::Position => "position",
            Self::Commentable => "commentable",
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

    /// Returns `true` for a `richtext` field (issue #1255).
    ///
    /// Used by the scaffold generator to pick the Markdown editor control and
    /// the sanitizing `show`-view render, and to enable `autumn-web`'s
    /// `markdown` feature on the project. Storage-wise the column is
    /// indistinguishable from [`FieldKind::Text`].
    #[must_use]
    pub const fn is_rich_text(self) -> bool {
        matches!(self, Self::RichText)
    }

    /// Returns `true` for a `slug{from:col}` routing-key field (issue #1260).
    ///
    /// Used by the scaffold generator to key `show`/`edit`/`update`/`delete`
    /// routes and generated links off this field instead of `id`, and to
    /// auto-derive its value from [`FieldConstraints::from`] on create.
    #[must_use]
    pub const fn is_slug(self) -> bool {
        matches!(self, Self::Slug)
    }

    /// Returns `true` for an exact-precision `decimal` field.
    ///
    /// Used by the scaffold/model generators to know when the project needs
    /// the `rust_decimal` dependency wired in.
    #[must_use]
    pub const fn is_decimal(self) -> bool {
        matches!(self, Self::Decimal { .. })
    }

    /// Returns `true` for a server-managed `position` ordering field (issue
    /// #1358).
    ///
    /// Used by the model/migration/repository/scaffold generators to: exclude
    /// the column from `New*`/`Update*` structs, emit its `NOT NULL BIGINT`
    /// column plus auto-index, wire `position(column = "...", scope = "...")`
    /// into the generated `#[repository(...)]` attribute, and order the
    /// scaffold index view by it with Move up/down buttons.
    #[must_use]
    pub const fn is_position(self) -> bool {
        matches!(self, Self::Position)
    }

    /// Returns `true` for a `commentable` polymorphic-comments field (issue
    /// #1367).
    ///
    /// Used by the model/migration/scaffold generators to: keep the counter
    /// column out of `New*`/`Update*` structs and the create/edit form, write
    /// `#[commentable(...)]` onto the generated model, and emit the shared
    /// `comments` table migration.
    #[must_use]
    pub const fn is_commentable(self) -> bool {
        matches!(self, Self::Commentable)
    }

    /// Returns `true` for a column the *framework* assigns and maintains, so
    /// no generated form, `New*` struct or `Update*` struct may offer it.
    ///
    /// One predicate rather than a growing `is_position() || is_commentable()
    /// || …` chain at every call site — a new server-managed kind is then a
    /// one-line change here instead of a hunt through the generators.
    #[must_use]
    pub const fn is_server_managed(self) -> bool {
        matches!(self, Self::Position | Self::Commentable)
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

    /// The primary-key SQL fragment for the target `backend` (issue #1614).
    /// Postgres keeps [`IdType::pk_sql`] byte-for-byte. On `SQLite`:
    ///
    /// - `BigSerial` -> `INTEGER PRIMARY KEY AUTOINCREMENT` — an alias of the
    ///   `rowid` that never reuses a freed value (`SQLite` has no `BIGSERIAL`).
    /// - `Uuid` -> `TEXT PRIMARY KEY` — `SQLite` has no `uuid` type nor a
    ///   `gen_random_uuid()` default, so the application supplies the id.
    #[must_use]
    pub const fn pk_sql_for(self, backend: DatabaseBackend) -> &'static str {
        match backend {
            DatabaseBackend::Postgres => self.pk_sql(),
            DatabaseBackend::Sqlite => match self {
                Self::BigSerial => "INTEGER PRIMARY KEY AUTOINCREMENT",
                Self::Uuid => "TEXT PRIMARY KEY",
            },
        }
    }

    /// Diesel `table!` schema type token for the primary key on the target
    /// `backend` (issue #1614). Postgres keeps [`IdType::schema_type`]; `SQLite`
    /// remaps `Uuid` (unsupported by diesel's `SQLite` backend) to `Text`.
    #[must_use]
    pub const fn schema_type_for(self, backend: DatabaseBackend) -> &'static str {
        match backend {
            DatabaseBackend::Postgres => self.schema_type(),
            DatabaseBackend::Sqlite => match self {
                Self::BigSerial => "Int8",
                Self::Uuid => "Text",
            },
        }
    }

    /// An optional migration comment documenting trade-offs. Only `Uuid`
    /// returns `Some`, pointing developers toward the `UUIDv7` upgrade path.
    #[must_use]
    pub const fn migration_comment_for(self, backend: DatabaseBackend) -> Option<&'static str> {
        match backend {
            // The comment documents the Postgres-only `gen_random_uuid()` /
            // `uuid_generate_v7()` upgrade path, which does not apply to a
            // SQLite target (no such functions; the app supplies the id).
            DatabaseBackend::Postgres => self.migration_comment(),
            DatabaseBackend::Sqlite => None,
        }
    }

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
pub const SUPPORTED_TYPES: &str = "String, Text, richtext, i32, i64, bool, f32, f64, \
    Uuid, NaiveDateTime, DateTime, Vec<u8>, Bytea, Attachment, json (alias jsonb), references, \
    enum{a,b,…}, decimal{precision,scale}, slug{from:col}, position (optionally \
    position{scope:col}), commentable, Option<…>, :unique, String{encrypted}, \
    String{encrypted:deterministic}, String{translatable}";

/// The DSL field kinds that map to a working diesel `SQLite` conversion
/// (issue #1614 AC #4; #1924) — the complement of the kinds
/// [`FieldKind::sqlite_has_diesel_conversion`] rejects. As of #1924 that
/// complement is every kind, so this mirrors [`SUPPORTED_TYPES`]; it is still
/// the message a future unconvertible kind would be reported against.
pub const SQLITE_SUPPORTED_KINDS: &str = "String, Text, richtext, i32, i64, bool, f32, f64, \
    Uuid, NaiveDateTime, DateTime, Vec<u8>, Bytea, Attachment, json (alias jsonb), references, \
    enum{a,b,…}, decimal{precision,scale}, slug{from:col}, position (optionally \
    position{scope:col}), commentable, Option<…>, :unique";

/// The Rust type a `Uuid` field renders as on `SQLite` — `autumn-web`'s
/// `TEXT`-backed newtype (issue #1924). See [`FieldKind::rust_type_for`].
pub const SQLITE_UUID_RUST_TYPE: &str = "autumn_web::db::sqlite_types::SqliteUuid";

/// The Rust type a `decimal{p,s}` field renders as on `SQLite` — `autumn-web`'s
/// `TEXT`-backed newtype (issue #1924). See [`FieldKind::rust_type_for`].
pub const SQLITE_DECIMAL_RUST_TYPE: &str = "autumn_web::db::sqlite_types::SqliteDecimal";

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
///
/// Like `jsonb` (which round-trips as a plain `FieldKind::Json`, losing the
/// distinction from a hand-written `json` column), a `position` column has
/// no distinguishing catalog type of its own — it's a plain `int4`/`int8`
/// NOT NULL column plus an index and a pair of triggers. `db pull` therefore
/// introspects it back as an ordinary [`FieldKind::I32`]/[`FieldKind::I64`],
/// not [`FieldKind::Position`]; this is expected, not a bug — there is no
/// SQL-level marker to recover the `position`-ness from.
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
        // it unsupported rather than risk corrupting brownfield data. Issue
        // #1341 deepens the same ambiguity rather than resolving it: `json`
        // fields ALSO forward-map to `JSONB`, so a pulled `jsonb` column could
        // now be either a `Blob` or a `serde_json::Value` — still no way to
        // tell, so this stays unsupported on purpose (`db pull` is forward-
        // generation tooling; reverse introspection of a JSONB column is out
        // of this issue's scope).
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
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "the `{from:<field>}`/`{from:title}` mentions in the slug error message are the \
              DSL's own constraint-modifier syntax, not format-string placeholders"
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
    // Peel a trailing `:states(from -> to, …)` state-machine modifier (issue
    // #1326) *before* the `:unique` split below, so its internal `->`, `,`,
    // and guard `:` tokens are never mistaken for a `:unique` suffix. The
    // clause is paren-delimited (like the `{…}` constraint block is
    // brace-delimited), so it stays unambiguous against the other modifiers.
    let (rest, state_machine_body) = split_state_machine_modifier(rest);
    let state_machine = match state_machine_body {
        Some(body) => {
            Some(
                parse_state_machine(body).map_err(|reason| GenerateError::InvalidField {
                    token: token.to_owned(),
                    reason,
                })?,
            )
        }
        None => None,
    };
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
        if state_machine.is_some() {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: STATE_MACHINE_STRING_ONLY.to_owned(),
            });
        }
        return Ok(Field {
            name: name.to_owned(),
            kind: FieldKind::Enum,
            nullable,
            variants,
            unique,
            constraints: FieldConstraints::default(),
            state_machine: None,
        });
    }

    if let Some((precision, scale, nullable)) =
        parse_decimal_type(ty).map_err(|reason| GenerateError::InvalidField {
            token: token.to_owned(),
            reason,
        })?
    {
        if state_machine.is_some() {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: STATE_MACHINE_STRING_ONLY.to_owned(),
            });
        }
        return Ok(Field {
            name: name.to_owned(),
            kind: FieldKind::Decimal { precision, scale },
            nullable,
            variants: Vec::new(),
            unique,
            constraints: FieldConstraints::default(),
            state_machine: None,
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

    // A state machine (issue #1326) is only meaningful on a plain, non-nullable
    // `String`/`Text` column — the `#[state_machine]` macro requires the field's
    // Rust type to be exactly `String`, and the generated `transition_*` methods
    // operate on `&str`. Reject it anywhere else with a clear message rather than
    // emitting an attribute the macro will refuse to compile.
    if state_machine.is_some() && (nullable || !matches!(kind, FieldKind::String | FieldKind::Text))
    {
        return Err(GenerateError::InvalidField {
            token: token.to_owned(),
            reason: STATE_MACHINE_STRING_ONLY.to_owned(),
        });
    }

    // `references` fields always end in `_id` — `post:references` resolves to
    // the column `post_id`. Tolerate an already-suffixed name (`post_id:references`)
    // rather than doubling the suffix.
    let name = if kind == FieldKind::References && !name.ends_with("_id") {
        format!("{name}_id")
    } else if kind == FieldKind::Commentable {
        // `comments:commentable` names the *feature*, not the column. The
        // column it actually adds is the counter-cache source, whose name
        // follows `#[model]`'s `{snake(child)}_count` convention — so the
        // declared name is singularised and suffixed, and an already-suffixed
        // name is left alone rather than doubled.
        if name.ends_with("_count") {
            name.to_owned()
        } else {
            format!("{}_count", naming::singularize(name))
        }
    } else {
        name.to_owned()
    };

    // `commentable` is a whole feature behind one token, not a value the caller
    // supplies: the counter it adds is maintained by the framework inside each
    // comment's transaction, so it can be neither nullable (`NULL + 1` is
    // `NULL`) nor `:unique` (every model starts at 0), and it takes no `{…}`
    // modifiers at all.
    if kind == FieldKind::Commentable {
        if nullable {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `commentable` field cannot be nullable — it is the \
                         framework-maintained comment counter, and `NULL + 1` is `NULL`"
                    .into(),
            });
        }
        if unique {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `commentable` field cannot be `:unique` — it is a comment \
                         counter, and every model starts at 0"
                    .into(),
            });
        }
        if !constraints.is_empty() {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `commentable` field takes no `{…}` modifiers — configure the \
                         association on the generated model's `#[commentable(...)]` \
                         attribute instead"
                    .into(),
            });
        }
    }

    // A `slug` field (issue #1260) is always the record's routing key: it
    // can never be nullable (every record needs a URL) and always needs a
    // `{from:...}` modifier naming the field it auto-derives from on
    // create — enforced here rather than left to a downstream codegen panic.
    if kind == FieldKind::Slug {
        if nullable {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `slug` field cannot be nullable — it is the record's routing key \
                         and every record needs a URL"
                    .into(),
            });
        }
        if constraints.from.is_none() {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `slug` field requires a `{from:<field>}` modifier naming its \
                         source field, e.g. `slug:slug{from:title}`"
                    .into(),
            });
        }
    }
    // A slug is implicitly unique (it's the routing key) — falls into the
    // existing `unique`-field `UNIQUE INDEX` and `find_by_slug` repository
    // machinery (issue #1032) for free, whether or not `:unique` was typed.
    let unique = unique || kind == FieldKind::Slug;

    // A `position` field (issue #1358) can never be nullable — every row has
    // a place in its ordering — and can never be `:unique` — the whole point
    // is a dense, repository-maintained `0..len-1` sequence per scope, not a
    // one-off distinct value.
    if kind == FieldKind::Position {
        if nullable {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `position` field cannot be nullable — every row has a place in its \
                         ordering"
                    .into(),
            });
        }
        if unique {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `position` field cannot be `:unique` — the repository maintains a \
                         dense `0..len-1` sequence per scope, not a one-off distinct value"
                    .into(),
            });
        }
    }

    // ── `{encrypted}` cross-checks (issue #1340) ────────────────────────────
    // The kind allowlist already ran in `set_encrypted_constraint`; what's left
    // are the checks that need the *whole* parsed field.
    if let Some(mode) = constraints.encrypted {
        // `#[encrypted]` supports non-null `String` columns in v1 — the diesel
        // `serialize_as`/`deserialize_as` wrappers are not threaded through
        // `Option<String>`. Reject here rather than emit a model the macro
        // refuses to compile.
        if nullable {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "an `encrypted` field cannot be nullable — `#[encrypted]` supports \
                         non-null `String` columns in v1. Drop the `Option<…>` (store an \
                         empty string for \"unset\")."
                    .into(),
            });
        }
        // The concrete blocker: the scaffold's generated transition handler
        // writes the new state with a RAW `diesel::update(...).set(col.eq(
        // new_state))`, which never passes through the `serialize_as` wrapper
        // that encrypts the column — so every transition would store PLAINTEXT
        // in a column the reader then tries to decrypt. (Encrypting a state
        // column is also semantically wrong: it is a low-entropy closed set, so
        // randomized mode makes state queries impossible and deterministic mode
        // leaks every row's state through ciphertext equality.) Refuse the pair
        // rather than half-support it.
        if state_machine.is_some() {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "an `encrypted` field cannot also declare a `states(…)` state machine: \
                         the generated transition handler writes the new state with a raw \
                         `UPDATE … SET col = 'state'` that bypasses encryption, so transitions \
                         would store plaintext in the encrypted column. (A state column is also \
                         a low-entropy closed set, where randomized mode makes state queries \
                         impossible and deterministic mode leaks every row's state.)"
                    .into(),
            });
        }
        // AC6: a randomized column can never satisfy an equality lookup, so a
        // `UNIQUE` index over it enforces nothing and the free `find_by_<col>`
        // the generators derive from `unique` fails at runtime with
        // `EncryptionError::RandomizedEqualityLookup`. Fail here, where the fix
        // is one word away.
        if unique && mode == EncryptedMode::Randomized {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: randomized_equality_lookup_reason(&name, "is `unique`"),
            });
        }
    }

    // ── `{translatable}` cross-checks (issue #1384) ─────────────────────────
    // The kind allowlist already ran while parsing the modifier; these need the
    // whole parsed field.
    if constraints.translatable {
        // The container itself models "no translation" as an empty map, so a
        // nullable column would give two spellings for one state — and the
        // `#[model]` macro refuses `Option<Translated>` for the same reason.
        if nullable {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `translatable` field cannot be nullable — the per-locale container \
                         already represents \"no translation\" as an empty map. Drop the \
                         `Option<…>`."
                    .into(),
            });
        }
        // The column holds one opaque ciphertext envelope under `{encrypted}`,
        // which has no per-locale structure to resolve against.
        if constraints.encrypted.is_some() {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `translatable` field cannot also be `encrypted`: an encrypted column \
                         stores one opaque envelope, with no per-locale structure to resolve. \
                         Keep a separate encrypted column for the secret."
                    .into(),
            });
        }
        // A UNIQUE index (and the free `find_by_<col>` the generators derive
        // from it) would compare whole JSON containers, so two records with
        // identical text translated into different locale sets never collide.
        if unique {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `translatable` field cannot be `:unique`: the index would compare \
                         whole per-locale containers, so identical text translated into \
                         different locale sets would never collide, and the derived \
                         `find_by_<col>` lookup could never match. Put the uniqueness on a \
                         non-translatable column (e.g. a `slug`)."
                    .into(),
            });
        }
        // The `{min}/{max}/{email}/{url}` fan-out emits `#[validate(length(..))]`
        // / `#[validate(email)]` on the field, and `validator` implements those
        // rules for `String`-shaped types only — there is no impl for a
        // per-locale container, so the generated model would not compile. (And
        // even with one, "which locale is this rule about?" has no single
        // answer.) Refuse here, where the fix is one token away.
        let validation_modifiers = !constraints.validation_attrs_are_empty();
        if validation_modifiers {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `translatable` field cannot carry `min`/`max`/`email`/`url` \
                         constraints: those emit a `#[validate(...)]` rule that `validator` \
                         implements for strings only, and a per-locale container has one \
                         value per locale rather than one string to measure. Validate the \
                         individual translations in your own edit handler."
                    .into(),
            });
        }
        // The scaffold's transition handler writes the state with a raw
        // `UPDATE … SET col = 'state'`, which would replace the whole container
        // with a bare state name. A state is also locale-invariant by
        // definition.
        if state_machine.is_some() {
            return Err(GenerateError::InvalidField {
                token: token.to_owned(),
                reason: "a `translatable` field cannot also declare a `states(…)` state \
                         machine: a state name is locale-invariant, and the generated \
                         transition handler's raw `UPDATE … SET col = 'state'` would replace \
                         the whole per-locale container."
                    .into(),
            });
        }
    }

    Ok(Field {
        name,
        kind,
        nullable,
        variants: Vec::new(),
        unique,
        constraints,
        state_machine,
    })
}

/// The shared explanation for every generate-time refusal of an equality
/// lookup against a **randomized**-encrypted column (issue #1340 AC6).
///
/// `what` is a short verb phrase naming the thing that would perform the
/// lookup — "is `unique`", "has a `--query` derived equality lookup" — so each
/// call site reads naturally while the diagnosis and the fix stay identical.
/// The wording deliberately mirrors the runtime's
/// `EncryptionError::RandomizedEqualityLookup`, so a developer who has met one
/// recognises the other.
#[must_use]
pub(super) fn randomized_equality_lookup_reason(field: &str, what: &str) -> String {
    format!(
        "field '{field}' {what}, but `{{encrypted}}` is randomized: a fresh nonce per write \
         means equal plaintexts never produce equal ciphertext, so the lookup can never match \
         (the runtime raises `EncryptionError::RandomizedEqualityLookup`) and a UNIQUE index \
         over it would enforce nothing. Declare it `{{encrypted:deterministic}}` to support \
         `find_by`/`exists_by` equality lookups and real uniqueness, or drop the lookup."
    )
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

/// The single error message used when a `:states(…)` state-machine modifier
/// (issue #1326) is declared on anything but a plain, non-nullable
/// `String`/`Text` field. Shared so the enum, decimal, and scalar branches of
/// [`parse_field`] can never drift.
const STATE_MACHINE_STRING_ONLY: &str =
    "a `states(…)` state machine is only supported on a non-nullable `String`/`Text` field";

/// Split a field's post-`name:` remainder into its leading part and the body of
/// a trailing `:states(…)` state-machine modifier (issue #1326), if present.
///
/// `"String:states(a -> b)"` -> `("String", Some("a -> b"))`;
/// `"String:unique:states(a -> b)"` -> `("String:unique", Some("a -> b"))`;
/// a remainder with no such clause yields `(rest, None)`. The clause is
/// paren-delimited and must be the final modifier (the `)` closes the token),
/// mirroring the trailing-modifier style of `:unique` and the `{…}` block.
fn split_state_machine_modifier(rest: &str) -> (&str, Option<&str>) {
    let rest = rest.trim();
    if let Some(pos) = rest.find(":states(")
        && rest.ends_with(')')
    {
        let before = rest[..pos].trim_end();
        let inner = rest[pos + ":states(".len()..rest.len() - 1].trim();
        return (before, Some(inner));
    }
    (rest, None)
}

/// Parse the inner body of a `:states(…)` modifier — a comma-separated list of
/// `from -> to` edges, each with an optional `: guard` plain-identifier suffix
/// (issue #1326). Mirrors the `#[state_machine(transitions(…))]` macro grammar
/// so the generator can re-emit the parsed set verbatim.
///
/// # Errors
/// Returns a human-readable message when the list is empty, an edge is missing
/// its `->`, or any state/guard token is not a valid `snake_case` identifier.
fn parse_state_machine(body: &str) -> Result<StateMachine, String> {
    let mut transitions = Vec::new();
    for segment in body.split(',') {
        let segment = segment.trim();
        // Tolerate a trailing comma (`a -> b,`) exactly like the macro grammar.
        if segment.is_empty() {
            continue;
        }
        let (from, to_and_guard) = segment.split_once("->").ok_or_else(|| {
            format!("malformed transition '{segment}'; expected `from -> to` (missing `->`)")
        })?;
        // Split an optional `: guard` off the right-hand side. The states clause
        // is peeled whole before any `:unique` handling, so this colon is
        // unambiguously the guard separator.
        let (to, guard) = match to_and_guard.split_once(':') {
            Some((to, guard)) => (to.trim(), Some(guard.trim())),
            None => (to_and_guard.trim(), None),
        };
        let from = from.trim();
        validate_state_token(from, "state")?;
        validate_state_token(to, "state")?;
        if let Some(guard) = guard {
            validate_state_token(guard, "guard")?;
        }
        transitions.push(StateTransition {
            from: from.to_owned(),
            to: to.to_owned(),
            guard: guard.map(str::to_owned),
        });
    }
    if transitions.is_empty() {
        return Err(
            "empty `states(…)` modifier; declare at least one `from -> to` transition".to_owned(),
        );
    }
    Ok(StateMachine { transitions })
}

/// Validate one state name or guard name from a `:states(…)` modifier: it must
/// be a non-empty `snake_case` identifier and not a Rust keyword, since the
/// generator emits states as bare identifiers (and guards as method names)
/// inside the `#[state_machine(transitions(…))]` attribute.
fn validate_state_token(token: &str, kind: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err(format!("empty {kind} name in `states(…)` modifier"));
    }
    if !is_valid_ident(token) || is_rust_keyword(token) {
        return Err(format!(
            "'{token}' is not a valid {kind} name; expected a plain snake_case identifier such \
             as `draft` or `can_publish`"
        ));
    }
    Ok(())
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

/// Ensure a reparsed float's shortest-round-trip string is a valid Rust *float*
/// literal for a `#[validate(range(...))]` bound and the HTML5 `min`/`max`
/// attributes: a whole-number value (`1` from `1`/`+1`/`1.0`, or `1000` from
/// `1e3`) must carry a decimal point (`1.0`), since a bare integer literal is
/// not accepted where an `f32`/`f64` bound is expected. Values that already
/// contain `.`/`e`/`E` (`0.5`, `1.5e3`) are left as-is. Input is assumed finite
/// (the caller rejects `inf`/`NaN`).
fn canonical_float_literal(reparsed: &str) -> String {
    if reparsed.contains(['.', 'e', 'E']) {
        reparsed.to_owned()
    } else {
        format!("{reparsed}.0")
    }
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

    // `min`/`max` length bounds apply to every text-shaped column, `richtext`
    // included. The `email`/`url` *format* validators do not: a Markdown body
    // can never satisfy a single-line format rule, so accepting them would emit
    // a field no submission could ever fill (issue #1255).
    let length_bounded = matches!(
        kind,
        FieldKind::String | FieldKind::Text | FieldKind::RichText
    );
    let string_like = matches!(kind, FieldKind::String | FieldKind::Text);
    let numeric = matches!(
        kind,
        FieldKind::I32 | FieldKind::I64 | FieldKind::F32 | FieldKind::F64
    );
    let is_reference = kind.is_reference();
    let is_slug = kind.is_slug();

    for raw in body.split(',') {
        let tok = raw.trim();
        if tok.is_empty() {
            return Err("empty constraint (stray comma) in the `{…}` block".to_owned());
        }
        // `label:col` (references display column) and `from:col` (slug source
        // field) use a colon; every other key/value pair uses `=`. Bare
        // tokens (`email`, `url`) have neither.
        if let Some((key, value)) = tok.split_once('=') {
            parse_constraint_kv(&mut c, key.trim(), value.trim(), kind)?;
        } else if let Some((key, value)) = tok.split_once(':') {
            match key.trim() {
                "label" => set_label_constraint(&mut c, value.trim(), is_reference)?,
                "from" => set_from_constraint(&mut c, value.trim(), is_slug)?,
                "scope" => set_scope_constraint(&mut c, value.trim(), kind.is_position())?,
                // The kind check runs FIRST: on `count:i64{encrypted:bogus}` the
                // useful diagnosis is that `encrypted` doesn't apply to an
                // `i64` at all, not that `bogus` isn't a mode.
                "encrypted" => {
                    reject_encrypted_on_kind(kind)?;
                    reject_repeated_encrypted(&c)?;
                    c.encrypted = Some(EncryptedMode::parse(value.trim())?);
                }
                other => return Err(unknown_constraint_message(other, kind)),
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
                // A bare `{encrypted}` is the safe default mode, exactly like a
                // bare `#[encrypted]` attribute.
                "encrypted" => {
                    reject_encrypted_on_kind(kind)?;
                    reject_repeated_encrypted(&c)?;
                    c.encrypted = Some(EncryptedMode::Randomized);
                }
                // #1384: per-locale content storage. Bare token only — the
                // storage strategy is the framework's to choose, so unlike
                // `{encrypted}` there is no mode to spell.
                "translatable" => {
                    reject_translatable_on_kind(kind)?;
                    if c.translatable {
                        return Err(
                            "the `translatable` modifier is declared more than once — remove \
                             the duplicate"
                                .to_owned(),
                        );
                    }
                    c.translatable = true;
                }
                _ => return Err(unknown_constraint_message(tok, kind)),
            }
        }
    }

    // `email` and `url` are mutually exclusive format validators: emitting both
    // `#[validate(email)]` and `#[validate(url)]` makes the field unwritable (a
    // valid email fails `url` and vice versa), and the HTML5 renderer can only
    // pick one `type`. Reject the pair rather than silently choosing a winner,
    // which would change the author's intent (issue #1388).
    if c.email && c.url {
        return Err(
            "the `email` and `url` constraints are mutually exclusive — a value can't satisfy \
             both; keep only one"
                .to_owned(),
        );
    }

    // Cross-check the combination against the kind: length/range bounds need a
    // string or numeric field, and require min <= max when both are present.
    if (c.min.is_some() || c.max.is_some()) && !length_bounded && !numeric {
        return Err(format!(
            "min/max constraints are not supported for {} fields",
            kind.rust_type()
        ));
    }
    if let (Some(min), Some(max)) = (&c.min, &c.max) {
        // Compare at the field's CONCRETE width, never via `f64`: a large `i64`
        // pair such as `{min=9007199254740993,max=9007199254740992}` would
        // otherwise round to the same float and slip past this check, emitting
        // an impossible `range(min > max)` that rejects every submitted value
        // instead of failing generation here. Each bound already passed
        // `parse_bound` for `kind`, so these re-parses succeed exactly.
        let inverted = match kind {
            FieldKind::String | FieldKind::Text | FieldKind::RichText => {
                matches!((min.parse::<u64>(), max.parse::<u64>()), (Ok(lo), Ok(hi)) if lo > hi)
            }
            FieldKind::I32 | FieldKind::I64 => {
                matches!((min.parse::<i64>(), max.parse::<i64>()), (Ok(lo), Ok(hi)) if lo > hi)
            }
            // `f32`/`f64`: `f64` compares `f32` values exactly, so this is lossless.
            _ => matches!((min.parse::<f64>(), max.parse::<f64>()), (Ok(lo), Ok(hi)) if lo > hi),
        };
        if inverted {
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
        "from" => set_from_constraint(c, value, kind.is_slug()),
        _ => Err(unknown_constraint_message(key, kind)),
    }
}

/// Validate and normalize a `min=`/`max=` bound for `kind`: a length bound on
/// a `String`/`Text` field must be a non-negative integer; a range bound on a
/// numeric field must be a valid number.
fn parse_bound(value: &str, kind: FieldKind) -> Result<String, String> {
    // Every arm returns the CANONICAL reparsed value, never the raw user token:
    // the stored bound is spliced verbatim into both `#[validate(range/length(…))]`
    // and the HTML5 `min`/`max`/`minlength`/`maxlength` attributes, so a token
    // that parses but isn't a valid Rust literal (`.5`, `+1`, `007`, `1.`) would
    // otherwise fail to COMPILE the generated app. Reparsing at the field's
    // concrete type and re-`to_string()`-ing yields a literal that is always
    // valid and value-preserving (issue #1388 follow-up).
    match kind {
        FieldKind::String | FieldKind::Text | FieldKind::RichText => {
            let n = value
                .parse::<u64>()
                .map_err(|_| format!("length bound '{value}' must be a non-negative integer"))?;
            if n > u64::from(u32::MAX) {
                return Err(format!(
                    "length bound '{value}' must be at most {} (HTML length attributes fit a u32)",
                    u32::MAX
                ));
            }
            Ok(n.to_string())
        }
        FieldKind::I32 => {
            // Parse at the field's CONCRETE width, not a wider `i64`: a bound
            // that overflows `i32` (e.g. `count:i32{max=3000000000}`) would
            // otherwise be emitted as `#[validate(range(max = ...))]` on an
            // `i32` field and fail to COMPILE the generated app.
            let n = value.parse::<i32>().map_err(|_| {
                format!(
                    "range bound '{value}' must be an integer within the i32 range ({}..={})",
                    i32::MIN,
                    i32::MAX
                )
            })?;
            Ok(n.to_string())
        }
        FieldKind::I64 => {
            let n = value.parse::<i64>().map_err(|_| {
                format!(
                    "range bound '{value}' must be an integer within the i64 range ({}..={})",
                    i64::MIN,
                    i64::MAX
                )
            })?;
            Ok(n.to_string())
        }
        FieldKind::F32 => {
            // Rust's float parse saturates overflow to ±∞ rather than erroring,
            // so an out-of-`f32`-range literal would compile to a non-finite
            // bound; reject non-finite explicitly, then canonicalize.
            let parsed = value
                .parse::<f32>()
                .map_err(|_| format!("range bound '{value}' must be a number"))?;
            if !parsed.is_finite() {
                return Err(format!(
                    "range bound '{value}' is out of range for an f32 field (it overflows to a non-finite value)"
                ));
            }
            Ok(canonical_float_literal(&parsed.to_string()))
        }
        FieldKind::F64 => {
            let parsed = value
                .parse::<f64>()
                .map_err(|_| format!("range bound '{value}' must be a number"))?;
            if !parsed.is_finite() {
                return Err(format!(
                    "range bound '{value}' is out of range for an f64 field (it overflows to a non-finite value)"
                ));
            }
            Ok(canonical_float_literal(&parsed.to_string()))
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

/// Set the `slug` source-field override (issue #1260), rejecting `from` on a
/// non-`slug` field and a value that isn't a valid `snake_case` identifier.
fn set_from_constraint(c: &mut FieldConstraints, value: &str, is_slug: bool) -> Result<(), String> {
    if !is_slug {
        return Err("the `from` constraint only applies to `slug` fields".to_owned());
    }
    if !is_valid_ident(value) {
        return Err(format!(
            "slug source field '{value}' is not a valid snake_case identifier"
        ));
    }
    c.from = Some(value.to_owned());
    Ok(())
}

/// Set the `position` scope column (issue #1358), rejecting `scope` on a
/// non-`position` field and a value that isn't a valid `snake_case`
/// identifier.
fn set_scope_constraint(
    c: &mut FieldConstraints,
    value: &str,
    is_position: bool,
) -> Result<(), String> {
    if !is_position {
        return Err("the `scope` constraint only applies to `position` fields".to_owned());
    }
    if !is_valid_ident(value) {
        return Err(format!(
            "position scope column '{value}' is not a valid snake_case identifier"
        ));
    }
    c.scope = Some(value.to_owned());
    Ok(())
}

/// Reject a `{encrypted}` modifier on any field kind the `#[encrypted]`
/// attribute macro does not support (issue #1340).
///
/// v1 of the encryption runtime accepts **non-null `String` columns only**
/// (`autumn-macros`' `validate_encrypted_field`): the plaintext is held in
/// memory as a `String` and round-trips through the `RandomizedText` /
/// `DeterministicText` diesel wrappers. So `Bytea`, numerics, `enum{…}`,
/// `decimal{…}`, `references`, `richtext`, `slug` and friends are refused here
/// rather than emitted as a model the macro would then refuse to compile.
/// (The issue's AC text mentions `bytea`; the runtime does not support it, so
/// the DSL says so plainly instead of generating broken code.) The nullability
/// half of that rule is enforced in [`parse_field`], which is where the
/// `Option<…>` wrapper has already been peeled off.
/// Refuse a second `encrypted` modifier in the same `{…}` block (issue #1340).
///
/// The other repeatable modifiers last-win harmlessly (`{min=1,min=2}` is just
/// a sloppier `{min=2}`), but a repeated `encrypted` silently changes the
/// *security* of the column in whichever direction the last token happens to
/// name: `{encrypted,encrypted:deterministic}` quietly upgrades a randomized
/// declaration to deterministic — trading away plaintext-equality hiding — and
/// `{encrypted:deterministic,encrypted}` quietly takes away the equality
/// lookups the author asked for. Neither should be inferred from token order.
fn reject_repeated_encrypted(c: &FieldConstraints) -> Result<(), String> {
    if c.encrypted.is_some() {
        return Err(
            "the `encrypted` modifier is declared more than once — the modes differ in what \
             they leak, so the mode must be unambiguous. Keep exactly one of `encrypted` \
             (randomized) or `encrypted:deterministic`."
                .to_owned(),
        );
    }
    Ok(())
}

fn reject_encrypted_on_kind(kind: FieldKind) -> Result<(), String> {
    if matches!(kind, FieldKind::String | FieldKind::Text) {
        return Ok(());
    }
    // Name the DSL token, not `rust_type()` — `richtext`, `enum{…}` and `slug`
    // all render as `String` in Rust, so reporting the Rust type would produce
    // the self-contradicting "only applies to `String` … Got a `String` field".
    Err(format!(
        "the `encrypted` modifier only applies to `String`/`Text` fields; \
         `#[encrypted]` supports non-null `String` columns in v1 (encrypt structured \
         data before storing it). Got a `{}` field.",
        kind.dsl_token()
    ))
}

/// `{translatable}` stores a JSON container of per-locale values in the
/// column, so the column has to be free-form text. Every other kind either has
/// no text to translate (numbers, timestamps, foreign keys) or is a machine
/// identifier whose value must stay stable across locales (`slug`, `enum`).
fn reject_translatable_on_kind(kind: FieldKind) -> Result<(), String> {
    if matches!(kind, FieldKind::String | FieldKind::Text) {
        return Ok(());
    }
    // Name the DSL token, not `rust_type()` — `richtext`, `enum{…}` and `slug`
    // all render as `String` in Rust (see `reject_encrypted_on_kind`).
    Err(format!(
        "the `translatable` modifier only applies to `String`/`Text` fields; it stores a \
         per-locale JSON container in the column, which needs free-form text. Got a `{}` \
         field.",
        kind.dsl_token()
    ))
}

/// A per-kind "unknown constraint" message that names the offending token and
/// lists what the kind *does* accept (issue #1388 AC5).
fn unknown_constraint_message(token: &str, kind: FieldKind) -> String {
    let accepted = match kind {
        FieldKind::String | FieldKind::Text => {
            "min=N, max=N, email, url, encrypted, encrypted:deterministic, translatable"
        }
        // `RichText` shares the numeric arm's accepted set, not `String`'s: it
        // takes the `min`/`max` length bounds but NOT the `email`/`url` format
        // validators, which a Markdown body could never satisfy (issue #1255).
        FieldKind::RichText | FieldKind::I32 | FieldKind::I64 | FieldKind::F32 | FieldKind::F64 => {
            "min=N, max=N"
        }
        FieldKind::References => "label:col",
        FieldKind::Slug => "from:col",
        FieldKind::Position => "scope:col",
        _ => "(none — this field type takes no constraint modifiers)",
    };
    // Reported by the DSL token the author actually typed (`commentable`),
    // not by the Rust type it happens to lower to (`i64`) — the latter reads
    // like a different field entirely.
    format!(
        "unknown constraint '{token}' for `{}` fields; supported: {accepted}",
        kind.dsl_token()
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
/// Each token is `name:Type` with optional trailing modifiers: `:unique`
/// (issue #1032), a `{…}` constraint block (issue #1388 / #1146), and — for a
/// non-nullable `String`/`Text` field — a `:states(…)` state-machine modifier
/// (issue #1326). The state-machine clause lists `from -> to` edges, each with
/// an optional `: guard` method name, e.g.
/// `status:String:states(draft -> published: can_publish, published -> archived)`.
/// It re-emits as a `#[state_machine(transitions(…))]` attribute on the field.
///
/// # Errors
/// Bubbles up the first failed token, rejects duplicate field names —
/// emitting two entries with the same column name would produce duplicate
/// struct members and duplicate SQL columns — and (issue #1260) validates
/// every `slug{from:...}` field's cross-field constraints: at most one slug
/// field per model (it's the routing key), and its `from` must name a
/// declared `String`/`Text`/`richtext` field (declaration order doesn't
/// matter — a slug may derive from a field declared later in the list).
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
    validate_slug_fields(tokens, &fields)?;
    validate_position_fields(tokens, &fields)?;
    validate_commentable_fields(tokens, &fields)?;
    Ok(fields)
}

/// Cross-field validation for every `position`/`position{scope:...}` field in
/// `fields` (issue #1358) — see [`parse_fields`]. Runs after every token has
/// parsed individually, since `scope`'s target may be declared earlier OR
/// later in the token list.
fn validate_position_fields(tokens: &[String], fields: &[Field]) -> Result<(), GenerateError> {
    let position_fields: Vec<(usize, &Field)> = fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.kind.is_position())
        .collect();
    if let [(_, first), (second_idx, second), ..] = position_fields[..] {
        return Err(GenerateError::InvalidField {
            token: tokens[second_idx].clone(),
            reason: format!(
                "only one `position` field is supported per model — found both '{}' and '{}'",
                first.name, second.name
            ),
        });
    }
    for (idx, position_field) in &position_fields {
        let Some(scope) = position_field.constraints.scope.as_deref() else {
            continue;
        };
        let token = tokens[*idx].clone();
        let Some(source) = fields.iter().find(|f| f.name == scope) else {
            return Err(GenerateError::InvalidField {
                token,
                reason: format!(
                    "position field '{}' is scoped `scope:{scope}`, but no field named \
                     '{scope}' is declared",
                    position_field.name
                ),
            });
        };
        if !source.kind.is_reference() {
            return Err(GenerateError::InvalidField {
                token,
                reason: format!(
                    "position field '{}' is scoped `scope:{scope}`, but '{scope}' is a {} \
                     field — position can only be scoped to a `references` foreign key",
                    position_field.name,
                    source.rust_type()
                ),
            });
        }
        // A nullable scope FK renders as `Option<i64>` at the Rust level,
        // but the generated triggers/`move_to` compare the scope column via
        // Postgres `hashtext(NEW."{scope}"::text)` / `"{scope}" = $1`
        // equality, both of which are SQL-NULL-strict: a NULL scope value
        // hashes to NULL (taking no advisory lock at all) and never equals
        // itself in a `WHERE` filter, so every NULL-scoped row would
        // silently get position 0 with no working compaction. Reject the
        // combination outright rather than emit ordering that quietly
        // breaks the moment a scope value is unset.
        if source.nullable {
            return Err(GenerateError::InvalidField {
                token,
                reason: format!(
                    "position field '{}' is scoped `scope:{scope}`, but '{scope}' is nullable \
                     (`Option<…>`) — a NULL scope value can't be locked or matched by the \
                     generated triggers/`move_to`. Make '{scope}' non-nullable, or drop the \
                     `references` field's `:null`.",
                    position_field.name
                ),
            });
        }
    }
    Ok(())
}

/// Cross-field validation for `commentable` fields (issue #1367) — see
/// [`parse_fields`].
///
/// At most one per model: a second token would mean a second counter column
/// *and* a second `#[commentable]` attribute, and `#[model]` rejects the
/// latter outright (both would generate the same `{Model}Comments` trait). The
/// DSL refuses first so the error names the token rather than surfacing as a
/// macro error in generated code the author did not write.
fn validate_commentable_fields(tokens: &[String], fields: &[Field]) -> Result<(), GenerateError> {
    let commentable: Vec<(usize, &Field)> = fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.kind.is_commentable())
        .collect();
    if let [(_, first), (second_idx, second), ..] = commentable[..] {
        return Err(GenerateError::InvalidField {
            token: tokens[second_idx].clone(),
            reason: format!(
                "only one `commentable` field is supported per model — found both '{}' and \
                 '{}'. One shared comments table already attaches to every model; a second \
                 token would add a second counter column and a second `#[commentable]` \
                 attribute, which `#[model]` rejects.",
                first.name, second.name
            ),
        });
    }
    Ok(())
}

/// Cross-field validation for every `slug{from:...}` field in `fields`
/// (issue #1260) — see [`parse_fields`]. Runs after every token has parsed
/// individually, since `from`'s target may be declared earlier OR later in
/// the token list.
fn validate_slug_fields(tokens: &[String], fields: &[Field]) -> Result<(), GenerateError> {
    // `parse_fields` builds `fields` from `tokens` one-to-one with no skips,
    // so `tokens[i]` is always the token that produced `fields[i]` — no need
    // to rediscover the index via a linear name search below.
    let slug_fields: Vec<(usize, &Field)> = fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.kind.is_slug())
        .collect();
    if let [(_, first), (second_idx, second), ..] = slug_fields[..] {
        return Err(GenerateError::InvalidField {
            token: tokens[second_idx].clone(),
            reason: format!(
                "only one `slug` field is supported per model (it's the routing key) — \
                 found both '{}' and '{}'",
                first.name, second.name
            ),
        });
    }
    for (idx, slug_field) in &slug_fields {
        // Presence of `constraints.from` is already enforced per-token in
        // `parse_field`; this is the cross-field half of that check.
        let from = slug_field
            .constraints
            .from
            .as_deref()
            .expect("parse_field rejects a slug field with no `from` constraint");
        let token = tokens[*idx].clone();
        let Some(source) = fields.iter().find(|f| f.name == from) else {
            return Err(GenerateError::InvalidField {
                token,
                reason: format!(
                    "slug field '{}' derives `from:{from}`, but no field named '{from}' is \
                     declared",
                    slug_field.name
                ),
            });
        };
        if !matches!(
            source.kind,
            FieldKind::String | FieldKind::Text | FieldKind::RichText
        ) {
            return Err(GenerateError::InvalidField {
                token,
                reason: format!(
                    "slug field '{}' derives `from:{from}`, but '{from}' is a {} field — \
                     slug can only derive from a String/Text/richtext field",
                    slug_field.name,
                    source.rust_type()
                ),
            });
        }
        // Issue #1340: a slug is derived by `slugify`ing the source value and
        // is then stored in its OWN plaintext, UNIQUE-indexed column *and* used
        // as the record's route key. Deriving one from an `{encrypted}` column
        // would therefore write a lightly-mangled copy of the secret to disk in
        // the clear (defeating the encryption entirely), leak plaintext
        // equality through the unique index, and put it in URL paths — where it
        // reaches access logs, `Referer` headers, browser history, and proxy
        // caches. Refuse the pair outright; there is no mode of encryption that
        // makes it safe.
        if source.is_encrypted() {
            return Err(GenerateError::InvalidField {
                token,
                reason: format!(
                    "slug field '{}' derives `from:{from}`, but '{from}' is `{{encrypted}}`: \
                     the slug is stored in its own PLAINTEXT column and used as the record's \
                     URL, so deriving it from an encrypted column would write the secret to \
                     disk in the clear and put it in every request path. Derive the slug from \
                     a non-encrypted field instead.",
                    slug_field.name
                ),
            });
        }
        // A nullable source renders as `Option<String>` on the generated
        // `New*` insert struct, but `autumn_web::slugify` takes `&str` — the
        // create handler's `slugify(&new.{from})` would not compile.
        if source.nullable {
            return Err(GenerateError::InvalidField {
                token,
                reason: format!(
                    "slug field '{}' derives `from:{from}`, but '{from}' is nullable \
                     (`Option<…>`) — slug can only derive from a non-nullable field",
                    slug_field.name
                ),
            });
        }
    }
    Ok(())
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
        // richtext (issue #1255): a TEXT column holding user-submitted Markdown
        // that the generated views render through the sanitizing
        // `markdown::render_user_content`. All three spellings are accepted so
        // the token reads naturally however the author types it.
        "richtext" | "RichText" | "rich_text" => Some(FieldKind::RichText),
        // slug (issue #1260): a URL-safe routing key auto-derived from
        // another field, e.g. `slug:slug{from:title}`.
        "slug" | "Slug" => Some(FieldKind::Slug),
        // json / jsonb (issue #1341): arbitrary structured data stored as
        // `serde_json::Value`. `jsonb` is accepted as an alias per the AC —
        // both spellings (and their PascalCase forms, consistent with
        // `Attachment`/`attachment` and `decimal`/`Decimal`) map to the same
        // kind; there is no separate non-binary "json" storage distinction on
        // the Postgres side (the column is always `JSONB`).
        "json" | "Json" | "jsonb" | "Jsonb" => Some(FieldKind::Json),
        // position (issue #1358): a server-managed contiguous ordering
        // column, e.g. `rank:position` or `rank:position{scope:board_id}`.
        "position" | "Position" => Some(FieldKind::Position),
        "commentable" | "Commentable" => Some(FieldKind::Commentable),
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

    // ── #1384: `{translatable}` modifier ────────────────────────────────────

    #[test]
    fn translatable_modifier_parses_on_string_and_text() {
        for token in ["title:String{translatable}", "body:Text{translatable}"] {
            let f = parse_field(token).unwrap_or_else(|e| panic!("{token}: {e}"));
            assert!(f.is_translatable(), "{token}");
            // The Rust type becomes the per-locale container...
            assert_eq!(f.rust_type(), "autumn_web::i18n::Translated");
            // ...while the column stays a portable TEXT holding a JSON object.
            assert_eq!(f.sql_column_type(), "TEXT");
            assert_eq!(f.schema_type(), "Text");
            assert_eq!(f.sql_nullability(), "NOT NULL");
            assert_eq!(f.sql_default(), Some("'{}'"));
        }
    }

    #[test]
    fn translatable_modifier_is_rejected_on_non_text_kinds() {
        for token in [
            "count:i64{translatable}",
            "at:NaiveDateTime{translatable}",
            "post:references{translatable}",
        ] {
            let err = parse_field(token).unwrap_err().to_string();
            assert!(
                err.contains("translatable"),
                "{token} should name the modifier: {err}"
            );
        }
    }

    #[test]
    fn translatable_is_rejected_on_a_nullable_column() {
        // The container already models "no translation" as an empty map.
        let err = parse_field("title:Option<String>{translatable}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("translatable"), "{err}");
    }

    #[test]
    fn translatable_and_encrypted_are_mutually_exclusive() {
        let err = parse_field("title:String{translatable,encrypted}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("encrypted"), "{err}");
    }

    #[test]
    fn translatable_is_rejected_with_unique() {
        // A UNIQUE index over a JSON container compares whole containers.
        let err = parse_field("title:String:unique{translatable}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("translatable"), "{err}");
    }

    #[test]
    fn translatable_is_rejected_with_a_state_machine() {
        let err = parse_field("status:String{translatable}:states(draft -> live)")
            .unwrap_err()
            .to_string();
        assert!(err.contains("translatable"), "{err}");
    }

    #[test]
    fn a_repeated_translatable_modifier_is_rejected() {
        let err = parse_field("title:String{translatable,translatable}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("translatable"), "{err}");
    }

    #[test]
    fn non_translatable_fields_keep_their_type_and_have_no_default() {
        let f = parse_field("title:String").unwrap();
        assert!(!f.is_translatable());
        assert_eq!(f.rust_type(), "String");
        assert_eq!(f.sql_default(), None);
    }

    #[test]
    fn unknown_constraint_message_advertises_translatable() {
        let err = parse_field("title:String{bogus}").unwrap_err().to_string();
        assert!(err.contains("translatable"), "{err}");
    }

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

    // ── SQLite backend-aware column mapping (issue #1614) ──────────────

    /// The complete `SQLite` column-type mapping for every scalar `FieldKind`.
    /// AC #4: every kind maps to a working `SQLite` type — none is rejected.
    #[test]
    fn sqlite_sql_type_covers_every_kind() {
        let cases = [
            ("title:String", "TEXT"),
            ("body:Text", "TEXT"),
            ("count:i32", "INTEGER"),
            ("big:i64", "INTEGER"),
            ("flag:bool", "INTEGER"),
            ("ratio:f32", "REAL"),
            ("amount:f64", "REAL"),
            ("token:Uuid", "TEXT"),
            ("naive:NaiveDateTime", "TEXT"),
            ("at:DateTime", "TEXT"),
            ("data:Bytea", "BLOB"),
            ("cover:Attachment", "TEXT"),
            ("post:references", "INTEGER"),
        ];
        for (token, expected) in cases {
            let f = parse_field(token).unwrap();
            assert_eq!(
                f.sql_column_type_for(DatabaseBackend::Sqlite),
                expected,
                "SQLite column type for `{token}`"
            );
            // Postgres path must stay byte-for-byte identical to the legacy output.
            assert_eq!(
                f.sql_column_type_for(DatabaseBackend::Postgres),
                f.sql_column_type(),
                "Postgres column type for `{token}` must be unchanged"
            );
        }
    }

    #[test]
    fn sqlite_enum_and_decimal_map_to_text() {
        let e = parse_field("status:enum{draft,published}").unwrap();
        assert_eq!(e.sql_column_type_for(DatabaseBackend::Sqlite), "TEXT");
        assert_eq!(e.sql_column_type_for(DatabaseBackend::Postgres), "TEXT");

        // Decimal collapses to TEXT on SQLite (no fixed-precision NUMERIC),
        // while Postgres keeps the exact `NUMERIC(precision,scale)` rendering.
        let d = parse_field("price:decimal{10,2}").unwrap();
        assert_eq!(d.sql_column_type_for(DatabaseBackend::Sqlite), "TEXT");
        assert_eq!(
            d.sql_column_type_for(DatabaseBackend::Postgres),
            "NUMERIC(10,2)"
        );
    }

    /// The diesel `schema.rs` types on the `SQLite` path must be types diesel's
    /// `SQLite` backend implements — never the Postgres-only `Timestamptz`,
    /// `Jsonb`, `Uuid`, `Bytea`, or `Numeric`. `DateTime<Utc>` maps to the
    /// `SQLite`-valid `TimestamptzSqlite` (issue #1924).
    #[test]
    fn sqlite_schema_type_avoids_postgres_only_diesel_types() {
        let cases = [
            // `NaiveDateTime` maps to the core, ungated `Timestamp`; `DateTime`
            // maps to diesel's SQLite `TimestamptzSqlite` (issue #1924).
            ("at:DateTime", "TimestamptzSqlite"),
            ("naive:NaiveDateTime", "Timestamp"),
            ("token:Uuid", "Text"),
            // `Attachment` fields are auto-nullable, so `Field::schema_type_for`
            // wraps the SQLite `Text` inner type in `Nullable<…>`.
            ("cover:Attachment", "Nullable<Text>"),
            ("data:Bytea", "Binary"),
            ("price:decimal{10,2}", "Text"),
            ("big:i64", "Int8"),
            ("flag:bool", "Bool"),
        ];
        for (token, expected) in cases {
            let f = parse_field(token).unwrap();
            assert_eq!(
                f.schema_type_for(DatabaseBackend::Sqlite),
                expected,
                "SQLite schema type for `{token}`"
            );
        }
        // Nullable wrapping is preserved on the SQLite path.
        let opt = parse_field("token:Option<Uuid>").unwrap();
        assert_eq!(
            opt.schema_type_for(DatabaseBackend::Sqlite),
            "Nullable<Text>"
        );
        // No SQLite schema type may be a Postgres-only diesel type.
        // `TimestamptzSqlite` is deliberately NOT forbidden — it is a diesel
        // *SQLite* sql-type (issue #1924), not a Postgres-only one.
        for token in [
            "at:DateTime",
            "token:Uuid",
            "cover:Attachment",
            "data:Bytea",
        ] {
            let ty = parse_field(token)
                .unwrap()
                .schema_type_for(DatabaseBackend::Sqlite);
            assert!(
                !["Timestamptz", "Jsonb", "Uuid", "Bytea", "Numeric"].contains(&ty.as_str()),
                "`{token}` -> `{ty}` must not be a Postgres-only diesel type on SQLite"
            );
        }
    }

    /// Issue #1924 closes the last three gaps — `Uuid` and `Decimal` through
    /// `autumn-web`'s `TEXT`-backed newtypes, `Enum` through the app-local
    /// `Text`/`Sqlite` impls the model generator emits — so every DSL field
    /// kind now has a working diesel `SQLite` conversion.
    #[test]
    fn every_field_kind_has_a_sqlite_diesel_conversion() {
        for token in [
            "title:String",
            "body:Text",
            "rich:richtext",
            "count:i32",
            "big:i64",
            "flag:bool",
            "ratio:f32",
            "amount:f64",
            "naive:NaiveDateTime",
            "at:DateTime",
            "cover:Attachment",
            "data:Bytea",
            "post:references",
            "payload:json",
            "token:Uuid",
            "price:decimal{10,2}",
            "status:enum{draft,published}",
        ] {
            assert!(
                parse_field(token)
                    .unwrap()
                    .kind
                    .sqlite_has_diesel_conversion(),
                "`{token}` must be a SQLite-supported kind"
            );
        }
    }

    /// The two foreign types render `autumn-web`'s `SQLite` newtypes; every
    /// other kind — and all of Postgres — is byte-for-byte unchanged (#1924).
    #[test]
    fn sqlite_rust_types_wrap_only_uuid_and_decimal() {
        let cases = [
            ("token:Uuid", SQLITE_UUID_RUST_TYPE.to_owned()),
            ("price:decimal{10,2}", SQLITE_DECIMAL_RUST_TYPE.to_owned()),
            (
                "owner:Option<Uuid>",
                format!("Option<{SQLITE_UUID_RUST_TYPE}>"),
            ),
            ("title:String", "String".to_owned()),
            ("at:DateTime", "chrono::DateTime<chrono::Utc>".to_owned()),
            (
                "cover:Attachment",
                "Option<autumn_web::storage::Blob>".to_owned(),
            ),
        ];
        for (token, expected) in cases {
            let f = parse_field(token).unwrap();
            assert_eq!(
                f.rust_type_for(DatabaseBackend::Sqlite),
                expected,
                "SQLite Rust type for `{token}`"
            );
            assert_eq!(
                f.rust_type_for(DatabaseBackend::Postgres),
                f.rust_type(),
                "Postgres Rust type for `{token}` must be unchanged"
            );
        }
        // An enum field keeps its generated type name on both backends — the
        // enum is local to the app, so it needs no wrapper.
        let e = parse_field("status:enum{draft,published}").unwrap();
        assert_eq!(e.rust_type_for(DatabaseBackend::Sqlite), e.rust_type());
    }

    #[test]
    fn sqlite_primary_key_mapping() {
        assert_eq!(
            IdType::BigSerial.pk_sql_for(DatabaseBackend::Sqlite),
            "INTEGER PRIMARY KEY AUTOINCREMENT"
        );
        assert_eq!(
            IdType::Uuid.pk_sql_for(DatabaseBackend::Sqlite),
            "TEXT PRIMARY KEY"
        );
        // Postgres primary keys stay byte-for-byte identical.
        assert_eq!(
            IdType::BigSerial.pk_sql_for(DatabaseBackend::Postgres),
            IdType::BigSerial.pk_sql()
        );
        assert_eq!(
            IdType::Uuid.pk_sql_for(DatabaseBackend::Postgres),
            IdType::Uuid.pk_sql()
        );
        // The UUIDv7 migration comment is Postgres-only guidance.
        assert!(
            IdType::Uuid
                .migration_comment_for(DatabaseBackend::Postgres)
                .is_some()
        );
        assert!(
            IdType::Uuid
                .migration_comment_for(DatabaseBackend::Sqlite)
                .is_none()
        );
        assert_eq!(
            IdType::BigSerial.schema_type_for(DatabaseBackend::Sqlite),
            "Int8"
        );
        assert_eq!(
            IdType::Uuid.schema_type_for(DatabaseBackend::Sqlite),
            "Text"
        );
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

    // ── slug cross-field validation (issue #1260) ──────────────────────────

    #[test]
    fn parse_fields_accepts_slug_deriving_from_earlier_string_field() {
        let tokens = vec!["title:String".into(), "slug:slug{from:title}".into()];
        let fs = parse_fields(&tokens).unwrap();
        assert_eq!(fs[1].constraints.from.as_deref(), Some("title"));
    }

    #[test]
    fn parse_fields_accepts_slug_deriving_from_later_string_field() {
        // Declaration order shouldn't matter for the `from` reference.
        let tokens = vec!["slug:slug{from:title}".into(), "title:String".into()];
        assert!(parse_fields(&tokens).is_ok());
    }

    #[test]
    fn parse_fields_rejects_slug_from_unknown_field() {
        let tokens = vec!["slug:slug{from:headline}".into()];
        let err = parse_fields(&tokens).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("headline"), "unexpected error: {msg}");
    }

    #[test]
    fn parse_fields_rejects_slug_from_non_string_field() {
        let tokens = vec!["count:i32".into(), "slug:slug{from:count}".into()];
        let err = parse_fields(&tokens).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("count"), "unexpected error: {msg}");
    }

    #[test]
    fn parse_fields_rejects_slug_from_nullable_source_field() {
        // `new.{from}` would be `Option<String>` on the generated `New*`
        // insert struct — `autumn_web::slugify(&new.{from})` doesn't compile
        // against that type, so this must be rejected at generate time
        // rather than emit uncompilable code.
        let tokens = vec![
            "title:Option<String>".into(),
            "slug:slug{from:title}".into(),
        ];
        let err = parse_fields(&tokens).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("title") && msg.contains("nullable"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn parse_fields_accepts_slug_deriving_from_text_field() {
        let tokens = vec!["body:Text".into(), "slug:slug{from:body}".into()];
        assert!(parse_fields(&tokens).is_ok());
    }

    #[test]
    fn parse_fields_accepts_slug_deriving_from_richtext_field() {
        let tokens = vec!["body:richtext".into(), "slug:slug{from:body}".into()];
        assert!(parse_fields(&tokens).is_ok());
    }

    #[test]
    fn parse_fields_rejects_more_than_one_slug_field() {
        // A slug is the model's routing key -- only one makes sense.
        let tokens = vec![
            "title:String".into(),
            "slug:slug{from:title}".into(),
            "slug2:slug{from:title}".into(),
        ];
        let err = parse_fields(&tokens).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("slug"), "unexpected error: {msg}");
    }

    // ── position cross-field validation (issue #1358) ───────────────────────

    #[test]
    fn parse_fields_accepts_position_scoped_to_earlier_reference_field() {
        let tokens = vec![
            "board:references".into(),
            "rank:position{scope:board_id}".into(),
        ];
        let fs = parse_fields(&tokens).unwrap();
        assert_eq!(fs[1].constraints.scope.as_deref(), Some("board_id"));
    }

    #[test]
    fn parse_fields_accepts_position_scoped_to_later_reference_field() {
        // Declaration order shouldn't matter for the `scope` reference.
        let tokens = vec![
            "rank:position{scope:board_id}".into(),
            "board:references".into(),
        ];
        assert!(parse_fields(&tokens).is_ok());
    }

    #[test]
    fn parse_fields_rejects_position_scope_unknown_field() {
        let tokens = vec!["rank:position{scope:board_id}".into()];
        let err = parse_fields(&tokens).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("board_id"), "unexpected error: {msg}");
    }

    #[test]
    fn parse_fields_rejects_position_scope_non_reference_field() {
        let tokens = vec![
            "board_id:i64".into(),
            "rank:position{scope:board_id}".into(),
        ];
        let err = parse_fields(&tokens).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("board_id"), "unexpected error: {msg}");
    }

    #[test]
    fn parse_fields_rejects_position_scope_nullable_reference_field() {
        let tokens = vec![
            "board:references?".into(),
            "rank:position{scope:board_id}".into(),
        ];
        let err = parse_fields(&tokens).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("board_id"), "unexpected error: {msg}");
        assert!(msg.contains("nullable"), "unexpected error: {msg}");
    }

    #[test]
    fn parse_fields_rejects_more_than_one_position_field() {
        let tokens = vec!["rank:position".into(), "rank2:position".into()];
        let err = parse_fields(&tokens).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("position"), "unexpected error: {msg}");
    }

    #[test]
    fn parse_fields_accepts_unscoped_position_field() {
        let tokens = vec!["rank:position".into()];
        let fs = parse_fields(&tokens).unwrap();
        assert_eq!(fs[0].constraints.scope, None);
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

    // ── richtext (issue #1255) ─────────────────────────────────────────────

    #[test]
    fn parse_richtext_token() {
        let f = parse_field("body:richtext").unwrap();
        assert_eq!(f.kind, FieldKind::RichText);
        assert!(!f.nullable);
        assert_eq!(f.name, "body");
    }

    #[test]
    fn parse_richtext_accepts_documented_spellings() {
        for token in ["body:richtext", "body:RichText", "body:rich_text"] {
            assert_eq!(
                parse_field(token).unwrap().kind,
                FieldKind::RichText,
                "{token} should parse as RichText"
            );
        }
    }

    #[test]
    fn richtext_stores_markdown_source_in_a_text_column() {
        // The column holds the Markdown *source*, not rendered HTML — so it is
        // an ordinary TEXT/String column everywhere in the storage stack.
        let f = parse_field("body:richtext").unwrap();
        assert_eq!(f.rust_type(), "String");
        assert_eq!(f.sql_type(), "TEXT");
        assert_eq!(f.schema_type(), "Text");
        assert_eq!(
            f.sql_column_type_for(DatabaseBackend::Sqlite),
            "TEXT",
            "richtext must work on the SQLite backend too"
        );
        assert_eq!(f.schema_type_for(DatabaseBackend::Sqlite), "Text");
        assert!(FieldKind::RichText.sqlite_has_diesel_conversion());
    }

    #[test]
    fn optional_richtext_parses() {
        let f = parse_field("body:Option<richtext>").unwrap();
        assert_eq!(f.kind, FieldKind::RichText);
        assert!(f.nullable);
        assert_eq!(f.rust_type(), "Option<String>");
        assert_eq!(f.schema_type(), "Nullable<Text>");
    }

    #[test]
    fn richtext_is_rich_text_predicate() {
        assert!(FieldKind::RichText.is_rich_text());
        assert!(!FieldKind::Text.is_rich_text());
        assert!(!FieldKind::String.is_rich_text());
    }

    #[test]
    fn richtext_accepts_length_constraints() {
        let f = parse_field("body:richtext{min=10,max=50000}").unwrap();
        assert_eq!(f.validation_attrs(), vec!["length(min = 10, max = 50000)"]);
    }

    #[test]
    fn richtext_rejects_format_constraints() {
        // `email`/`url` are single-line format validators; a Markdown body can
        // never satisfy them, so accepting them would emit an unwritable field.
        for token in ["body:richtext{email}", "body:richtext{url}"] {
            let err = parse_field(token).unwrap_err().to_string();
            assert!(
                err.contains("email") || err.contains("url"),
                "{token} should be rejected, got: {err}"
            );
        }
    }

    #[test]
    fn richtext_rejects_state_machine_modifier() {
        let err = parse_field("body:richtext:states(draft -> live)")
            .unwrap_err()
            .to_string();
        assert!(err.contains("state machine"), "{err}");
    }

    #[test]
    fn richtext_supports_unique_modifier_like_other_text_columns() {
        let f = parse_field("body:richtext:unique").unwrap();
        assert_eq!(f.kind, FieldKind::RichText);
        assert!(f.unique);
    }

    #[test]
    fn richtext_appears_in_supported_types_constants() {
        assert!(
            SUPPORTED_TYPES.contains("richtext"),
            "SUPPORTED_TYPES must list richtext"
        );
        assert!(
            SQLITE_SUPPORTED_KINDS.contains("richtext"),
            "SQLITE_SUPPORTED_KINDS must list richtext — it is a plain TEXT column"
        );
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

    // ── RED: json/jsonb field kind (issue #1341) ────────────────────────────

    #[test]
    fn parse_json_lowercase() {
        let f = parse_field("config:json").unwrap();
        assert_eq!(f.kind, FieldKind::Json);
        assert_eq!(f.name, "config");
        assert!(
            !f.nullable,
            "unlike Attachment, a json field is not implicitly nullable"
        );
    }

    #[test]
    fn parse_json_pascal_case() {
        assert_eq!(parse_field("config:Json").unwrap().kind, FieldKind::Json);
    }

    #[test]
    fn parse_jsonb_alias_lowercase() {
        assert_eq!(parse_field("config:jsonb").unwrap().kind, FieldKind::Json);
    }

    #[test]
    fn parse_jsonb_alias_pascal_case() {
        assert_eq!(parse_field("config:Jsonb").unwrap().kind, FieldKind::Json);
    }

    #[test]
    fn json_rust_type_is_serde_json_value() {
        let f = parse_field("config:json").unwrap();
        assert_eq!(f.rust_type(), "serde_json::Value");
    }

    #[test]
    fn json_sql_type_is_jsonb() {
        let f = parse_field("config:json").unwrap();
        assert_eq!(f.sql_type(), "JSONB");
    }

    #[test]
    fn json_schema_type_is_jsonb() {
        let f = parse_field("config:json").unwrap();
        assert_eq!(f.schema_type(), "Jsonb");
    }

    #[test]
    fn json_sqlite_sql_type_is_text() {
        // AC1: "SQLite dev/test backend: TEXT storing JSON".
        let f = parse_field("config:json").unwrap();
        assert_eq!(f.sql_column_type_for(DatabaseBackend::Sqlite), "TEXT");
    }

    #[test]
    fn json_sqlite_schema_type_is_diesels_json_not_text_or_jsonb() {
        // Diesel ships `FromSql`/`ToSql<Json, Sqlite> for serde_json::Value`
        // (diesel 2.3+, `serde_json` + `sqlite` features — both already
        // unconditionally on in this workspace). Using diesel's *bare* `Text`
        // sql-type here would not compile (no `serde_json::Value` conversion
        // for it), and diesel's `Jsonb` on SQLite uses a proprietary binary
        // encoding, not the plain-text JSON the AC calls for — so this must be
        // `Json`, distinct from both.
        let f = parse_field("config:json").unwrap();
        assert_eq!(f.schema_type_for(DatabaseBackend::Sqlite), "Json");
    }

    #[test]
    fn json_sqlite_has_diesel_conversion_is_true() {
        // Unlike `Decimal`/`Uuid`/`Enum`, `serde_json::Value`'s SQLite
        // conversion is implemented by diesel itself, so no orphan-rule
        // wrapper is needed and the kind is fully SQLite-supported.
        assert!(FieldKind::Json.sqlite_has_diesel_conversion());
    }

    #[test]
    fn optional_json_parses() {
        let f = parse_field("config:Option<json>").unwrap();
        assert_eq!(f.kind, FieldKind::Json);
        assert!(f.nullable);
        assert_eq!(f.rust_type(), "Option<serde_json::Value>");
        assert_eq!(f.schema_type(), "Nullable<Jsonb>");
        assert_eq!(f.schema_type_for(DatabaseBackend::Sqlite), "Nullable<Json>");
    }

    #[test]
    fn optional_jsonb_alias_parses() {
        let f = parse_field("config:Option<jsonb>").unwrap();
        assert_eq!(f.kind, FieldKind::Json);
        assert!(f.nullable);
    }

    #[test]
    fn unique_json_field_parses() {
        // Postgres's `jsonb` type has a default B-tree operator class, so a
        // `UNIQUE INDEX` over a json column is legitimate — not rejected the
        // way a randomized-encrypted column's equality lookup is.
        let f = parse_field("config:json:unique").unwrap();
        assert_eq!(f.kind, FieldKind::Json);
        assert!(f.unique);
    }

    #[test]
    fn json_in_list_of_fields() {
        let tokens = vec![
            "title:String".into(),
            "config:json".into(),
            "count:i64".into(),
        ];
        let fields = parse_fields(&tokens).unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[1].kind, FieldKind::Json);
    }

    #[test]
    fn json_appears_in_supported_types_constant() {
        assert!(
            SUPPORTED_TYPES.contains("json"),
            "SUPPORTED_TYPES must list json/jsonb"
        );
    }

    #[test]
    fn json_appears_in_sqlite_supported_kinds_constant() {
        // Unlike Decimal/Uuid/Enum, json IS SQLite-supported.
        assert!(
            SQLITE_SUPPORTED_KINDS.contains("json"),
            "SQLITE_SUPPORTED_KINDS must list json — diesel supports it natively"
        );
    }

    #[test]
    fn json_dsl_token_is_json() {
        assert_eq!(FieldKind::Json.dsl_token(), "json");
    }

    #[test]
    fn json_stays_unmapped_for_db_pull_inverse() {
        // Same ambiguity as Attachment (both forward-map to `jsonb`): the
        // inverse can't tell a `Blob` column from a raw `Json` column, so
        // `db pull` introspection leaves `jsonb`/`json` unsupported rather
        // than guessing (see `sql_type_inverse_rejects_unknown_types`, which
        // already locks this and is unchanged by this feature).
        assert!(sql_type_to_field_kind("jsonb").is_none());
        assert!(sql_type_to_field_kind("json").is_none());
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
    fn email_and_url_together_are_rejected() {
        // Both format validators on one field make it unwritable (a valid email
        // fails `url` and vice versa), so the pair is rejected at parse time
        // rather than silently picking a winner.
        let err = parse_field("contact:String{email,url}").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mutually exclusive"),
            "must explain the conflict: {msg}"
        );
        assert!(msg.contains("contact"), "must name the field: {msg}");
    }

    #[test]
    fn parse_numeric_range_constraint() {
        let f = parse_field("age:i32{min=0,max=130}").unwrap();
        assert_eq!(f.kind, FieldKind::I32);
        assert_eq!(f.validation_attrs(), vec!["range(min = 0, max = 130)"]);
    }

    #[test]
    fn i32_range_bound_within_range_is_accepted() {
        let f = parse_field("count:i32{max=1000000}").unwrap();
        assert_eq!(f.kind, FieldKind::I32);
        assert_eq!(f.constraints.max.as_deref(), Some("1000000"));
        assert_eq!(f.validation_attrs(), vec!["range(max = 1000000)"]);
    }

    #[test]
    fn i32_range_bound_exceeding_i32_max_is_rejected() {
        // `3000000000` > i32::MAX (~2.147e9): parsed at the field's concrete
        // width so it fails generation with an actionable error rather than
        // emitting `#[validate(range(max = 3000000000))]` on an `i32` field,
        // which would fail to COMPILE the generated app.
        let err = parse_field("count:i32{max=3000000000}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("3000000000"), "must name the bad bound: {msg}");
        assert!(msg.contains("i32"), "must name the field type: {msg}");
        assert!(
            msg.contains("2147483647"),
            "must state the valid range: {msg}"
        );
    }

    #[test]
    fn i64_accepts_a_bound_that_would_overflow_i32() {
        // The same literal is fine on an i64 field.
        let f = parse_field("count:i64{max=3000000000}").unwrap();
        assert_eq!(f.kind, FieldKind::I64);
        assert_eq!(f.validation_attrs(), vec!["range(max = 3000000000)"]);
    }

    #[test]
    fn f32_range_bound_overflowing_the_type_is_rejected() {
        // Rust parses an out-of-f32-range literal to +∞ rather than erroring,
        // so reject non-finite bounds explicitly.
        let err = parse_field("ratio:f32{max=1e40}").unwrap_err();
        assert!(err.to_string().contains("f32"), "{}", err);
    }

    #[test]
    fn i64_min_greater_than_max_beyond_f64_precision_is_rejected() {
        // `min` is exactly one greater than `max`, but both round to the same
        // `f64` (they exceed 2^53). Comparing at the concrete `i64` width must
        // still catch the inversion, rather than emitting an impossible
        // `range(min > max)` that rejects every submitted value.
        let err = parse_field("count:i64{min=9007199254740993,max=9007199254740992}").unwrap_err();
        assert!(
            err.to_string().contains("cannot be greater than"),
            "must report the inverted range: {err}"
        );
    }

    #[test]
    fn i64_valid_large_range_is_accepted() {
        let f = parse_field("count:i64{min=1,max=9007199254740992}").unwrap();
        assert_eq!(f.kind, FieldKind::I64);
        assert_eq!(
            f.validation_attrs(),
            vec!["range(min = 1, max = 9007199254740992)"]
        );
    }

    #[test]
    fn float_leading_dot_bound_is_canonicalized_to_a_valid_literal() {
        // `.5` parses as a number but is not a valid Rust float literal; the
        // stored/emitted bound must be `0.5`, and a whole-number float bound
        // must carry a decimal point (`1` -> `1.0`).
        let f = parse_field("ratio:f64{min=.5,max=1}").unwrap();
        assert_eq!(f.constraints.min.as_deref(), Some("0.5"));
        assert_eq!(f.constraints.max.as_deref(), Some("1.0"));
        assert_eq!(f.validation_attrs(), vec!["range(min = 0.5, max = 1.0)"]);
    }

    #[test]
    fn integer_signed_and_zero_padded_bounds_are_canonicalized() {
        // `+1` and `007` parse but aren't the literal we want to splice; the
        // stored/emitted bound must be the canonical decimal (`1`, `7`).
        let f = parse_field("count:i32{min=+1,max=007}").unwrap();
        assert_eq!(f.constraints.min.as_deref(), Some("1"));
        assert_eq!(f.constraints.max.as_deref(), Some("7"));
        assert_eq!(f.validation_attrs(), vec!["range(min = 1, max = 7)"]);
    }

    #[test]
    fn integer_bounds_within_range_are_emitted_verbatim() {
        // Regression: ordinary in-range integer bounds are unchanged.
        assert_eq!(
            parse_field("age:i32{min=0,max=130}")
                .unwrap()
                .validation_attrs(),
            vec!["range(min = 0, max = 130)"]
        );
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

    // ── slug (issue #1260) ───────────────────────────────────────────────

    #[test]
    fn parse_slug_field_with_from() {
        let f = parse_field("slug:slug{from:title}").unwrap();
        assert_eq!(f.name, "slug");
        assert_eq!(f.kind, FieldKind::Slug);
        assert_eq!(f.constraints.from.as_deref(), Some("title"));
        assert_eq!(f.rust_type(), "String");
        assert_eq!(f.sql_type(), "TEXT");
    }

    #[test]
    fn slug_field_is_implicitly_unique() {
        // A slug is the record's routing key, so it must be unique even
        // without an explicit `:unique` modifier — this is what lets it
        // fall into the existing `unique`-field migration/repository
        // machinery (issue #1032) for free.
        let f = parse_field("slug:slug{from:title}").unwrap();
        assert!(f.unique);
    }

    #[test]
    fn slug_field_explicit_unique_modifier_is_harmless() {
        let f = parse_field("slug:slug{from:title}:unique").unwrap();
        assert!(f.unique);
        assert_eq!(f.constraints.from.as_deref(), Some("title"));
    }

    #[test]
    fn slug_field_is_never_nullable() {
        // A slug is always the routing key; a nullable slug would mean some
        // records have no URL. `NOT NULL` is unconditional (AC3), so reject
        // rather than silently drop the modifier.
        let err = parse_field("slug:Option<slug>{from:title}").unwrap_err();
        assert!(
            err.to_string().contains("slug") && err.to_string().contains("nullable"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn slug_field_requires_from_modifier() {
        let err = parse_field("slug:slug").unwrap_err();
        assert!(err.to_string().contains("from"), "unexpected error: {err}");
    }

    #[test]
    fn slug_field_rejects_empty_constraint_block() {
        let err = parse_field("slug:slug{}").unwrap_err();
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn from_constraint_only_applies_to_slug_fields() {
        let err = parse_field("title:String{from:body}").unwrap_err();
        assert!(err.to_string().contains("from"), "unexpected error: {err}");
    }

    #[test]
    fn slug_field_rejects_non_ident_from_value() {
        let err = parse_field("slug:slug{from:not a field}").unwrap_err();
        assert!(err.to_string().contains("from"), "unexpected error: {err}");
    }

    #[test]
    fn slug_field_rejects_min_max_constraints() {
        // Sanity: `slug` doesn't accidentally pick up unrelated constraints.
        let err = parse_field("slug:slug{from:title,min=3}").unwrap_err();
        assert!(err.to_string().contains("min"), "unexpected error: {err}");
    }

    #[test]
    fn bare_unique_modifier_still_parses() {
        let f = parse_field("email:String:unique").unwrap();
        assert!(f.unique);
        assert!(f.constraints.is_empty());
    }

    // ── `position` ordering field (issue #1358) ─────────────────────────────

    #[test]
    fn parse_position_field() {
        let f = parse_field("rank:position").unwrap();
        assert_eq!(f.name, "rank");
        assert_eq!(f.kind, FieldKind::Position);
        assert!(!f.nullable);
        assert!(!f.unique);
        assert_eq!(f.constraints.scope, None);
        assert_eq!(f.rust_type(), "i64");
        assert_eq!(f.sql_type(), "BIGINT");
        assert_eq!(f.kind.sqlite_sql_type(), "INTEGER");
    }

    #[test]
    fn parse_position_field_with_scope() {
        let f = parse_field("rank:position{scope:board_id}").unwrap();
        assert_eq!(f.kind, FieldKind::Position);
        assert_eq!(f.constraints.scope.as_deref(), Some("board_id"));
    }

    #[test]
    fn position_field_is_never_nullable() {
        let err = parse_field("rank:Option<position>").unwrap_err();
        assert!(
            err.to_string().contains("position") && err.to_string().contains("nullable"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn position_field_rejects_unique_modifier() {
        let err = parse_field("rank:position:unique").unwrap_err();
        assert!(
            err.to_string().contains("unique"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn scope_constraint_only_applies_to_position_fields() {
        let err = parse_field("title:String{scope:board_id}").unwrap_err();
        assert!(err.to_string().contains("scope"), "unexpected error: {err}");
    }

    #[test]
    fn position_field_rejects_non_ident_scope_value() {
        let err = parse_field("rank:position{scope:not a field}").unwrap_err();
        assert!(err.to_string().contains("scope"), "unexpected error: {err}");
    }

    #[test]
    fn position_field_rejects_empty_constraint_block() {
        let err = parse_field("rank:position{}").unwrap_err();
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn position_field_rejects_min_max_constraints() {
        let err = parse_field("rank:position{min=0}").unwrap_err();
        assert!(err.to_string().contains("min"), "unexpected error: {err}");
    }

    #[test]
    fn position_field_accepts_pascal_case_token() {
        let f = parse_field("rank:Position").unwrap();
        assert_eq!(f.kind, FieldKind::Position);
    }

    #[test]
    fn supported_types_documents_position() {
        assert!(
            SUPPORTED_TYPES.contains("position"),
            "SUPPORTED_TYPES must list position"
        );
        assert!(
            SQLITE_SUPPORTED_KINDS.contains("position"),
            "SQLITE_SUPPORTED_KINDS must list position — it is a plain INTEGER column"
        );
    }

    // ── `{encrypted}` at-rest column encryption (issue #1340) ───────────────

    #[test]
    fn parse_encrypted_modifier_defaults_to_randomized() {
        // AC1: a bare `{encrypted}` marks the column at-rest encrypted in the
        // safe default mode — randomized AEAD, matching bare `#[encrypted]`.
        let f = parse_field("token:String{encrypted}").unwrap();
        assert_eq!(f.name, "token");
        assert_eq!(f.kind, FieldKind::String);
        assert_eq!(f.encrypted_mode(), Some(EncryptedMode::Randomized));
        assert!(f.is_encrypted());
        assert!(f.is_randomized_encrypted());
    }

    #[test]
    fn parse_encrypted_deterministic_modifier() {
        // AC2: the deterministic variant is expressible and maps to the
        // `#[encrypted(deterministic)]` attribute, which keeps `find_by`/
        // `exists_by` equality lookups working.
        let f = parse_field("email:String{encrypted:deterministic}").unwrap();
        assert_eq!(f.encrypted_mode(), Some(EncryptedMode::Deterministic));
        assert!(f.is_encrypted());
        assert!(!f.is_randomized_encrypted());
    }

    #[test]
    fn parse_encrypted_randomized_modifier_is_explicit_default() {
        // The `#[encrypted]` attribute macro accepts an explicit `randomized`
        // option; the DSL mirrors it so a reader can spell out the choice.
        let f = parse_field("token:String{encrypted:randomized}").unwrap();
        assert_eq!(f.encrypted_mode(), Some(EncryptedMode::Randomized));
    }

    #[test]
    fn encrypted_modifier_accepts_the_text_alias() {
        let f = parse_field("notes:Text{encrypted}").unwrap();
        assert_eq!(f.kind, FieldKind::Text);
        assert!(f.is_encrypted());
    }

    #[test]
    fn encrypted_modifier_composes_with_validation_constraints() {
        // Validation runs on the in-memory PLAINTEXT (the model field is a
        // plain `String`), so length/format rules compose normally.
        let f = parse_field("email:String{encrypted:deterministic,max=254,email}").unwrap();
        assert_eq!(f.encrypted_mode(), Some(EncryptedMode::Deterministic));
        assert_eq!(
            f.validation_attrs(),
            vec!["length(max = 254)".to_owned(), "email".to_owned()]
        );
    }

    #[test]
    fn unencrypted_field_reports_no_encryption() {
        let f = parse_field("title:String").unwrap();
        assert_eq!(f.encrypted_mode(), None);
        assert!(!f.is_encrypted());
        assert!(!f.is_randomized_encrypted());
    }

    #[test]
    fn encrypted_only_block_is_not_an_empty_constraint_set() {
        // `is_empty()` means "no constraint modifier was declared"; a field
        // carrying only `{encrypted}` has one.
        let f = parse_field("token:String{encrypted}").unwrap();
        assert!(!f.constraints.is_empty());
        // …but it contributes no `#[validate(...)]` rule.
        assert!(f.validation_attrs().is_empty());
    }

    #[test]
    fn encrypted_modifier_on_nullable_field_is_rejected() {
        // R1 / macro limit: `#[encrypted]` is only supported on non-null
        // `String` columns in v1, so reject rather than emit a model the
        // `#[model]` macro refuses to compile.
        let err = parse_field("token:Option<String>{encrypted}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("encrypted"), "must name the modifier: {msg}");
        assert!(msg.contains("nullable"), "must explain the limit: {msg}");
    }

    #[test]
    fn encrypted_modifier_on_non_string_kinds_is_rejected() {
        // R2: every non-`String`/`Text` kind is rejected at parse time with a
        // message naming the v1 limitation — including `Bytea` (and its
        // `Vec<u8>` spelling), which the issue's AC text mentions but the
        // runtime does not support.
        //
        // The assertion is on the REASON and the kind it names, never on the
        // echoed token: `GenerateError::InvalidField` renders as
        // `invalid field '{token}': {reason}`, so a token containing the word
        // "encrypted" would satisfy `contains("encrypted")` for ANY failure —
        // including an unrelated parse error that never reached this check.
        for (token, kind_name) in [
            ("payload:Bytea{encrypted}", "Bytea"),
            ("payload:Vec<u8>{encrypted}", "Bytea"),
            ("count:i64{encrypted}", "i64"),
            ("ratio:f64{encrypted}", "f64"),
            ("flag:bool{encrypted}", "bool"),
            ("at:DateTime{encrypted}", "DateTime"),
            ("body:richtext{encrypted}", "richtext"),
            ("post:references{encrypted}", "references"),
            ("handle:slug{from:title,encrypted}", "slug{…}"),
        ] {
            let err = parse_field(token).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("the `encrypted` modifier only applies to `String`/`Text` fields"),
                "`{token}` must be rejected by the kind guard: {msg}"
            );
            // `richtext`, `slug{…}` and `enum{…}` all render as `String` in
            // Rust, so the message must name the DECLARED kind or it reads
            // "only applies to String … got a String field".
            assert!(
                msg.contains(&format!("Got a `{kind_name}` field")),
                "`{token}` must name the declared kind `{kind_name}`: {msg}"
            );
        }
    }

    #[test]
    fn encrypted_modifier_on_nullable_text_alias_is_rejected() {
        // The `Option<Text>` spelling hits the same non-null rule as
        // `Option<String>`.
        let err = parse_field("notes:Option<Text>{encrypted}").unwrap_err();
        assert!(
            err.to_string().contains("cannot be nullable"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn encrypted_modifier_tolerates_whitespace_around_the_mode() {
        let f = parse_field("email:String{ encrypted : deterministic }").unwrap();
        assert_eq!(f.encrypted_mode(), Some(EncryptedMode::Deterministic));
    }

    #[test]
    fn repeated_encrypted_modifier_is_rejected() {
        // Last-wins would SILENTLY change what the column leaks: the first form
        // below would upgrade a randomized declaration to deterministic, the
        // second would take away the equality lookups the author asked for.
        for token in [
            "token:String{encrypted,encrypted:deterministic}",
            "token:String{encrypted:deterministic,encrypted}",
            "token:String{encrypted,encrypted}",
        ] {
            let err = parse_field(token).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("declared more than once"),
                "`{token}` must be refused as ambiguous: {msg}"
            );
        }
    }

    #[test]
    fn encrypted_mode_error_prefers_the_kind_diagnosis() {
        // On `count:i64{encrypted:bogus}` the useful diagnosis is that
        // `encrypted` does not apply to an `i64` at all — not that `bogus`
        // is not a mode.
        let err = parse_field("count:i64{encrypted:bogus}").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("only applies to `String`/`Text` fields"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn encrypted_modifier_on_enum_field_is_rejected() {
        // `enum{…}`'s braces are part of the TYPE, so `enum{a,b}{encrypted}`
        // never even reaches the constraint parser — the variant list swallows
        // the second block and fails as a bad variant name. That is a fine
        // outcome (the token IS malformed), but it proves nothing about
        // encryption, so assert the case that does: an `enum` field reaching
        // the kind guard through the `Option<…>`-free scalar path.
        let err = parse_field("status:enum{draft,published}{encrypted}").unwrap_err();
        assert!(
            err.to_string()
                .contains("not a valid snake_case identifier")
                || err
                    .to_string()
                    .contains("only applies to `String`/`Text` fields"),
            "unexpected error: {err}"
        );
        // The kind guard itself refuses `Enum`, named as the DSL token.
        assert!(
            reject_encrypted_on_kind(FieldKind::Enum)
                .unwrap_err()
                .contains("Got a `enum{…}` field")
        );
    }

    #[test]
    fn encrypted_modifier_rejects_unknown_mode() {
        // R16: a typo must not silently degrade to the randomized default.
        let err = parse_field("token:String{encrypted:determinstic}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("determinstic"), "must name the token: {msg}");
        assert!(
            msg.contains("deterministic") && msg.contains("randomized"),
            "must list the accepted modes: {msg}"
        );
    }

    #[test]
    fn encrypted_modifier_rejects_attribute_only_options() {
        // `admin_visible` / `versioned_ciphertext` are real `#[encrypted(...)]`
        // options but deliberately outside the DSL: the whole point of the
        // generator wiring is that a scaffolded encrypted column is redacted in
        // the admin by default. Point at the hand-edit instead of silently
        // accepting or silently dropping them.
        for opt in ["admin_visible", "versioned_ciphertext"] {
            let err = parse_field(&format!("token:String{{encrypted:{opt}}}")).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains(opt), "must name the option: {msg}");
            assert!(
                msg.contains("#[encrypted"),
                "must point at the attribute to hand-edit: {msg}"
            );
        }
    }

    #[test]
    fn encrypted_modifier_with_state_machine_is_rejected() {
        // R10: a `:states(…)` column is a low-entropy closed set. Randomized
        // encryption makes state queries impossible; deterministic encryption
        // leaks the state of every row through ciphertext equality. Neither is
        // what the author meant.
        let err = parse_field("status:String{encrypted}:states(draft -> published)").unwrap_err();
        let msg = err.to_string();
        // Assert the REASON, not the echoed token — which contains both
        // "encrypted" and "states" for free.
        assert!(
            msg.contains("bypasses encryption"),
            "must name the concrete blocker (the transition handler's raw write): {msg}"
        );
    }

    #[test]
    fn randomized_encrypted_unique_field_is_rejected() {
        // AC6: `{encrypted}` + `:unique` would emit a `CREATE UNIQUE INDEX`
        // over randomized ciphertext (which can never collide, so it enforces
        // nothing) and a `find_by_<col>` that fails at RUNTIME with
        // `EncryptionError::RandomizedEqualityLookup`. Fail at generate time
        // instead, pointing at the deterministic mode.
        let err = parse_field("token:String{encrypted}:unique").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("token"), "must name the field: {msg}");
        assert!(
            msg.contains("deterministic"),
            "must point at the fix: {msg}"
        );
    }

    #[test]
    fn deterministic_encrypted_unique_field_is_allowed() {
        // Deterministic ciphertext is stable, so a UNIQUE index over it really
        // does enforce plaintext uniqueness and `find_by_<col>` works.
        let f = parse_field("email:String{encrypted:deterministic}:unique").unwrap();
        assert!(f.unique);
        assert_eq!(f.encrypted_mode(), Some(EncryptedMode::Deterministic));
    }

    #[test]
    fn slug_cannot_derive_from_an_encrypted_field() {
        // A slug is stored in its own PLAINTEXT, unique-indexed column and used
        // as the route key, so deriving one from an encrypted column would put
        // a lightly-mangled copy of the secret on disk in the clear and in
        // every URL. Neither mode makes that safe.
        for mode in ["encrypted", "encrypted:deterministic"] {
            let err = parse_fields(&[
                format!("api_token:String{{{mode}}}"),
                "handle:slug{from:api_token}".to_owned(),
            ])
            .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("api_token"), "must name the source: {msg}");
            assert!(msg.contains("encrypted"), "must name the reason: {msg}");
        }
        // Deriving from a plaintext sibling is still fine on the same model.
        let fields = parse_fields(&[
            "title:String".to_owned(),
            "api_token:String{encrypted}".to_owned(),
            "handle:slug{from:title}".to_owned(),
        ])
        .unwrap();
        assert_eq!(fields.len(), 3);
    }

    #[test]
    fn encrypted_column_sql_type_is_unbounded_text() {
        // AC4: the stored value is a base64 ciphertext envelope, always longer
        // than the plaintext. `TEXT` is unbounded on both backends, so the
        // envelope always fits.
        let f = parse_field("token:String{encrypted}").unwrap();
        assert_eq!(f.sql_column_type(), "TEXT");
        assert_eq!(
            f.sql_column_type_for(autumn_web::config::DatabaseBackend::Sqlite),
            "TEXT"
        );
        assert_eq!(f.schema_type(), "Text");
        assert_eq!(f.rust_type(), "String");
    }

    #[test]
    fn supported_types_help_mentions_the_encrypted_modifier() {
        // AC7 (docs): the `--help`/error-message surface advertises the token.
        assert!(
            SUPPORTED_TYPES.contains("encrypted"),
            "supported-type help must advertise the encrypted modifier: {SUPPORTED_TYPES}"
        );
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

    // ── state machine modifier (issue #1326) ────────────────────────────────

    #[test]
    fn parse_field_without_state_machine_defaults_to_none() {
        let f = parse_field("title:String").unwrap();
        assert!(f.state_machine.is_none());
    }

    #[test]
    fn parse_state_machine_modifier_captures_transitions() {
        let f = parse_field(
            "status:String:states(draft -> published: can_publish, published -> archived)",
        )
        .unwrap();
        assert_eq!(f.kind, FieldKind::String);
        assert!(!f.nullable);
        let sm = f.state_machine.expect("state machine should be parsed");
        assert_eq!(
            sm.transitions,
            vec![
                StateTransition {
                    from: "draft".to_owned(),
                    to: "published".to_owned(),
                    guard: Some("can_publish".to_owned()),
                },
                StateTransition {
                    from: "published".to_owned(),
                    to: "archived".to_owned(),
                    guard: None,
                },
            ]
        );
    }

    #[test]
    fn parse_state_machine_tolerates_trailing_comma_and_whitespace() {
        let f = parse_field("status:String:states( draft -> published , )").unwrap();
        let sm = f.state_machine.unwrap();
        assert_eq!(sm.transitions.len(), 1);
        assert_eq!(sm.transitions[0].from, "draft");
        assert_eq!(sm.transitions[0].to, "published");
        assert!(sm.transitions[0].guard.is_none());
    }

    #[test]
    fn state_machine_composes_with_unique_and_text() {
        let f = parse_field("stage:Text:unique:states(a -> b)").unwrap();
        assert_eq!(f.kind, FieldKind::Text);
        assert!(f.unique);
        assert_eq!(f.state_machine.unwrap().transitions.len(), 1);
    }

    #[test]
    fn state_machine_on_nullable_string_is_rejected() {
        let err = parse_field("status:Option<String>:states(a -> b)").unwrap_err();
        assert!(err.to_string().contains("non-nullable"), "got: {err}");
    }

    #[test]
    fn state_machine_on_non_string_field_is_rejected() {
        let err = parse_field("count:i64:states(a -> b)").unwrap_err();
        assert!(err.to_string().contains("String"), "got: {err}");
    }

    #[test]
    fn state_machine_on_enum_field_is_rejected() {
        let err = parse_field("status:enum{a,b}:states(a -> b)").unwrap_err();
        assert!(err.to_string().contains("String"), "got: {err}");
    }

    #[test]
    fn empty_state_machine_modifier_is_rejected() {
        let err = parse_field("status:String:states()").unwrap_err();
        assert!(err.to_string().contains("at least one"), "got: {err}");
    }

    #[test]
    fn state_machine_missing_arrow_is_rejected() {
        let err = parse_field("status:String:states(draft published)").unwrap_err();
        assert!(err.to_string().contains("->"), "got: {err}");
    }

    #[test]
    fn state_machine_invalid_state_name_is_rejected() {
        let err = parse_field("status:String:states(Draft -> published)").unwrap_err();
        assert!(err.to_string().contains("state name"), "got: {err}");
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

/// Parity tests locking `dsl::FieldKind` / `dsl::IdType` to the canonical
/// `autumn-schema-core` IR (declarative schema wave, tracking issue #1975).
///
/// This module is the **drift lock**: for every `FieldKind` / `IdType` it
/// asserts the schema-core equivalent yields byte-identical Rust / diesel / SQL
/// mappings on both backends. If either side changes a mapping without the
/// other agreeing, one of these assertions fails. It exercises no `dsl.rs`
/// runtime behaviour — it only reads the existing mapping functions.
#[cfg(test)]
mod schema_core_parity {
    use super::{Field, FieldConstraints, FieldKind, IdType};
    use autumn_schema_core::{Backend, ColumnType, IdKind};
    use autumn_web::config::DatabaseBackend;

    /// Build a minimal, non-null `Field` wrapping `kind` so we can call the
    /// `Field`-level `sql_column_type_for` — the method that renders the full
    /// `NUMERIC(precision,scale)` for decimals (the bare `FieldKind::sql_type_for`
    /// returns only `"NUMERIC"`), i.e. the exact string that reaches DDL.
    fn field_of(kind: FieldKind, variants: Vec<String>) -> Field {
        Field {
            name: "col".to_owned(),
            kind,
            nullable: false,
            variants,
            unique: false,
            constraints: FieldConstraints::default(),
            state_machine: None,
        }
    }

    #[test]
    fn field_kind_mappings_match_schema_core() {
        // (FieldKind, schema-core ColumnType, enum variants for the Field).
        // `References` maps to `Int64` — a foreign key is a `Column` property in
        // the IR, not a distinct column type. `Enum` compares the `String`/`TEXT`
        // storage fallback (the concrete enum type is a later-slice concern).
        let cases: Vec<(FieldKind, ColumnType, Vec<String>)> = vec![
            (FieldKind::String, ColumnType::Text, vec![]),
            (FieldKind::Text, ColumnType::Text, vec![]),
            // `RichText` is a presentation/UX distinction only — it stores the
            // Markdown *source*, so every storage mapping is `Text`'s.
            (FieldKind::RichText, ColumnType::Text, vec![]),
            (FieldKind::I32, ColumnType::Int32, vec![]),
            (FieldKind::I64, ColumnType::Int64, vec![]),
            (FieldKind::References, ColumnType::Int64, vec![]),
            (FieldKind::Bool, ColumnType::Bool, vec![]),
            (FieldKind::F32, ColumnType::Float32, vec![]),
            (FieldKind::F64, ColumnType::Float64, vec![]),
            (FieldKind::Uuid, ColumnType::Uuid, vec![]),
            (FieldKind::NaiveDateTime, ColumnType::Timestamp, vec![]),
            (FieldKind::DateTime, ColumnType::TimestampTz, vec![]),
            (FieldKind::Bytea, ColumnType::Bytes, vec![]),
            (FieldKind::Attachment, ColumnType::Attachment, vec![]),
            (FieldKind::Json, ColumnType::Json, vec![]),
            (
                FieldKind::Decimal {
                    precision: 12,
                    scale: 2,
                },
                ColumnType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                vec![],
            ),
            (
                FieldKind::Enum,
                ColumnType::Enum {
                    variants: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
                },
                vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            ),
        ];

        for (fk, ct, variants) in cases {
            let field = field_of(fk, variants);

            // Rust `#[model]` type (storage fallback for Enum: `String`).
            assert_eq!(
                fk.rust_type(),
                ct.rust_type(),
                "rust_type parity for {fk:?}"
            );

            // Diesel `schema.rs` token, both backends.
            assert_eq!(
                fk.schema_type_for(DatabaseBackend::Postgres),
                ct.diesel_type(Backend::Postgres),
                "pg diesel token parity for {fk:?}"
            );
            assert_eq!(
                fk.schema_type_for(DatabaseBackend::Sqlite),
                ct.diesel_type(Backend::Sqlite),
                "sqlite diesel token parity for {fk:?}"
            );

            // SQL DDL type, both backends — via the `Field`-level renderer so a
            // decimal's `NUMERIC(precision,scale)` is compared in full.
            assert_eq!(
                field.sql_column_type_for(DatabaseBackend::Postgres),
                ct.sql_type(Backend::Postgres),
                "pg sql type parity for {fk:?}"
            );
            assert_eq!(
                field.sql_column_type_for(DatabaseBackend::Sqlite),
                ct.sql_type(Backend::Sqlite),
                "sqlite sql type parity for {fk:?}"
            );

            // SQLite diesel-conversion eligibility.
            assert_eq!(
                fk.sqlite_has_diesel_conversion(),
                ct.sqlite_has_diesel_conversion(),
                "sqlite diesel-conversion parity for {fk:?}"
            );

            // Rust `#[model]` type on SQLite — `Uuid`/`Decimal` render
            // autumn-web's TEXT-backed newtypes (issue #1924).
            assert_eq!(
                fk.rust_type_for(DatabaseBackend::Sqlite),
                ct.rust_type_for(Backend::Sqlite),
                "sqlite rust_type parity for {fk:?}"
            );
            assert_eq!(
                fk.rust_type_for(DatabaseBackend::Postgres),
                ct.rust_type_for(Backend::Postgres),
                "pg rust_type parity for {fk:?}"
            );
        }
    }

    #[test]
    fn id_type_mappings_match_schema_core() {
        for (idt, idk) in [
            (IdType::BigSerial, IdKind::BigSerial),
            (IdType::Uuid, IdKind::Uuid),
        ] {
            assert_eq!(
                idt.rust_type(),
                idk.rust_type(),
                "id rust_type parity for {idt:?}"
            );
            for (db, be) in [
                (DatabaseBackend::Postgres, Backend::Postgres),
                (DatabaseBackend::Sqlite, Backend::Sqlite),
            ] {
                assert_eq!(
                    idt.pk_sql_for(db),
                    idk.pk_sql(be),
                    "pk_sql parity for {idt:?} on {db:?}"
                );
                assert_eq!(
                    idt.schema_type_for(db),
                    idk.diesel_type(be),
                    "id diesel token parity for {idt:?} on {db:?}"
                );
            }
        }
    }
}
