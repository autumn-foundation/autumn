//! The declarative-schema **diff engine** (slice 4 of tracking issue #1975).
//!
//! Given a **desired** state (the app's `#[model]` structs, lifted by the
//! slice-2 [parser](crate::schema::parse)) and a **baseline** (the checked-in,
//! dialect-tagged [snapshot](crate::schema::snapshot) from slice 3), this module
//! computes the pending migration — `desired − baseline` — as a structured
//! [`MigrationPlan`] and renders it as a diesel `up.sql` / `down.sql` pair.
//!
//! It is a **pure, DB-free, IO-free** library module: [`diff_schema`] is a pure
//! function of two in-memory schema states, [`guard_plan`] is a pure policy
//! check, and [`emit_up_sql`] / [`emit_down_sql`] are pure renderers. All IO
//! (loading the snapshot, parsing the models directory, writing the migration
//! files) lives in the command wiring ([`crate::schema::run`]'s `run_diff`), not
//! here.
//!
//! # Conservatism over completeness (the load-bearing rule)
//!
//! The slice-2 parser is a **partial** view of the schema: it documents three
//! gaps it cannot represent — (a) it never fabricates a `CHECK` for an enum
//! column, (b) only an explicit `#[references]` yields a foreign key (association
//! FKs are invisible), and (c) only convention `#[default]`s are recovered. A
//! migration derived from a partial desired state must therefore **never emit a
//! destructive op for a facet the parser simply cannot see** — doing so would
//! delete hand-written constraints that live only in the (richer) baseline.
//!
//! This module encodes that as the deliberate **absence** of `DropCheck`,
//! `DropForeignKey`, and `DropDefault` variants on [`SchemaChange`]: a baseline
//! check / FK / default absent from the desired side is treated as *unknown,
//! retained* — not *removed*. An enum column the parser skipped (recorded as a
//! [`SchemaDiagnostic`](crate::schema::parse::SchemaDiagnostic)) is likewise not
//! dropped. The **accepted trade-off** is that the diff is intentionally
//! conservative and will *miss* a genuine removal of one of those facets; that is
//! preferable to ever destroying parser-invisible or hand-written state, and the
//! genuine-removal case is recovered by later slices (`#[renamed_from]`, richer
//! parsing, a shadow-DB oracle).
//!
//! # Invisible `#[belongs_to(...)]` association foreign keys (offline limitation)
//!
//! The slice-2 parser cannot see a generated `#[belongs_to(...)]` association
//! foreign key: real generator output models it as a plain `<name>_id: i64`
//! column plus a **struct-level** `#[belongs_to(...)]` attribute, and the parser
//! lifts the `i64` field as an ordinary `Int64` column with `references: None` and
//! **no diagnostic** (only an *unresolvable field type* — a generated enum, or an
//! association written as a bare struct-typed field `pub user: User` — records a
//! [`SchemaDiagnostic`](crate::schema::parse::SchemaDiagnostic), and even that
//! carries no target-table). So the diff engine has **no offline signal** for the
//! presence, column, or target of an association FK. This produces two
//! consequences the engine handles conservatively:
//!
//! 1. **Adding a foreign key to a pre-existing column is refused**
//!    ([`DiffError::AddForeignKeyToPreexistingColumn`]). When a column present on
//!    both sides gains an explicit `#[references]` where the baseline recorded
//!    none, the baseline `references: None` is *unknown*, not proof the database
//!    lacks a constraint — the column may already carry an inline `REFERENCES`
//!    named `<table>_<column>_fkey` from the original association-FK generation, so
//!    a fresh `ADD CONSTRAINT <table>_<column>_fkey` would collide and the
//!    migration would not apply. The engine refuses (no override) rather than emit
//!    it; the fix is a manual migration or an authoritative re-snapshot. A
//!    **brand-new** FK column (an `AddColumn` whose `REFERENCES` renders inline) is
//!    safe and is emitted normally — there is no pre-existing constraint to hit.
//!
//! 2. **`DROP TABLE` cannot be checked against invisible inbound association
//!    FKs.** The inbound-reference scan ([`detect_inbound_fk_blocks`]) refuses a
//!    drop whenever a retained table holds a **visible** `references` to the
//!    dropped table (rounds 2/3/5, unchanged and still enforced). But an invisible
//!    `#[belongs_to(...)]` association FK on a retained table produces no
//!    `Column.references` and no diagnostic, so it cannot be detected offline —
//!    the resulting `DROP TABLE` (already `--allow-destructive`-gated) may fail to
//!    apply because the real `<retained>_<col>_fkey` constraint still depends on
//!    the dropped table. A precise guard is impossible without schema introspection
//!    (a future slice's shadow-DB / `--dev-url` oracle); a blanket "refuse every
//!    table drop" would make the common case useless and is deliberately **not**
//!    done. Instead every emitted `DROP TABLE` carries an advisory
//!    `-- autumn-safety:` comment naming this exact gap so the operator verifies
//!    (introspection or a manual FK drop) before applying. This is a documented
//!    limitation of the offline approach, not a bug.
//!
//! # Uniqueness is diffed only through indexes
//!
//! `Column.unique` is treated as informational metadata and is **not** separately
//! diffed. The slice-2 parser always emits a `unique` column together with its
//! `idx_<table>_<field>_unique` unique index, so the index set already carries
//! the uniqueness signal; diffing both would double-count.
//!
//! # Required-column additions need a default
//!
//! Adding a `NOT NULL` column **without a default** to an *existing* table is
//! refused ([`DiffError::RequiredColumnWithoutDefault`]): Postgres validates the
//! constraint against existing rows the instant the column is added, so the
//! `ALTER TABLE ... ADD COLUMN ... NOT NULL` fails on any table that already has
//! rows, and the offline diff engine has no backfill value to synthesize (the
//! safe nullable → backfill → `SET NOT NULL` sequence is a manual multi-step
//! migration). Give the field a `#[default(...)]` (including a synthesized one
//! like `created_at DEFAULT NOW()`) or make it nullable (`Option<...>`). A
//! `NOT NULL`, no-default column inside a brand-new `CreateTable` is fine — the
//! table is empty — and is not refused.
//!
//! Its exact sibling — turning an *existing* nullable column non-null
//! (`SET NOT NULL`) — is refused for the same reason
//! ([`DiffError::SetNotNullRequiresBackfill`]): Postgres validates the
//! constraint against every existing row the instant the `ALTER COLUMN ... SET
//! NOT NULL` runs, so it fails on any pre-existing NULL, and the offline engine
//! has no backfill value to synthesize. The only appliable form would pair a
//! default with a backfill, which is not expressible offline. The inverse
//! (`DROP NOT NULL`, making a column nullable) is always safe and is emitted
//! normally.
//!
//! # Generated identifiers must fit Postgres's 63-byte limit
//!
//! This engine derives some identifiers by unbounded interpolation — the foreign-key
//! constraint name `{table}_{column}_fkey` (`ADD CONSTRAINT` up, `DROP CONSTRAINT`
//! down — both derive it identically, so they always agree) — and emits the
//! parser-provided index names (`idx_<table>_<field>`) verbatim. Postgres silently
//! truncates any identifier to 63 bytes (`NAMEDATALEN - 1`). A generated name over
//! that limit is therefore (a) *silently renamed* on apply, risking a mismatch with
//! hand-written SQL that spells the untruncated name, and (b) a *collision* hazard:
//! if two generated names in the same migration share their first 63 bytes, PG
//! accepts the first `ADD CONSTRAINT`/`CREATE INDEX` and rejects the second as a
//! duplicate relation — an unappliable migration.
//!
//! The **engine-generated FK-constraint name** is purely internal (Postgres would
//! otherwise auto-name the constraint), so it is safe to rename: when
//! `{table}_{column}_fkey` would exceed 63 bytes it is passed through
//! [`bounded_pg_identifier`], which truncates the head and appends a short,
//! deterministic hex suffix (a digest of the full untruncated name) so the result
//! is a valid `≤ 63`-byte identifier. The **same** helper drives both the up
//! `ADD CONSTRAINT` and the down `DROP CONSTRAINT`, and the [`guard_plan`] length
//! check, so all three always agree on the emitted name. A short FK name (the
//! common case) is returned unchanged, so Postgres output is byte-stable.
//!
//! Parser-provided index names (`idx_<table>_<field>`) are **not** renamed — the
//! app spells them elsewhere — so [`guard_plan`] still refuses (no override — the
//! SQL is unappliable, not merely lossy) any Postgres plan that generates an index
//! identifier **longer than 63 bytes**, plus (defensively) any pair of generated
//! names that collide after truncation ([`DiffError::GeneratedIdentifierTooLong`]).
//! The check is **Postgres-only**: `SQLite` has no comparably short identifier
//! limit and does not truncate.
//!
//! # `SQLite` boundary
//!
//! Slice 5 renders both dialects. Postgres emits the direct `ALTER`-family
//! statements; `SQLite` — which has no `ALTER COLUMN TYPE` / `SET`/`DROP NOT NULL`
//! / `SET DEFAULT` / `ADD CHECK` / `ADD CONSTRAINT` — realises those via the
//! **table-recreate** strategy ([`emit_up_sql_with_context`]): `CREATE` a new table
//! with the desired shape, `INSERT .. SELECT` the surviving rows, `DROP` the old
//! table, `RENAME` the new one into place, and recreate the desired indexes,
//! wrapped with `PRAGMA foreign_keys` + a `PRAGMA foreign_key_check`. The recreate
//! needs a table's full desired (up) / baseline (down) shape, which the per-change
//! [`SchemaChange`] deltas do not carry, so it is threaded in via a
//! [`SchemaContext`]; the context-free [`emit_up_sql`] / [`emit_down_sql`] cover
//! Postgres and the portable `SQLite` subset (`CREATE TABLE`, `DROP TABLE`,
//! `ADD COLUMN`, `CREATE`/`DROP INDEX`) and return [`EmitError::SqliteRebuildUnsupported`]
//! if a `SQLite` rebuild is required without the shapes to build it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use autumn_schema_core::{
    Backend, CheckConstraint, Column, ColumnDefault, ColumnType, IdKind, Index, Table,
};

use crate::schema::parse::ParsedSchema;

/// The computed migration: an ordered list of structural changes plus the
/// dialect they render against. Deterministic — same inputs ⇒ same plan ⇒ same
/// SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    /// The dialect the plan renders against (its provider-lock).
    pub backend: Backend,
    /// The structural changes, in a stable order.
    pub changes: Vec<SchemaChange>,
}

impl MigrationPlan {
    /// True when there is nothing to migrate (the no-op case).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// One structural change. Variants that DROP existing state carry the full
/// baseline object so `down.sql` can reconstruct it; `Alter*` carry both
/// endpoints so the down leg can invert.
///
/// **Deliberately-absent variants (conservatism):** there is no `DropDefault`,
/// no `DropForeignKey`, no `DropCheck`, and no `AlterUnique`. Those would fire on
/// facets the slice-2 parser cannot fully observe, so emitting them risks
/// destroying hand-written state — their absence is the conservatism mechanism,
/// not an oversight (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaChange {
    // ---- table level ----
    /// A managed table present in the desired state but not the baseline.
    CreateTable(Table),
    /// A managed table present in the baseline but not the desired state.
    /// DESTRUCTIVE; carries the full baseline [`Table`] so `down.sql` can rebuild
    /// it.
    DropTable(Table),

    // ---- column level (keyed on column NAME, never position) ----
    /// A column present in the desired table but not the baseline.
    AddColumn {
        /// The owning table name.
        table: String,
        /// The new column.
        column: Column,
    },
    /// A column present in the baseline table but not the desired one.
    /// DESTRUCTIVE; carries the full baseline [`Column`] so `down.sql` can re-add
    /// it.
    DropColumn {
        /// The owning table name.
        table: String,
        /// The baseline column being dropped.
        column: Column,
    },
    /// A same-named column whose logical type changed.
    AlterColumnType {
        /// The owning table name.
        table: String,
        /// The column name.
        column: String,
        /// The baseline type (for the down leg).
        from: ColumnType,
        /// The desired type.
        to: ColumnType,
    },
    /// A same-named column that gained a `NOT NULL` constraint.
    SetNotNull {
        /// The owning table name.
        table: String,
        /// The column name.
        column: String,
    },
    /// A same-named column that dropped its `NOT NULL` constraint.
    DropNotNull {
        /// The owning table name.
        table: String,
        /// The column name.
        column: String,
    },
    /// A same-named column whose default was set or changed. Carries the baseline
    /// default so the down leg can restore or drop it.
    SetDefault {
        /// The owning table name.
        table: String,
        /// The column name.
        column: String,
        /// The desired default.
        to: ColumnDefault,
        /// The baseline default, if any (for the down leg).
        from: Option<ColumnDefault>,
    },
    /// An explicit foreign key added as a **standalone** `ADD CONSTRAINT ...
    /// FOREIGN KEY` statement.
    ///
    /// Not currently produced by [`diff_schema`]: a **new** FK column renders its
    /// `REFERENCES` inline in the `ADD COLUMN`, and adding an FK to a
    /// **pre-existing** column is refused (see
    /// [`SchemaChange::AddForeignKeyToExistingColumn`] — the parser cannot see a
    /// generated association FK, so it cannot prove the constraint is absent). The
    /// variant and its renderer are retained for the future slice that can observe
    /// FK constraints reliably (schema introspection / a shadow-DB oracle) and can
    /// therefore safely emit a standalone FK add on an existing column.
    #[allow(dead_code)]
    AddForeignKey {
        /// The owning table name.
        table: String,
        /// The column name.
        column: String,
        /// The referenced table/column.
        foreign_key: autumn_schema_core::ForeignKey,
    },

    // ---- index / check level (add/remove by NAME) ----
    /// A secondary index present in the desired table but not the baseline.
    AddIndex {
        /// The owning table name.
        table: String,
        /// The new index.
        index: Index,
    },
    /// A secondary index present in the baseline table but not the desired one.
    /// Carries the full baseline [`Index`] so `down.sql` can recreate it. Not in
    /// the destructive tier (it drops no rows).
    DropIndex {
        /// The owning table name.
        table: String,
        /// The baseline index being dropped.
        index: Index,
    },
    /// A `CHECK` constraint present in the desired table but not the baseline.
    /// (There is deliberately no `DropCheck` — see the module docs.)
    AddCheck {
        /// The owning table name.
        table: String,
        /// The new check constraint.
        check: CheckConstraint,
    },

    /// A managed both-present table whose primary key differs. A non-emittable
    /// marker: [`guard_plan`] refuses any plan containing it (with no override),
    /// so it never reaches the emitters in the command flow. It exists so the
    /// plan-only `guard_plan` signature can detect the change — a PK migration is
    /// exotic, backend-divergent, and out of scope for this slice.
    PrimaryKeyChange {
        /// The table whose primary key changed.
        table: String,
    },

    /// A same-named column whose **existing** explicit foreign key changed target
    /// (e.g. `author_id` from `users(id)` to `accounts(id)`). A non-emittable
    /// marker: [`guard_plan`] refuses any plan containing it (with no override).
    ///
    /// It exists because a conservative engine cannot safely retarget an FK: it
    /// has no `DropForeignKey` variant to remove the baseline constraint (whose
    /// name is PG's default `<table>_<column>_fkey`), so blindly emitting an
    /// `AddForeignKey` would collide on that constraint name and the migration
    /// would fail. Retargeting an FK is deferred (a later slice can drop+recreate
    /// once it can observe FK constraints reliably); until then it is refused.
    ForeignKeyChange {
        /// The owning table name.
        table: String,
        /// The column whose FK target changed.
        column: String,
    },

    /// A managed table is dropped while a **retained** table still holds a
    /// baseline foreign key pointing at it (e.g. drop `users` while
    /// `posts.user_id REFERENCES users(id)` survives). A non-emittable marker:
    /// [`guard_plan`] refuses any plan containing it, with **no override** — even
    /// `--allow-destructive` cannot permit it, because that flag authorizes losing
    /// the dropped table's own data, not silently breaking another table's
    /// referential integrity (PG rejects the bare `DROP TABLE`, and this engine has
    /// no `DropForeignKey` variant to clear the constraint first). Self-referential
    /// FKs and FKs from a table that is itself being dropped do **not** produce
    /// this marker — those constraints go away with their table.
    DropTableBlockedByInboundFk {
        /// The table being dropped.
        table: String,
        /// The retained table whose FK references the dropped table.
        referencing_table: String,
        /// The column on `referencing_table` carrying the blocking FK.
        referencing_column: String,
    },

    /// A same-named column whose logical type changed *and* which participates in
    /// a foreign-key constraint that this engine cannot drop and recreate — either
    /// the column is itself a referencing FK column, or another retained table has
    /// a baseline FK targeting it. A non-emittable marker: [`guard_plan`] refuses
    /// any plan containing it with **no override**, because Postgres rejects
    /// `ALTER COLUMN ... TYPE` on a column bound by an FK constraint, and this
    /// engine has no `DropForeignKey`/re-add path to clear it first. It sits on top
    /// of the [`DiffError::NonImplicitTypeConversion`] guard, catching even the
    /// implicit widenings (`int4`→`int8`, `float4`→`float8`) that guard allows.
    AlterColumnTypeBlockedByFk {
        /// The owning table name.
        table: String,
        /// The column whose type change is blocked by an FK constraint.
        column: String,
    },

    /// A **pre-existing** baseline column (present on both sides) that gained an
    /// explicit `#[references]` where the baseline recorded no foreign key. A
    /// non-emittable marker: [`guard_plan`] refuses any plan containing it (with
    /// **no override**).
    ///
    /// It exists because the slice-2 parser cannot see a generated
    /// `#[belongs_to(...)]` association FK — a `<name>_id: i64` column is parsed as
    /// a plain `Int64` with `references: None`, producing *no* diagnostic. So a
    /// baseline `references: None` on a pre-existing column is **unknown**, not
    /// proof the database has no constraint: the column may already carry an inline
    /// `REFERENCES` constraint named `<table>_<column>_fkey` from the original
    /// association-FK generation. Emitting `AddForeignKey` would
    /// `ADD CONSTRAINT <table>_<column>_fkey` a second time and collide with that
    /// existing constraint — an unappliable migration. A **newly-added** FK column
    /// (an [`SchemaChange::AddColumn`], which renders its `REFERENCES` inline) is
    /// safe and is never flagged; only a pre-existing column gaining an FK is.
    AddForeignKeyToExistingColumn {
        /// The owning table name.
        table: String,
        /// The pre-existing column that gained an explicit foreign key.
        column: String,
    },

    /// A brand-new managed table whose model has one or more fields the parser
    /// could not represent (a generated enum, a bare `pub user: User` association)
    /// and therefore recorded a [`SchemaDiagnostic`](crate::schema::parse::SchemaDiagnostic)
    /// for instead of a column. A non-emittable marker: [`guard_plan`] refuses any
    /// plan containing it (with **no override**).
    ///
    /// It exists because emitting `CREATE TABLE` from the partial parser output
    /// would produce DDL that OMITS the skipped column(s) — the generated model
    /// still queries them, so the app would hit "column does not exist" at runtime
    /// the moment it touches the new table. A table missing a column the app
    /// queries is as broken as unappliable SQL, so the conservative stance refuses
    /// and directs the user to a manual migration rather than emit incomplete DDL.
    /// This fires ONLY for a table being CREATED; an existing table with a skipped
    /// column is handled by the `DropColumn`/`DropIndex` suppressions instead.
    CreateTableBlockedBySkippedField {
        /// The table that cannot be safely created.
        table: String,
        /// The parser-skipped field name(s) that would be missing from the DDL.
        fields: Vec<String>,
    },
}

/// Diff policy knobs.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiffOptions {
    /// Second-tier destructive escape hatch. When `false` (the default), a plan
    /// containing a `DropColumn`/`DropTable` — or an ambiguous rename — is refused
    /// by [`guard_plan`]. When `true`, both are permitted (the rename is treated
    /// as an independent drop+add).
    pub allow_destructive: bool,
}

/// Why a computed plan is refused for emission (policy, not structure).
#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    /// The plan drops a table or column and `--allow-destructive` was not passed.
    #[error(
        "refusing to emit a destructive migration: {summary}. \
         Re-run with --allow-destructive to generate it anyway."
    )]
    Destructive {
        /// A human-readable list of the destructive ops.
        summary: String,
        /// The destructive ops, for programmatic inspection.
        ops: Vec<DestructiveOp>,
    },

    /// A single table both dropped and added column(s) — possibly a rename.
    #[error(
        "ambiguous change on table `{table}`: column(s) [{}] disappeared and [{}] appeared. \
         If this is a rename, use #[renamed_from] (not yet supported — slice 5+); \
         refusing to emit a drop+add. Re-run with --allow-destructive to treat them \
         as independent drop/add.",
        .dropped.join(", "),
        .added.join(", ")
    )]
    PossibleRename {
        /// The table with the ambiguous change.
        table: String,
        /// The dropped column names.
        dropped: Vec<String>,
        /// The added column names.
        added: Vec<String>,
    },

    /// A both-present table's primary key changed (unsupported this slice, no
    /// override).
    #[error("primary-key change on table `{table}` is not supported in this slice")]
    PrimaryKeyChange {
        /// The table whose primary key changed.
        table: String,
    },

    /// An existing explicit foreign key changed its target (unsupported this
    /// slice, no override — see [`SchemaChange::ForeignKeyChange`]).
    #[error(
        "foreign-key retarget on `{table}.{column}` is not supported in this slice: \
         the baseline already has a foreign key on this column, and this engine has no \
         way to drop it before adding the new one without colliding on the \
         `{table}_{column}_fkey` constraint name. Retargeting a foreign key is deferred \
         to a later slice."
    )]
    ForeignKeyChange {
        /// The owning table name.
        table: String,
        /// The column whose FK target changed.
        column: String,
    },

    /// A table is dropped while another table (a retained baseline table, or a
    /// new/retained desired table) references it (unsupported this slice, **no
    /// override** — `--allow-destructive` does not permit it).
    #[error(
        "cannot drop table `{table}`: `{referencing_table}.{referencing_column}` \
         has a foreign key referencing it. Dropping `{table}` would violate that constraint, \
         and this engine has no way to drop the inbound foreign key first. Drop or retarget \
         `{referencing_table}.{referencing_column}` first. (--allow-destructive does not override \
         this: it authorizes losing this table's data, not breaking another table's integrity.)"
    )]
    DropTableInboundReference {
        /// The table that cannot be dropped.
        table: String,
        /// The table holding the inbound foreign key.
        referencing_table: String,
        /// The column carrying the inbound foreign key.
        referencing_column: String,
    },

    /// A column type change PG cannot cast implicitly (needs a manual `USING`
    /// clause), so a bare `ALTER COLUMN ... TYPE` would be rejected (unsupported
    /// this slice, **no override** — the SQL is unappliable, not merely lossy).
    #[error(
        "non-implicit type conversion on `{table}.{column}` from {from} to {to} requires a manual \
         migration with a USING clause — not supported in this slice"
    )]
    NonImplicitTypeConversion {
        /// The owning table name.
        table: String,
        /// The column name.
        column: String,
        /// The baseline SQL type.
        from: String,
        /// The desired SQL type.
        to: String,
    },

    /// An `ADD COLUMN` of a `NOT NULL` column without a default, to an existing
    /// table (unsupported this slice, **no override** — the SQL is unappliable on
    /// a table that already has rows, not merely lossy). Postgres validates the
    /// `NOT NULL` against existing rows the moment the column is added, so
    /// `ALTER TABLE t ADD COLUMN c <type> NOT NULL` fails on any non-empty table;
    /// the offline diff engine has no backfill value to synthesize. A brand-new
    /// [`SchemaChange::CreateTable`] carrying such a column is fine (the table is
    /// empty) and is **not** refused.
    #[error(
        "cannot add required column `{table}.{column}`: a NOT NULL column without a default \
         fails on a table that already has rows. Add a default (`#[default(...)]`) or make the \
         column nullable (`Option<...>`)."
    )]
    RequiredColumnWithoutDefault {
        /// The table gaining the column.
        table: String,
        /// The required column name.
        column: String,
    },

    /// A `SET NOT NULL` on an existing (previously-nullable) column (unsupported
    /// this slice, **no override** — the SQL is unappliable on a table whose
    /// column already holds NULLs, not merely lossy). Postgres validates the
    /// constraint against every existing row the instant `ALTER COLUMN ... SET
    /// NOT NULL` runs, so it fails on any pre-existing NULL; the offline diff
    /// engine has no backfill value to synthesize. This is the exact sibling of
    /// [`DiffError::RequiredColumnWithoutDefault`] (adding a required column) —
    /// the only appliable form would be a simultaneous default plus a backfill,
    /// which is not expressible offline, so the engine refuses rather than emit
    /// an unappliable migration. The inverse [`SchemaChange::DropNotNull`]
    /// (non-null → nullable) is always safe and is never refused.
    #[error(
        "cannot set `{table}.{column}` NOT NULL: existing NULL rows would fail the constraint. \
         Backfill the column and apply the change manually, or keep it nullable (`Option<...>`)."
    )]
    SetNotNullRequiresBackfill {
        /// The owning table.
        table: String,
        /// The column being made non-null.
        column: String,
    },

    /// A unique-index add whose columns ALL pre-existed on an existing table
    /// (unsupported this slice, **no override** — the SQL is unappliable on a
    /// table whose existing rows may already hold duplicate values, not merely
    /// lossy). Postgres validates uniqueness against every existing row the
    /// instant `CREATE UNIQUE INDEX` runs, so it fails on any pre-existing
    /// duplicate; the offline diff engine cannot dedup the data. This is the
    /// index-level sibling of [`DiffError::SetNotNullRequiresBackfill`]. It fires
    /// only for an [`SchemaChange::AddIndex`] with `unique: true` on a
    /// pre-existing table where **none** of the indexed columns is being added in
    /// the same plan — a unique index on a brand-new [`SchemaChange::CreateTable`]
    /// (empty table) or one touching a newly-added column (whose existing rows are
    /// all NULL, which Postgres treats as distinct) is always safe and is never
    /// refused. A non-unique `AddIndex` is likewise never refused.
    #[error(
        "cannot add unique index `{index}` on `{table}`: existing rows may contain duplicate \
         values that would fail `CREATE UNIQUE INDEX`. Resolve duplicates and apply this \
         manually, or drop the uniqueness requirement."
    )]
    UniqueIndexRequiresDedup {
        /// The owning table.
        table: String,
        /// The unique index being added.
        index: String,
    },

    /// A column type change on a column that participates in a foreign-key
    /// constraint (unsupported this slice, **no override** — the SQL is
    /// unappliable, not merely lossy). Postgres rejects `ALTER COLUMN ... TYPE` on
    /// a column bound by an FK constraint (as either the referencing column or the
    /// referenced key), and this engine has no `DropForeignKey`/re-add path to
    /// clear the constraint first. This applies even to the implicit widenings the
    /// [`DiffError::NonImplicitTypeConversion`] guard would otherwise allow.
    #[error(
        "cannot change the type of `{table}.{column}`: it participates in a foreign-key \
         constraint, which this engine cannot drop and recreate. Change the type via a manual \
         migration that drops and recreates the foreign key."
    )]
    TypeChangeOnForeignKeyColumn {
        /// The owning table name.
        table: String,
        /// The column whose type change is blocked by an FK constraint.
        column: String,
    },

    /// An explicit foreign key was added to a **pre-existing** column whose
    /// baseline recorded no foreign key (unsupported this slice, **no override** —
    /// the SQL may be unappliable, not merely lossy). Because the slice-2 parser
    /// cannot see a generated `#[belongs_to(...)]` association FK (a `<name>_id`
    /// column parses as a plain `Int64` with `references: None` and no diagnostic),
    /// a baseline `references: None` on a pre-existing column is *unknown*, not
    /// proof the database lacks a constraint: the column may already carry an
    /// inline `REFERENCES` named `<table>_<column>_fkey`, so a fresh
    /// `ADD CONSTRAINT` of that same name would collide. A brand-new FK column (an
    /// `ADD COLUMN` with an inline `REFERENCES`) is safe and is never refused.
    #[error(
        "cannot add a foreign key to the pre-existing column `{table}.{column}`: the offline \
         snapshot cannot confirm whether the database already has a constraint for it (e.g. a \
         generated association foreign key), and adding a duplicate `{table}_{column}_fkey` would \
         fail. Add the foreign key via a manual migration, or re-snapshot from an authoritative \
         source."
    )]
    AddForeignKeyToPreexistingColumn {
        /// The owning table name.
        table: String,
        /// The pre-existing column that gained an explicit foreign key.
        column: String,
    },

    /// A generated FK-constraint or index identifier is emitted more than once, or
    /// exceeds Postgres's 63-byte identifier limit, or collides with another
    /// generated name after PG truncates both to 63 bytes (unsupported this slice,
    /// **no override** — the SQL is unappliable, not merely lossy). Postgres
    /// silently truncates any identifier to `NAMEDATALEN - 1` (63) bytes, so an
    /// over-long generated name is both renamed on apply (risking a mismatch with
    /// hand-written SQL) and a duplicate-relation hazard when two names share their
    /// first 63 bytes. A second, distinct hazard: two generated names that are
    /// *exactly* identical (e.g. a `#[unique] foo` field and a separate field named
    /// `foo_unique` both yield `idx_<table>_foo_unique`) — PG accepts the first
    /// `CREATE INDEX`/`ADD CONSTRAINT` and rejects the second as a duplicate
    /// relation. Postgres-only: `SQLite` has no comparable short limit.
    #[error(
        "generated constraint/index name `{name}` is emitted more than once, or exceeds \
         PostgreSQL's 63-byte identifier limit (colliding with another generated name after \
         truncation to 63 bytes); rename the offending field or shorten the table/column names."
    )]
    GeneratedIdentifierTooLong {
        /// The offending generated identifier.
        name: String,
    },

    /// A brand-new managed table whose model has field(s) the schema parser could
    /// not represent (a generated enum, a bare association like `pub user: User`),
    /// so the parser recorded a diagnostic and skipped the column(s) (unsupported
    /// this slice, **no override** — the DDL would be incomplete, not merely lossy).
    /// Emitting `CREATE TABLE` from that partial output would omit the skipped
    /// column(s), but the generated model still queries them, so the app would fail
    /// with "column does not exist" at runtime. A table missing a queried column is
    /// as broken as unappliable SQL, so the engine refuses rather than emit
    /// incomplete DDL. This applies only to a table being CREATED — an existing
    /// table with a skipped column keeps that column via the drop suppressions.
    #[error(
        "cannot generate `CREATE TABLE {table}`: the model has field(s) the schema parser \
         could not represent ({}), so the generated DDL would omit those column(s) and the \
         application would query a missing column at runtime. Write this table's migration \
         manually.",
        .fields.join(", ")
    )]
    CreateTableWithSkippedField {
        /// The table that cannot be safely created.
        table: String,
        /// The parser-skipped field name(s) that would be missing from the DDL.
        fields: Vec<String>,
    },
}

/// A single destructive operation, for [`DiffError::Destructive`] inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestructiveOp {
    /// `DROP TABLE <name>`.
    Table(String),
    /// `DROP COLUMN <table>.<column>`.
    Column {
        /// The owning table.
        table: String,
        /// The dropped column.
        column: String,
    },
}

/// Why a plan cannot be rendered to SQL on its backend.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    /// A change kind that this slice cannot render on `backend` (slice 5 owns the
    /// full `SQLite` `ALTER` path).
    #[error("change kind `{kind}` is not renderable on {backend:?} yet (slice 5 owns full SQLite)")]
    UnsupportedOnBackend {
        /// The unrenderable change kind.
        kind: &'static str,
        /// The backend it could not be rendered on.
        backend: Backend,
    },

    /// Two or more newly-created tables reference each other through inline
    /// foreign keys, so no `CREATE TABLE` order satisfies all of them. Inline
    /// `REFERENCES` cannot express a cycle (it needs a deferred `ALTER TABLE ADD
    /// CONSTRAINT` after all the `CREATE`s, deferred to a later slice), so this
    /// slice refuses rather than emitting a migration that cannot apply.
    #[error(
        "cannot order the new tables for creation: a foreign-key dependency cycle exists among \
         [{}]. Inline-FK cycles are not supported in this slice.",
        .tables.join(", ")
    )]
    CyclicTableDependencies {
        /// The tables participating in the cycle (sorted, for a stable message).
        tables: Vec<String>,
    },

    /// A `SQLite` `ALTER`-family change needs the table-recreate strategy, but the
    /// full table shape required to rebuild it is unavailable (the context-free
    /// [`emit_up_sql`] / [`emit_down_sql`] were used, or the shape is missing from
    /// the [`SchemaContext`]). Use [`emit_up_sql_with_context`] /
    /// [`emit_down_sql_with_context`] with a populated context.
    #[error("cannot rebuild SQLite table `{table}` for change `{kind}`: {reason}")]
    SqliteRebuildUnsupported {
        /// The table that could not be rebuilt.
        table: String,
        /// The kind of rebuild that was attempted.
        kind: &'static str,
        /// Why the rebuild could not be rendered.
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Structural diff
// ---------------------------------------------------------------------------

/// Pure structural diff: `desired − baseline` → [`MigrationPlan`]. NEVER refuses,
/// NEVER does IO. Applies the module's conservatism suppressions and
/// managed-table scoping.
///
/// `desired` is the whole [`ParsedSchema`] (tables **and** diagnostics) because
/// the diagnostics drive the enum-skipped-column suppression (a column the parser
/// skipped must not be diffed as a drop).
///
/// `opts` is accepted for API stability; refusal is [`guard_plan`]'s job, so the
/// pure diff does not consult it.
#[must_use]
pub fn diff_schema(baseline: &[Table], desired: &ParsedSchema, opts: DiffOptions) -> MigrationPlan {
    let _ = opts;
    let backend = plan_backend(baseline, desired);
    let mut changes = Vec::new();

    let baseline_by_name: BTreeMap<&str, &Table> =
        baseline.iter().map(|t| (t.name.as_str(), t)).collect();
    let desired_by_name: BTreeMap<&str, &Table> = desired
        .tables
        .iter()
        .map(|t| (t.name.as_str(), t))
        .collect();

    // Iterate the union of table names in a stable (sorted) order.
    let mut names: Vec<&str> = baseline_by_name
        .keys()
        .chain(desired_by_name.keys())
        .copied()
        .collect();
    names.sort_unstable();
    names.dedup();

    for name in names {
        match (baseline_by_name.get(name), desired_by_name.get(name)) {
            // Present on both sides — diff only Autumn-managed tables.
            (Some(base), Some(want)) => {
                if want.managed {
                    diff_table(base, want, desired, &mut changes);
                }
            }
            // Desired only — create it if Autumn owns it. But if the parser
            // skipped any of the model's fields (a diagnostic for this table), the
            // parsed `Table` is incomplete: emitting `CREATE TABLE` from it would
            // omit those column(s) while the generated model still queries them, so
            // refuse via a non-emittable marker rather than emit broken DDL.
            (None, Some(want)) => {
                if want.managed {
                    let skipped = skipped_columns(&want.name, desired);
                    if skipped.is_empty() {
                        changes.push(SchemaChange::CreateTable((*want).clone()));
                    } else {
                        changes.push(SchemaChange::CreateTableBlockedBySkippedField {
                            table: want.name.clone(),
                            fields: skipped.into_iter().collect(),
                        });
                    }
                }
            }
            // Baseline only — drop it only if Autumn ever owned it.
            (Some(base), None) => {
                if base.managed {
                    changes.push(SchemaChange::DropTable((*base).clone()));
                }
            }
            (None, None) => unreachable!("name came from one of the two maps"),
        }
    }

    detect_inbound_fk_blocks(baseline, desired, &mut changes);
    detect_fk_type_change_blocks(baseline, desired, &mut changes);

    MigrationPlan { backend, changes }
}

/// Post-pass: for every dropped table, flag any table that still holds a foreign
/// key referencing it. Such a `DROP TABLE` is unappliable (PG rejects it and this
/// engine has no `DropForeignKey`), so a non-emittable
/// [`SchemaChange::DropTableBlockedByInboundFk`] marker is appended for
/// [`guard_plan`] to refuse.
///
/// Two referencer sources are scanned: the **baseline** side (a retained table
/// whose pre-existing FK targets a dropped table) and the **desired** side (a new
/// table, or a retained table with a newly-added FK column, referencing a dropped
/// table — the case the baseline scan cannot see, e.g. drop `users` while adding
/// `posts.author_id #[references(table = "users")]`). A dropped table is absent
/// from `desired`, so a self-referential FK and an FK from a co-dropped table are
/// excluded structurally on the desired side; the baseline scan excludes a
/// referencer that is itself being dropped — either way those constraints go away
/// with their table. The baseline scan additionally excludes a referencing
/// **column** that is itself being dropped in the same plan: `up_ordered` emits
/// every `DropColumn` (which removes the FK constraint) before every `DropTable`,
/// so dropping `posts.user_id` *and* `users` together is valid SQL and must not be
/// over-refused (a common combined cleanup). Markers are deduplicated (a
/// pre-existing FK on a retained table appears on both sides) and, via the
/// `BTreeSet`, deterministically sorted by (dropped, referencing table,
/// referencing column).
fn detect_inbound_fk_blocks(
    baseline: &[Table],
    desired: &ParsedSchema,
    changes: &mut Vec<SchemaChange>,
) {
    let dropped: BTreeSet<&str> = changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::DropTable(t) => Some(t.name.as_str()),
            _ => None,
        })
        .collect();
    if dropped.is_empty() {
        return;
    }

    // (referencing table, referencing column) pairs being dropped in this plan.
    // Such a column carries its FK constraint away with it — and `up_ordered`
    // emits every `DropColumn` before every `DropTable`, so the constraint is gone
    // before the referenced table is dropped. Excluding these avoids over-refusing
    // a valid combined "drop the FK column and its target table" cleanup.
    let dropped_columns: BTreeSet<(&str, &str)> = changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::DropColumn { table, column } => {
                Some((table.as_str(), column.name.as_str()))
            }
            _ => None,
        })
        .collect();

    // (dropped table, referencing table, referencing column); BTreeSet dedups the
    // baseline/desired overlap and keeps the emission order deterministic.
    let mut blocks: BTreeSet<(String, String, String)> = BTreeSet::new();

    // Baseline side: a retained table whose baseline FK targets a dropped table.
    for table in baseline {
        // A referencer that is itself being dropped takes its FK with it.
        if dropped.contains(table.name.as_str()) {
            continue;
        }
        for column in &table.columns {
            // A referencing column being dropped in this same plan takes its FK
            // constraint with it (emitted before the DROP TABLE), so it never
            // blocks the drop.
            if dropped_columns.contains(&(table.name.as_str(), column.name.as_str())) {
                continue;
            }
            if let Some(fk) = &column.references
                && dropped.contains(fk.table.as_str())
            {
                blocks.insert((fk.table.clone(), table.name.clone(), column.name.clone()));
            }
        }
    }

    // Desired side: a managed new-or-retained table whose FK targets a dropped
    // table (only managed desired tables emit any SQL). Dropped tables are absent
    // from `desired`, so self-references and co-dropped referencers never appear.
    for table in &desired.tables {
        if !table.managed {
            continue;
        }
        for column in &table.columns {
            if let Some(fk) = &column.references
                && dropped.contains(fk.table.as_str())
            {
                blocks.insert((fk.table.clone(), table.name.clone(), column.name.clone()));
            }
        }
    }

    changes.extend(
        blocks
            .into_iter()
            .map(|(table, referencing_table, referencing_column)| {
                SchemaChange::DropTableBlockedByInboundFk {
                    table,
                    referencing_table,
                    referencing_column,
                }
            }),
    );
}

/// Post-pass: for every `AlterColumnType` change, flag it when the column
/// participates in a foreign-key constraint this engine cannot drop and recreate.
/// Postgres rejects `ALTER COLUMN ... TYPE` on a column bound by an FK — whether
/// the column is the **referencing** column (its own `references` is set) or the
/// **referenced key** (another table's FK targets it) — so the bare
/// `ALTER COLUMN ... TYPE` this engine would emit is unappliable. A non-emittable
/// [`SchemaChange::AlterColumnTypeBlockedByFk`] marker is appended for
/// [`guard_plan`] to refuse (no override). This composes on top of the
/// [`DiffError::NonImplicitTypeConversion`] guard: that guard already refuses
/// non-implicit casts, and this one additionally refuses the implicit widenings
/// (`int4`→`int8`, `float4`→`float8`) it allows, whenever the column is FK-bound.
///
/// The FK scan mirrors [`detect_inbound_fk_blocks`]: baseline retained tables and
/// managed desired tables are both scanned for a referencing column so a
/// pre-existing or newly-declared FK constraint is seen either way. `AlterColumnType`
/// is only produced for a both-present managed table, so the altered column's own
/// table is always retained.
fn detect_fk_type_change_blocks(
    baseline: &[Table],
    desired: &ParsedSchema,
    changes: &mut Vec<SchemaChange>,
) {
    // (table, column) pairs undergoing a type change.
    let type_changes: BTreeSet<(&str, &str)> = changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::AlterColumnType { table, column, .. } => {
                Some((table.as_str(), column.as_str()))
            }
            _ => None,
        })
        .collect();
    if type_changes.is_empty() {
        return;
    }

    let mut blocks: BTreeSet<(String, String)> = BTreeSet::new();
    for &(table, column) in &type_changes {
        if column_participates_in_fk(table, column, baseline, desired) {
            blocks.insert((table.to_owned(), column.to_owned()));
        }
    }

    changes.extend(
        blocks
            .into_iter()
            .map(|(table, column)| SchemaChange::AlterColumnTypeBlockedByFk { table, column }),
    );
}

/// True when `table.column` participates in a foreign-key constraint — either as
/// the **referencing** column (it carries a `references`, baseline or desired) or
/// as the **referenced key** (some other table's FK targets `table.column`). Both
/// the baseline (existing constraints) and the managed desired tables (declared
/// constraints) are scanned, so a constraint that is present in the database, or
/// that the plan adds, both count. Used only to decide the FK-bound type-change
/// refusal, so it is deliberately conservative.
fn column_participates_in_fk(
    table: &str,
    column: &str,
    baseline: &[Table],
    desired: &ParsedSchema,
) -> bool {
    // Referencing side: the altered column itself carries an FK, in the baseline
    // (the constraint exists in the DB) or the desired managed table.
    let is_referencing = baseline
        .iter()
        .filter(|t| t.name == table)
        .flat_map(|t| &t.columns)
        .chain(
            desired
                .tables
                .iter()
                .filter(|t| t.managed && t.name == table)
                .flat_map(|t| &t.columns),
        )
        .any(|c| c.name == column && c.references.is_some());
    if is_referencing {
        return true;
    }

    // Referenced side: any baseline table, or any managed desired table, whose FK
    // targets `table.column`.
    baseline
        .iter()
        .flat_map(|t| &t.columns)
        .chain(
            desired
                .tables
                .iter()
                .filter(|t| t.managed)
                .flat_map(|t| &t.columns),
        )
        .any(|c| {
            c.references
                .as_ref()
                .is_some_and(|fk| fk.table == table && fk.column == column)
        })
}

/// Diff a table present on both sides (already known Autumn-managed on the
/// desired side), pushing column / index / check changes.
fn diff_table(base: &Table, want: &Table, desired: &ParsedSchema, changes: &mut Vec<SchemaChange>) {
    // A primary-key change is refused wholesale (guarded); emit only the marker
    // and skip the rest of this table's diff.
    if base.primary_key != want.primary_key {
        changes.push(SchemaChange::PrimaryKeyChange {
            table: want.name.clone(),
        });
        return;
    }

    let base_cols: BTreeMap<&str, &Column> =
        base.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let want_col_names: std::collections::BTreeSet<&str> =
        want.columns.iter().map(|c| c.name.as_str()).collect();

    // Columns the parser skipped for this table (rule D/E): a baseline column
    // whose name is skipped is "present but unmodelled" and must not be dropped.
    let skipped = skipped_columns(&want.name, desired);

    // Adds and same-name alters, in desired declared order.
    for want_col in &want.columns {
        match base_cols.get(want_col.name.as_str()) {
            None => changes.push(SchemaChange::AddColumn {
                table: want.name.clone(),
                column: want_col.clone(),
            }),
            Some(base_col) => diff_column(&want.name, base_col, want_col, changes),
        }
    }

    // Drops, in baseline declared order — suppressing parser-skipped columns.
    for base_col in &base.columns {
        if !want_col_names.contains(base_col.name.as_str())
            && !skipped.contains(base_col.name.as_str())
        {
            changes.push(SchemaChange::DropColumn {
                table: want.name.clone(),
                column: base_col.clone(),
            });
        }
    }

    diff_indexes(&want.name, base, want, &skipped, changes);
    diff_checks(&want.name, base, want, changes);
}

/// Diff a single same-named column (keyed on name, never position).
fn diff_column(table: &str, base: &Column, want: &Column, changes: &mut Vec<SchemaChange>) {
    if base.ty != want.ty {
        changes.push(SchemaChange::AlterColumnType {
            table: table.to_owned(),
            column: want.name.clone(),
            from: base.ty.clone(),
            to: want.ty.clone(),
        });
    }

    match (base.nullable, want.nullable) {
        (true, false) => changes.push(SchemaChange::SetNotNull {
            table: table.to_owned(),
            column: want.name.clone(),
        }),
        (false, true) => changes.push(SchemaChange::DropNotNull {
            table: table.to_owned(),
            column: want.name.clone(),
        }),
        _ => {}
    }

    // Rule C: only emit when the desired side has a default that differs. A
    // desired `None` is "unknown, retained" — never a DropDefault.
    if let Some(to) = &want.default
        && base.default.as_ref() != Some(to)
    {
        changes.push(SchemaChange::SetDefault {
            table: table.to_owned(),
            column: want.name.clone(),
            to: to.clone(),
            from: base.default.clone(),
        });
    }

    // Rule B: only the *add* direction is ever considered (a desired `None` is
    // "unknown, retained" — never a DropForeignKey). `diff_column` only runs for a
    // column present on BOTH sides — i.e. a pre-existing column — so an added FK
    // here is on a pre-existing column, which is unsafe: the parser cannot see a
    // generated `#[belongs_to(...)]` association FK, so a baseline `references:
    // None` is *unknown*, not proof the DB has no `<table>_<column>_fkey`
    // constraint — emitting `ADD CONSTRAINT` could collide with an existing one.
    //
    // The three sub-cases when the desired side has an explicit FK on a
    // pre-existing column:
    //   * baseline had none            → the FK may (invisibly) already exist →
    //     the refused `AddForeignKeyToExistingColumn` marker (no override).
    //   * baseline had the same FK     → no change.
    //   * baseline had a *different* FK → a retarget we cannot safely emit
    //     (there is no `DropForeignKey`, and re-`ADD CONSTRAINT`-ing the default
    //     `<table>_<column>_fkey` name would collide) → the refused
    //     `ForeignKeyChange` marker, mirroring `PrimaryKeyChange`.
    //
    // A genuinely-new FK column arrives as an `AddColumn` (whose `REFERENCES` is
    // rendered inline, never as a separate `AddForeignKey`), so it never reaches
    // this branch and is emitted normally.
    if let Some(fk) = &want.references {
        match &base.references {
            None => changes.push(SchemaChange::AddForeignKeyToExistingColumn {
                table: table.to_owned(),
                column: want.name.clone(),
            }),
            Some(existing) if existing == fk => {}
            Some(_) => changes.push(SchemaChange::ForeignKeyChange {
                table: table.to_owned(),
                column: want.name.clone(),
            }),
        }
    }
}

/// Diff a table's indexes by name. A same-named index whose shape changed is a
/// drop-then-add (neither is destructive — an index drops no rows).
///
/// A `DropIndex` is **suppressed** when the baseline index references ANY column in
/// `skipped` (a parser-skipped column, per [`skipped_columns`]): the parser never
/// saw the column, so it never saw the column's indexes either — their absence from
/// the desired side is a parser gap, not proof the index was removed. Dropping such
/// an index would silently lose a constraint (e.g. a UNIQUE index → duplicate data
/// becomes possible) the same way an unsuppressed `DropColumn` would lose the
/// column. This mirrors the `DropColumn` suppression in [`diff_table`]. A composite
/// index touching even one skipped column is suppressed whole, since the parser
/// cannot authoritatively say it was removed.
fn diff_indexes(
    table: &str,
    base: &Table,
    want: &Table,
    skipped: &BTreeSet<String>,
    changes: &mut Vec<SchemaChange>,
) {
    let base_by_name: BTreeMap<&str, &Index> =
        base.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
    let want_by_name: BTreeMap<&str, &Index> =
        want.indexes.iter().map(|i| (i.name.as_str(), i)).collect();

    for want_idx in &want.indexes {
        match base_by_name.get(want_idx.name.as_str()) {
            None => changes.push(SchemaChange::AddIndex {
                table: table.to_owned(),
                index: want_idx.clone(),
            }),
            Some(base_idx) if *base_idx != want_idx => {
                changes.push(SchemaChange::DropIndex {
                    table: table.to_owned(),
                    index: (*base_idx).clone(),
                });
                changes.push(SchemaChange::AddIndex {
                    table: table.to_owned(),
                    index: want_idx.clone(),
                });
            }
            Some(_) => {}
        }
    }

    for base_idx in &base.indexes {
        if !want_by_name.contains_key(base_idx.name.as_str())
            && !base_idx.columns.iter().any(|c| skipped.contains(c))
        {
            changes.push(SchemaChange::DropIndex {
                table: table.to_owned(),
                index: base_idx.clone(),
            });
        }
    }
}

/// Diff a table's checks — **add only** (rule A: a baseline-only check is never
/// dropped, because the parser cannot fabricate enum checks and so cannot prove
/// the check was removed).
fn diff_checks(table: &str, base: &Table, want: &Table, changes: &mut Vec<SchemaChange>) {
    for want_check in &want.checks {
        if !base.checks.iter().any(|c| c == want_check) {
            changes.push(SchemaChange::AddCheck {
                table: table.to_owned(),
                check: want_check.clone(),
            });
        }
    }
}

/// The set of column names the parser skipped for `table` (rule E): a
/// [`SchemaDiagnostic`](crate::schema::parse::SchemaDiagnostic) whose resolved
/// table name equals `table` contributes its field name. Such a baseline column
/// is "present but unmodelled" and must not be diffed as a drop.
///
/// The match is on the diagnostic's resolved `table` (the `#[model(table =
/// "...")]` override or the convention name), recorded at parse time — never a
/// re-derivation of the convention name from `model`, which would miss a custom
/// table name and let a real baseline column be diffed as a data-losing drop.
fn skipped_columns(table: &str, desired: &ParsedSchema) -> std::collections::BTreeSet<String> {
    desired
        .diagnostics
        .iter()
        .filter(|d| d.table == table)
        .map(|d| d.field.clone())
        .collect()
}

/// Infer the plan's backend from the (provider-locked) tables. The caller
/// guarantees both sides share a backend via `ensure_backend_matches`; defaulting
/// to Postgres only matters for the (backendless) empty-vs-empty case.
fn plan_backend(baseline: &[Table], desired: &ParsedSchema) -> Backend {
    desired
        .tables
        .first()
        .or_else(|| baseline.first())
        .map_or(Backend::Postgres, |t| t.backend)
}

// ---------------------------------------------------------------------------
// Policy guard
// ---------------------------------------------------------------------------

/// Policy guard, run AFTER [`diff_schema`] by the command. Refuses a plan that is
/// structurally computable but unsafe to emit unless permitted.
///
/// Guard order (most specific / most dangerous first, so the user sees the most
/// actionable message): **`PrimaryKeyChange` → `ForeignKeyChange` →
/// `PossibleRename` → `Destructive`**. `--allow-destructive` overrides the rename
/// and destructive tiers; a primary-key change and a foreign-key retarget have no
/// override.
///
/// # Errors
///
/// Returns a [`DiffError`] describing the first refusal; `Ok(())` for an
/// emittable plan (including the empty no-op plan).
pub fn guard_plan(plan: &MigrationPlan, opts: DiffOptions) -> Result<(), DiffError> {
    // 1. Primary-key change — no override.
    if let Some(table) = plan.changes.iter().find_map(|c| match c {
        SchemaChange::PrimaryKeyChange { table } => Some(table.clone()),
        _ => None,
    }) {
        return Err(DiffError::PrimaryKeyChange { table });
    }

    // 2. Foreign-key retarget — no override (there is no safe drop+recreate).
    if let Some((table, column)) = plan.changes.iter().find_map(|c| match c {
        SchemaChange::ForeignKeyChange { table, column } => Some((table.clone(), column.clone())),
        _ => None,
    }) {
        return Err(DiffError::ForeignKeyChange { table, column });
    }

    // 2b. Foreign key added to a pre-existing column — no override. A baseline
    //     `references: None` is *unknown* (the parser cannot see an association
    //     FK), so the DB may already carry the `<table>_<column>_fkey` constraint;
    //     emitting `ADD CONSTRAINT` would collide. A new FK column (an `AddColumn`
    //     with an inline `REFERENCES`) is safe and never reaches this marker.
    if let Some((table, column)) = plan.changes.iter().find_map(|c| match c {
        SchemaChange::AddForeignKeyToExistingColumn { table, column } => {
            Some((table.clone(), column.clone()))
        }
        _ => None,
    }) {
        return Err(DiffError::AddForeignKeyToPreexistingColumn { table, column });
    }

    // 2c. Create of a table whose model has parser-skipped field(s) — no override.
    //     The parsed table omits the skipped column(s), so emitting `CREATE TABLE`
    //     from it would produce DDL missing a column the generated model queries →
    //     runtime "column does not exist". Incomplete DDL is as broken as
    //     unappliable SQL, so it refuses and directs to a manual migration. Only a
    //     table being CREATED carries this marker (an existing table's skipped
    //     column is retained by the drop suppressions, not this refusal).
    if let Some((table, fields)) = plan.changes.iter().find_map(|c| match c {
        SchemaChange::CreateTableBlockedBySkippedField { table, fields } => {
            Some((table.clone(), fields.clone()))
        }
        _ => None,
    }) {
        return Err(DiffError::CreateTableWithSkippedField { table, fields });
    }

    // 3. Drop of a table a retained table still references — no override (even
    //    --allow-destructive cannot break another table's integrity).
    if let Some(err) = find_inbound_fk_block(plan) {
        return Err(err);
    }

    // 4. Non-implicit column type conversion (pg) — no override; the bare
    //    `ALTER COLUMN ... TYPE` is unappliable without a `USING` clause.
    if let Some(err) = find_non_implicit_conversion(plan) {
        return Err(err);
    }

    // 4b. Column type change on an FK-bound column — no override. PG rejects
    //     `ALTER COLUMN ... TYPE` on a column bound by a foreign key (as either the
    //     referencing column or the referenced key), and this engine has no
    //     drop+recreate path for the constraint. Composes on top of guard 4: it
    //     catches even the implicit widenings that guard allows.
    if let Some(err) = find_fk_type_change_block(plan) {
        return Err(err);
    }

    // 4c. Generated identifier over Postgres's 63-byte limit (or a post-truncation
    //     collision) — no override. PG silently truncates identifiers to 63 bytes,
    //     so an over-long FK-constraint / index name is renamed on apply and can
    //     collide with a sibling generated name (duplicate-relation error). Unappliable
    //     SQL, not merely lossy, so it always refuses. Postgres-only (SQLite does not
    //     truncate). See the module docs' 63-byte-limit section.
    if let Some(err) = find_identifier_limit_violation(plan) {
        return Err(err);
    }

    // 5. Possible rename — a single table that both dropped and added columns.
    //    Overridable, so it is only enforced when destructive changes are not
    //    allowed. Deliberately checked *before* the required-column guard below so
    //    a rename-shaped drop+add reports the more-actionable `#[renamed_from]`
    //    guidance rather than "add a default".
    if !opts.allow_destructive
        && let Some(err) = find_possible_rename(plan)
    {
        return Err(err);
    }

    // 6. Add of a required column (NOT NULL, no default) to an existing table —
    //    no override. Postgres validates the NOT NULL against existing rows the
    //    instant the column is added, so `ADD COLUMN ... NOT NULL` fails on any
    //    non-empty table; the offline engine has no backfill value to synthesize,
    //    so it refuses rather than emit an unappliable migration. This matches
    //    only `AddColumn` (altering an existing baseline table) — a NOT NULL,
    //    no-default column inlined in a brand-new `CreateTable` is empty-table-safe
    //    and is never matched here. It is not destructive, so it always refuses,
    //    regardless of `--allow-destructive` (checked in both branches: the
    //    `allow_destructive` short-circuit below is only reached after this guard).
    if let Some((table, column)) = plan.changes.iter().find_map(|c| match c {
        SchemaChange::AddColumn { table, column }
            if !column.nullable && column.default.is_none() =>
        {
            Some((table.clone(), column.name.clone()))
        }
        _ => None,
    }) {
        return Err(DiffError::RequiredColumnWithoutDefault { table, column });
    }

    // 6b. `SET NOT NULL` on an existing nullable column — no override. Postgres
    //     validates the constraint against every existing row the instant the
    //     ALTER runs, so it fails on any pre-existing NULL; the offline engine
    //     has no backfill value to synthesize. The exact sibling of the
    //     required-column refusal above. It is not destructive, so it always
    //     refuses, regardless of `--allow-destructive` (checked here, before the
    //     `allow_destructive` short-circuit below). The inverse `DropNotNull`
    //     (nullable-ing a column) is always safe and is never matched here.
    if let Some((table, column)) = plan.changes.iter().find_map(|c| match c {
        SchemaChange::SetNotNull { table, column } => Some((table.clone(), column.clone())),
        _ => None,
    }) {
        return Err(DiffError::SetNotNullRequiresBackfill { table, column });
    }

    // 6c. A unique index added to a pre-existing table where ALL its indexed
    //     columns already existed — no override. `CREATE UNIQUE INDEX` validates
    //     uniqueness against every existing row the instant it runs, so it fails
    //     on any pre-existing duplicate; the offline engine cannot dedup the data.
    //     The index-level sibling of the `SET NOT NULL` refusal above. It is not
    //     destructive, so it always refuses, regardless of `--allow-destructive`
    //     (checked here, before the `allow_destructive` short-circuit below). A
    //     unique index on a brand-new `CreateTable` (empty) rides inline on that
    //     change and never reaches `AddIndex`; a unique index touching a
    //     newly-added column (whose existing rows are all NULL, treated as
    //     distinct) and any non-unique index are safe and are never matched here.
    if let Some(err) = find_unique_index_requires_dedup(plan) {
        return Err(err);
    }

    if opts.allow_destructive {
        return Ok(());
    }

    // 7. Destructive drops.
    let destructive_ops: Vec<DestructiveOp> = plan
        .changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::DropTable(t) => Some(DestructiveOp::Table(t.name.clone())),
            SchemaChange::DropColumn { table, column } => Some(DestructiveOp::Column {
                table: table.clone(),
                column: column.name.clone(),
            }),
            _ => None,
        })
        .collect();
    if !destructive_ops.is_empty() {
        let summary = destructive_ops
            .iter()
            .map(|op| match op {
                DestructiveOp::Table(t) => format!("DROP TABLE {t}"),
                DestructiveOp::Column { table, column } => {
                    format!("DROP COLUMN {table}.{column}")
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(DiffError::Destructive {
            summary,
            ops: destructive_ops,
        });
    }

    Ok(())
}

/// The [`DiffError::DropTableInboundReference`] refusal for the first
/// [`SchemaChange::DropTableBlockedByInboundFk`] marker in `plan`, if any.
fn find_inbound_fk_block(plan: &MigrationPlan) -> Option<DiffError> {
    plan.changes.iter().find_map(|c| match c {
        SchemaChange::DropTableBlockedByInboundFk {
            table,
            referencing_table,
            referencing_column,
        } => Some(DiffError::DropTableInboundReference {
            table: table.clone(),
            referencing_table: referencing_table.clone(),
            referencing_column: referencing_column.clone(),
        }),
        _ => None,
    })
}

/// The [`DiffError::TypeChangeOnForeignKeyColumn`] refusal for the first
/// [`SchemaChange::AlterColumnTypeBlockedByFk`] marker in `plan`, if any.
fn find_fk_type_change_block(plan: &MigrationPlan) -> Option<DiffError> {
    plan.changes.iter().find_map(|c| match c {
        SchemaChange::AlterColumnTypeBlockedByFk { table, column } => {
            Some(DiffError::TypeChangeOnForeignKeyColumn {
                table: table.clone(),
                column: column.clone(),
            })
        }
        _ => None,
    })
}

/// The [`DiffError::UniqueIndexRequiresDedup`] refusal for the first unique
/// `AddIndex` in `plan` whose indexed columns ALL pre-existed on the table — none
/// is a column being added in this same plan.
///
/// An `AddIndex` is produced by [`diff_indexes`] only for a both-present
/// (pre-existing) table; a brand-new table's indexes ride inline on its
/// [`SchemaChange::CreateTable`] and never reach this path — so a unique
/// `AddIndex` already implies the table pre-exists and may hold rows. It is unsafe
/// (existing rows may contain duplicates that fail `CREATE UNIQUE INDEX`) UNLESS
/// at least one indexed column is newly added in this plan (an `AddColumn`): a
/// new column is NULL in every existing row, and Postgres treats NULLs as
/// distinct in a unique index, so the existing rows cannot conflict.
fn find_unique_index_requires_dedup(plan: &MigrationPlan) -> Option<DiffError> {
    let added: BTreeSet<(&str, &str)> = plan
        .changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::AddColumn { table, column } => {
                Some((table.as_str(), column.name.as_str()))
            }
            _ => None,
        })
        .collect();
    plan.changes.iter().find_map(|c| match c {
        SchemaChange::AddIndex { table, index } if index.unique => {
            let touches_new_column = index
                .columns
                .iter()
                .any(|col| added.contains(&(table.as_str(), col.as_str())));
            (!touches_new_column).then(|| DiffError::UniqueIndexRequiresDedup {
                table: table.clone(),
                index: index.name.clone(),
            })
        }
        _ => None,
    })
}

/// Postgres's identifier byte limit (`NAMEDATALEN - 1`). Names longer than this are
/// silently truncated on apply.
const PG_MAX_IDENTIFIER_BYTES: usize = 63;

/// The identifiers this engine generates for a plan, in change order: the
/// `{table}_{column}_fkey` FK-constraint name for each [`SchemaChange::AddForeignKey`]
/// (rendered as `ADD CONSTRAINT` up / `DROP CONSTRAINT` down, both from this same
/// derivation), and every index name emitted as a top-level `CREATE INDEX` — the
/// [`SchemaChange::AddIndex`] index name plus each index on a [`SchemaChange::CreateTable`].
/// These are exactly the names Postgres would truncate to 63 bytes.
fn generated_identifiers(plan: &MigrationPlan) -> Vec<String> {
    let mut names = Vec::new();
    for change in &plan.changes {
        match change {
            SchemaChange::AddForeignKey { table, column, .. } => {
                // The FK-constraint name is engine-generated and safe to rename, so it
                // goes through `bounded_pg_identifier` (matching the `ADD`/`DROP
                // CONSTRAINT` emitters). A short name is returned unchanged; only an
                // over-limit one is truncate+hashed — so this never trips the guard.
                names.push(bounded_pg_identifier(&format!("{table}_{column}_fkey")));
            }
            SchemaChange::AddIndex { index, .. } => names.push(index.name.clone()),
            SchemaChange::CreateTable(table) => {
                names.extend(table.indexes.iter().map(|i| i.name.clone()));
            }
            _ => {}
        }
    }
    names
}

/// `name` truncated to at most `max` bytes, on a UTF-8 char boundary.
fn truncate_to_bytes(name: &str, max: usize) -> &str {
    if name.len() <= max {
        return name;
    }
    let mut end = max;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// `name` truncated to Postgres's 63-byte identifier limit, on a UTF-8 char boundary
/// (PG truncates by byte; generated names are ASCII, but stay codepoint-safe anyway).
fn truncate_pg_identifier(name: &str) -> &str {
    truncate_to_bytes(name, PG_MAX_IDENTIFIER_BYTES)
}

/// A short, deterministic 8-hex-char digest of `raw`, for collision-resistant
/// identifier suffixing.
///
/// Uses a hand-rolled FNV-1a 64-bit hash (fully specified, dependency-free) rather
/// than `DefaultHasher`, whose output is not guaranteed stable across Rust versions —
/// this schema emitter must derive the same suffix on every toolchain, and the up and
/// down renderers must agree.
fn short_identifier_hash(raw: &str) -> String {
    let digest = fnv1a_64(raw.as_bytes());
    format!("{digest:016x}")[..8].to_owned()
}

/// FNV-1a 64-bit hash of `bytes` (offset basis `0xcbf29ce484222325`, prime
/// `0x100000001b3`), fully specified so the digest is deterministic everywhere.
const fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let prime: u64 = 0x0000_0100_0000_01b3;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(prime);
        i += 1;
    }
    hash
}

/// A Postgres-safe (`≤ 63`-byte) identifier for `raw`.
///
/// Returns `raw` unchanged when it already fits, so short names — the common case —
/// are byte-stable. An over-limit name is truncated and suffixed with
/// `_<8-hex-hash>` of the **full untruncated** name, yielding a deterministic,
/// collision-resistant identifier that always fits. Applied only to
/// engine-generated FK-constraint names (safe to rename), never to parser-provided
/// index names.
fn bounded_pg_identifier(raw: &str) -> String {
    if raw.len() <= PG_MAX_IDENTIFIER_BYTES {
        return raw.to_owned();
    }
    let hash = short_identifier_hash(raw);
    // suffix is "_" + 8 hex chars = 9 bytes; leave room for it within the 63 limit.
    let head_budget = PG_MAX_IDENTIFIER_BYTES - (hash.len() + 1);
    let head = truncate_to_bytes(raw, head_budget);
    format!("{head}_{hash}")
}

/// The [`DiffError::GeneratedIdentifierTooLong`] refusal when the plan generates an
/// FK-constraint or index identifier over Postgres's 63-byte limit, two distinct
/// generated names that collide after truncation to 63 bytes, or two generated names
/// that are *exactly* identical. Postgres-only — `SQLite` does not truncate identifiers
/// to a short limit, so the truncation hazard does not apply there (an exact-duplicate
/// relation name would still be rejected, but the parser only produces the colliding
/// `idx_<table>_<field>_unique` shape for a Postgres plan).
fn find_identifier_limit_violation(plan: &MigrationPlan) -> Option<DiffError> {
    if plan.backend != Backend::Postgres {
        return None;
    }
    let mut by_truncated: BTreeMap<String, String> = BTreeMap::new();
    for name in generated_identifiers(plan) {
        // Over the limit: PG truncates it silently — refuse outright (this alone
        // catches every *distinct*-name post-truncation collision, since such a
        // collision requires at least one name to exceed 63 bytes).
        if name.len() > PG_MAX_IDENTIFIER_BYTES {
            return Some(DiffError::GeneratedIdentifierTooLong { name });
        }
        // Any collision on the on-apply (truncated) identifier is a duplicate-relation
        // hazard: PG accepts the first `CREATE INDEX`/`ADD CONSTRAINT` and rejects the
        // second. Because over-limit names are refused above, every name here truncates
        // to itself — so this fires on an *exact duplicate* generated name (e.g. a
        // `#[unique] foo` field plus a separate field named `foo_unique`, both yielding
        // `idx_<table>_foo_unique`), which PG would reject as a duplicate relation. This
        // must NOT dedup or skip the `prev == name` case: identical names are precisely
        // the hazard.
        if by_truncated
            .insert(truncate_pg_identifier(&name).to_owned(), name.clone())
            .is_some()
        {
            return Some(DiffError::GeneratedIdentifierTooLong { name });
        }
    }
    None
}

/// The [`DiffError::PossibleRename`] refusal for the first both-present table that
/// both dropped and added column(s) — the ambiguous drop+add that might be a
/// rename. Grouped in a `BTreeMap` so the reported table is deterministic
/// (sorted). The caller only consults this when `--allow-destructive` is off.
fn find_possible_rename(plan: &MigrationPlan) -> Option<DiffError> {
    let mut per_table: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for change in &plan.changes {
        match change {
            SchemaChange::DropColumn { table, column } => {
                per_table
                    .entry(table.clone())
                    .or_default()
                    .0
                    .push(column.name.clone());
            }
            SchemaChange::AddColumn { table, column } => {
                per_table
                    .entry(table.clone())
                    .or_default()
                    .1
                    .push(column.name.clone());
            }
            _ => {}
        }
    }
    per_table
        .into_iter()
        .find(|(_, (dropped, added))| !dropped.is_empty() && !added.is_empty())
        .map(|(table, (dropped, added))| DiffError::PossibleRename {
            table,
            dropped,
            added,
        })
}

/// The [`DiffError::NonImplicitTypeConversion`] refusal for the first
/// non-implicit `AlterColumnType` in `plan`, if any. Postgres-only: on `SQLite`
/// every `AlterColumnType` is realised by the table-recreate path (which restates
/// the whole column shape, so an implicit-vs-explicit cast distinction does not
/// apply), so the classifier stays out of the `SQLite` boundary.
fn find_non_implicit_conversion(plan: &MigrationPlan) -> Option<DiffError> {
    if plan.backend != Backend::Postgres {
        return None;
    }
    plan.changes.iter().find_map(|c| match c {
        SchemaChange::AlterColumnType {
            table,
            column,
            from,
            to,
        } if !is_implicit_pg_type_cast(from, to) => Some(DiffError::NonImplicitTypeConversion {
            table: table.clone(),
            column: column.clone(),
            from: from.sql_type(plan.backend),
            to: to.sql_type(plan.backend),
        }),
        _ => None,
    })
}

/// True when PG casts `from` → `to` **implicitly**, without a `USING`
/// clause — the only type changes this slice renders as a bare
/// `ALTER COLUMN ... TYPE`. Deliberately conservative: only the lossless numeric
/// widenings PG auto-casts (`int4` → `int8`, `float4` → `float8`). Every other
/// pair (e.g. `TEXT` → `INTEGER`/`UUID`, `BOOLEAN` → `INTEGER`, `NUMERIC`
/// narrowing) needs a manual `USING` migration and is refused by [`guard_plan`].
/// The IR does not distinguish `TEXT` from a `VARCHAR` family, so no string
/// widening pair exists to admit here.
const fn is_implicit_pg_type_cast(from: &ColumnType, to: &ColumnType) -> bool {
    matches!(
        (from, to),
        (ColumnType::Int32, ColumnType::Int64) | (ColumnType::Float32, ColumnType::Float64)
    )
}

// ---------------------------------------------------------------------------
// SQL emission (pg + sqlite)
// ---------------------------------------------------------------------------

/// Full desired + baseline table shapes, keyed by table name.
///
/// The `SQLite` table-recreate path needs a table's complete shape (all columns,
/// PK, checks, indexes), which the per-change [`SchemaChange`] deltas don't carry,
/// so the schema-aware [`emit_up_sql_with_context`] / [`emit_down_sql_with_context`]
/// entry points thread it in. Postgres never rebuilds and so ignores it.
#[derive(Debug, Clone, Default)]
pub struct SchemaContext {
    /// Desired (post-migration) tables, by name — the "new shape" for an up rebuild.
    pub desired: BTreeMap<String, Table>,
    /// Baseline (pre-migration) tables, by name — the "new shape" for a down rebuild.
    pub baseline: BTreeMap<String, Table>,
}

impl SchemaContext {
    /// Build a context from the desired + baseline table lists (as passed to
    /// [`diff_schema`]).
    #[must_use]
    pub fn from_tables(desired: &[Table], baseline: &[Table]) -> Self {
        Self {
            desired: desired
                .iter()
                .map(|t| (t.name.clone(), t.clone()))
                .collect(),
            baseline: baseline
                .iter()
                .map(|t| (t.name.clone(), t.clone()))
                .collect(),
        }
    }
}

/// Render the plan's forward SQL without a schema context.
///
/// Sufficient for Postgres and the portable `SQLite` subset. If the plan needs a
/// `SQLite` table rebuild, prefer [`emit_up_sql_with_context`].
///
/// # Errors
///
/// Returns [`EmitError::SqliteRebuildUnsupported`] when a `SQLite` `ALTER`-family
/// change needs a table rebuild but no shape context was supplied, or
/// [`EmitError::CyclicTableDependencies`] on an unorderable new-table FK cycle.
// Retained context-free public API (the command wiring and the SQLite rebuild path
// use the `_with_context` entry points; this signature is consumed by the parallel
// slice-6 lane and the diff tests).
#[allow(dead_code)]
pub fn emit_up_sql(plan: &MigrationPlan) -> Result<String, EmitError> {
    emit_up_sql_with_context(plan, &SchemaContext::default())
}

/// Render the plan's reverse SQL without a schema context.
///
/// # Errors
///
/// See [`emit_up_sql`].
#[allow(dead_code)]
pub fn emit_down_sql(plan: &MigrationPlan) -> Result<String, EmitError> {
    emit_down_sql_with_context(plan, &SchemaContext::default())
}

/// Render the plan's forward SQL, using `ctx` for any `SQLite` table rebuild.
///
/// Postgres emits the per-change `ALTER`-family statements directly; `SQLite`
/// coalesces every `ALTER`-family change on a table into a single table-recreate
/// block built from `ctx.desired`.
///
/// # Errors
///
/// Returns [`EmitError::SqliteRebuildUnsupported`] when a rebuild is required but
/// the table's desired/baseline shape is missing from `ctx`, or
/// [`EmitError::CyclicTableDependencies`] on an unorderable new-table FK cycle.
pub fn emit_up_sql_with_context(
    plan: &MigrationPlan,
    ctx: &SchemaContext,
) -> Result<String, EmitError> {
    match plan.backend {
        Backend::Postgres => emit_up_sql_pg(plan),
        Backend::Sqlite => emit_up_sql_sqlite(plan, ctx),
    }
}

/// Render the plan's reverse SQL, using `ctx` for any `SQLite` table rebuild.
///
/// # Errors
///
/// See [`emit_up_sql_with_context`].
pub fn emit_down_sql_with_context(
    plan: &MigrationPlan,
    ctx: &SchemaContext,
) -> Result<String, EmitError> {
    match plan.backend {
        Backend::Postgres => emit_down_sql_pg(plan),
        Backend::Sqlite => emit_down_sql_sqlite(plan, ctx),
    }
}

/// The Postgres forward path: one statement group per change, canonical up-order,
/// groups blank-line separated. Unchanged from the pg-first slice.
fn emit_up_sql_pg(plan: &MigrationPlan) -> Result<String, EmitError> {
    let ordered = up_ordered(&plan.changes)?;
    let mut groups = Vec::new();
    for change in ordered {
        let sql = emit_change_up(change, plan.backend)?;
        let sql = sql.trim_end();
        if !sql.is_empty() {
            groups.push(sql.to_owned());
        }
    }
    Ok(join_groups(&groups))
}

/// The Postgres reverse path: changes reversed and individually inverted, with
/// `-- irreversible:` markers where data cannot round-trip.
fn emit_down_sql_pg(plan: &MigrationPlan) -> Result<String, EmitError> {
    let mut ordered = up_ordered(&plan.changes)?;
    ordered.reverse();
    let mut groups = Vec::new();
    for change in ordered {
        let sql = emit_change_down(change, plan.backend)?;
        let sql = sql.trim_end();
        if !sql.is_empty() {
            groups.push(sql.to_owned());
        }
    }
    Ok(join_groups(&groups))
}

/// The change kinds whose `SQLite` realisation is a full table recreate: the
/// `ALTER`-family Postgres would express as `ALTER COLUMN` / `ADD CHECK` /
/// `ADD CONSTRAINT`, none of which `SQLite` supports.
fn sqlite_rebuild_tables(changes: &[SchemaChange]) -> BTreeSet<String> {
    changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::AlterColumnType { table, .. }
            | SchemaChange::DropNotNull { table, .. }
            | SchemaChange::SetDefault { table, .. }
            | SchemaChange::AddCheck { table, .. }
            | SchemaChange::AddForeignKey { table, .. } => Some(table.clone()),
            _ => None,
        })
        .collect()
}

/// The table a change targets, for rebuild-membership tests.
const fn change_table_name(change: &SchemaChange) -> &str {
    match change {
        SchemaChange::CreateTable(t) | SchemaChange::DropTable(t) => t.name.as_str(),
        SchemaChange::AddColumn { table, .. }
        | SchemaChange::DropColumn { table, .. }
        | SchemaChange::AlterColumnType { table, .. }
        | SchemaChange::SetNotNull { table, .. }
        | SchemaChange::DropNotNull { table, .. }
        | SchemaChange::SetDefault { table, .. }
        | SchemaChange::AddForeignKey { table, .. }
        | SchemaChange::AddIndex { table, .. }
        | SchemaChange::DropIndex { table, .. }
        | SchemaChange::AddCheck { table, .. }
        | SchemaChange::PrimaryKeyChange { table }
        | SchemaChange::ForeignKeyChange { table, .. }
        | SchemaChange::DropTableBlockedByInboundFk { table, .. }
        | SchemaChange::AlterColumnTypeBlockedByFk { table, .. }
        | SchemaChange::AddForeignKeyToExistingColumn { table, .. }
        | SchemaChange::CreateTableBlockedBySkippedField { table, .. } => table.as_str(),
    }
}

/// The `SQLite` forward path. Every `ALTER`-family change on a table is coalesced
/// into a single table-recreate block (emitted once, at the first change touching
/// that table); all of that table's other changes are folded into the recreate,
/// which already targets its full desired shape. Non-rebuild tables emit through
/// the portable per-change path unchanged.
fn emit_up_sql_sqlite(plan: &MigrationPlan, ctx: &SchemaContext) -> Result<String, EmitError> {
    let ordered = up_ordered(&plan.changes)?;
    let rebuild = sqlite_rebuild_tables(&plan.changes);
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut groups = Vec::new();
    for change in &ordered {
        let table = change_table_name(change);
        if rebuild.contains(table) {
            if emitted.insert(table.to_owned()) {
                let block = emit_sqlite_rebuild(table, ctx, &ordered, RebuildLeg::Up)?;
                let block = block.trim_end();
                if !block.is_empty() {
                    groups.push(block.to_owned());
                }
            }
            continue;
        }
        let sql = emit_change_up(change, plan.backend)?;
        let sql = sql.trim_end();
        if !sql.is_empty() {
            groups.push(sql.to_owned());
        }
    }
    Ok(join_groups(&groups))
}

/// The `SQLite` reverse path: the mirror of [`emit_up_sql_sqlite`], with each
/// rebuild block rolled back to the table's baseline shape.
fn emit_down_sql_sqlite(plan: &MigrationPlan, ctx: &SchemaContext) -> Result<String, EmitError> {
    let mut ordered = up_ordered(&plan.changes)?;
    ordered.reverse();
    let rebuild = sqlite_rebuild_tables(&plan.changes);
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut groups = Vec::new();
    for change in &ordered {
        let table = change_table_name(change);
        if rebuild.contains(table) {
            if emitted.insert(table.to_owned()) {
                let block = emit_sqlite_rebuild(table, ctx, &ordered, RebuildLeg::Down)?;
                let block = block.trim_end();
                if !block.is_empty() {
                    groups.push(block.to_owned());
                }
            }
            continue;
        }
        let sql = emit_change_down(change, plan.backend)?;
        let sql = sql.trim_end();
        if !sql.is_empty() {
            groups.push(sql.to_owned());
        }
    }
    Ok(join_groups(&groups))
}

/// Which leg a `SQLite` rebuild renders — governs the target shape and the leading
/// marker.
#[derive(Debug, Clone, Copy)]
enum RebuildLeg {
    /// Rebuild to the post-migration shape (`ctx.baseline` + diff deltas).
    Up,
    /// Rebuild back to the baseline shape (`ctx.baseline`).
    Down,
}

/// Render the `SQLite` table-recreate block for `table` on the given leg, drawing
/// the target and source shapes from `ctx` and, on the up leg, the plan's own
/// `changes`.
///
/// The block `CREATE`s a `{table}__autumn_new` staging table with the target shape,
/// copies the columns common to both shapes, drops the old table, renames the
/// staging table into place, recreates the target shape's indexes, and validates
/// referential integrity — all wrapped in `PRAGMA foreign_keys` toggles.
///
/// The **up** leg's target shape is the rich baseline table with this table's
/// explicit diff deltas folded in ([`baseline_with_changes_applied`]) — **not** the
/// parser's partial `ctx.desired` table — so any baseline facet the parser cannot
/// see (a non-convention default, a skipped `CHECK`/enum, an association foreign
/// key) survives a rebuild triggered by an unrelated change. The **down** leg
/// targets the baseline shape directly and copies from the actual post-migration
/// shape (`baseline` + this table's diff deltas — the same value the up leg builds
/// as its target), **not** the parser's partial `ctx.desired`, so a
/// retained-but-parser-invisible column is copied back on rollback rather than
/// dropped.
fn emit_sqlite_rebuild(
    table: &str,
    ctx: &SchemaContext,
    changes: &[&SchemaChange],
    leg: RebuildLeg,
) -> Result<String, EmitError> {
    // The `{table}__autumn_new` staging name is not guaranteed collision-free: if a
    // real table already carries that name, the rebuild's CREATE/RENAME would clobber
    // it. Refuse rather than emit unappliable SQL.
    let staging = format!("{table}__autumn_new");
    if ctx.desired.contains_key(&staging) || ctx.baseline.contains_key(&staging) {
        return Err(EmitError::SqliteRebuildUnsupported {
            table: table.to_owned(),
            kind: "table rebuild",
            reason: format!(
                "staging table name `{staging}` collides with an existing table; \
                 rename the model to free that name"
            ),
        });
    }
    // Fetched as a validated precondition (a missing desired shape is a directed
    // error, exercised by the up leg's missing-context path); neither leg copies
    // from the parser's partial desired table any more.
    let _desired = ctx
        .desired
        .get(table)
        .ok_or_else(|| missing_rebuild_shape(table, "desired"))?;
    let baseline = ctx
        .baseline
        .get(table)
        .ok_or_else(|| missing_rebuild_shape(table, "baseline"))?;
    match leg {
        // Build the up-leg target from the rich baseline with this table's explicit
        // diff deltas applied, preserving every unchanged baseline facet the parser
        // could not observe; the source (INSERT..SELECT) columns are the baseline's,
        // i.e. the actual existing table.
        RebuildLeg::Up => {
            let new_shape = baseline_with_changes_applied(baseline, changes, table);
            Ok(render_sqlite_rebuild(table, &new_shape, baseline, leg))
        }
        // The down leg rolls back to the rich baseline shape. Its copy SOURCE is the
        // actual post-migration table — `baseline + this table's diff deltas`, the same
        // shape the up leg built as its target — NOT the parser's partial `ctx.desired`:
        // a retained-but-parser-invisible column (a skipped enum/association) is absent
        // from `desired`, so sourcing from it would drop that column from the rollback
        // INSERT..SELECT and lose its data. Intersecting the baseline target with the
        // post-migration source copies back every baseline column the up migration did
        // not drop, in baseline order.
        RebuildLeg::Down => {
            let post_migration = baseline_with_changes_applied(baseline, changes, table);
            Ok(render_sqlite_rebuild(table, baseline, &post_migration, leg))
        }
    }
}

/// Apply the table's own plan changes to its baseline shape, preserving every
/// baseline facet the diff did not explicitly change (parser-invisible defaults,
/// `CHECK`s, and so on) — the `SQLite` recreate's "new shape" for the up leg.
///
/// The slice-2 parser is deliberately partial (see the module header): it cannot
/// see a non-convention `#[default]`, some `CHECK`/enum facets, or an association
/// foreign key, and this engine has no `DropDefault` / `DropCheck` /
/// `DropForeignKey` variant. Rebuilding the whole table from the raw `ctx.desired`
/// (partial) shape would therefore silently drop any baseline facet the parser
/// could not observe. Folding the explicit diff deltas into the rich baseline
/// clone instead leaves every unchanged baseline facet intact, upholding the
/// engine's "never destroy parser-invisible state" invariant that the Postgres
/// per-change `ALTER` path already honours.
///
/// Only changes whose table is `table` are folded in; anything else (including the
/// non-emittable guard markers) is ignored.
fn baseline_with_changes_applied(
    baseline: &Table,
    changes: &[&SchemaChange],
    table: &str,
) -> Table {
    let mut shape = baseline.clone();
    for change in changes {
        if change_table_name(change) != table {
            continue;
        }
        match change {
            SchemaChange::AddColumn { column, .. } => shape.columns.push(column.clone()),
            SchemaChange::DropColumn { column, .. } => {
                shape.columns.retain(|c| c.name != column.name);
                shape.primary_key.retain(|n| n != &column.name);
            }
            SchemaChange::AlterColumnType { column, to, .. } => {
                if let Some(c) = shape.columns.iter_mut().find(|c| &c.name == column) {
                    c.ty = to.clone();
                }
            }
            SchemaChange::DropNotNull { column, .. } => {
                if let Some(c) = shape.columns.iter_mut().find(|c| &c.name == column) {
                    c.nullable = true;
                }
            }
            SchemaChange::SetNotNull { column, .. } => {
                if let Some(c) = shape.columns.iter_mut().find(|c| &c.name == column) {
                    c.nullable = false;
                }
            }
            SchemaChange::SetDefault { column, to, .. } => {
                if let Some(c) = shape.columns.iter_mut().find(|c| &c.name == column) {
                    c.default = Some(to.clone());
                }
            }
            SchemaChange::AddCheck { check, .. } => shape.checks.push(check.clone()),
            SchemaChange::AddForeignKey {
                column,
                foreign_key,
                ..
            } => {
                if let Some(c) = shape.columns.iter_mut().find(|c| &c.name == column) {
                    c.references = Some(foreign_key.clone());
                }
            }
            SchemaChange::AddIndex { index, .. } => shape.indexes.push(index.clone()),
            SchemaChange::DropIndex { index, .. } => {
                shape.indexes.retain(|i| i.name != index.name);
            }
            _ => {}
        }
    }
    shape
}

/// The [`EmitError::SqliteRebuildUnsupported`] for a rebuild whose `which`
/// (`"desired"` / `"baseline"`) table shape is absent from the context.
fn missing_rebuild_shape(table: &str, which: &'static str) -> EmitError {
    EmitError::SqliteRebuildUnsupported {
        table: table.to_owned(),
        kind: "table rebuild",
        reason: format!(
            "the {which} table shape is unavailable — call \
             emit_up_sql_with_context / emit_down_sql_with_context with a \
             SchemaContext populated from the desired and baseline tables"
        ),
    }
}

/// Render the recreate block: staging `CREATE`, `INSERT .. SELECT` of the columns
/// common to both shapes (in `new_shape` order), `DROP`, `RENAME`, index recreate,
/// and the integrity check. `leg` only prepends the down-leg irreversibility marker.
fn render_sqlite_rebuild(
    table: &str,
    new_shape: &Table,
    source_shape: &Table,
    leg: RebuildLeg,
) -> String {
    let new_table = format!("{table}__autumn_new");
    let source_cols: BTreeSet<&str> = source_shape
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    let common: Vec<&str> = new_shape
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .filter(|n| source_cols.contains(n))
        .collect();
    let col_list = common.join(", ");

    let mut out = String::new();
    if matches!(leg, RebuildLeg::Down) {
        let _ = writeln!(
            out,
            "-- irreversible: a SQLite table rebuild rolled back to the prior shape may not restore data lost by a narrowing type change or dropped column"
        );
    }
    let _ = writeln!(
        out,
        "-- autumn: SQLite table rebuild for `{table}` — ALTER-family changes require a full table recreate."
    );
    let _ = writeln!(
        out,
        "-- Transaction semantics: this migration must run inside a single transaction (diesel wraps each"
    );
    let _ = writeln!(
        out,
        "-- migration in one). NOTE: `PRAGMA foreign_keys` is a no-op inside a transaction, so the harness"
    );
    let _ = writeln!(
        out,
        "-- must ensure foreign-key enforcement is disabled around the migration."
    );
    let _ = writeln!(
        out,
        "-- autumn-safety: this recreate copies columns and re-creates indexes only. DROP TABLE drops the table's TRIGGERS (not restored here); VIEWS that reference the table are not dropped and may be left dangling or block the rename — re-create triggers and repair dependent views in a manual migration."
    );
    let _ = writeln!(
        out,
        "-- autumn-safety: `PRAGMA foreign_key_check` below only REPORTS violation rows (it does not raise); the recreate copies existing values verbatim and introduces no new orphans, but the migration runner must inspect the pragma's output to treat any pre-existing orphan as an error."
    );
    // Preserve the AUTOINCREMENT high-water mark only when the target shape's single
    // PK is `BigSerial` (an `INTEGER PRIMARY KEY AUTOINCREMENT` column). Such a table
    // has a `sqlite_sequence` row that `DROP TABLE` would discard, resetting the
    // high-water to the max surviving id and letting a later insert reuse an id that
    // was previously issued and deleted. Uuid / composite / non-single PK tables have
    // no `sqlite_sequence` entry, so they emit none of these statements.
    let preserve_seq = matches!(single_pk_column(new_shape), Some((_, IdKind::BigSerial)));

    let _ = writeln!(out, "PRAGMA foreign_keys=OFF;");
    if preserve_seq {
        let _ = writeln!(
            out,
            "-- preserve the AUTOINCREMENT high-water mark so IDs issued-then-deleted are never reused"
        );
        let _ = writeln!(
            out,
            "CREATE TEMP TABLE _autumn_seq_{table} AS SELECT seq FROM sqlite_sequence WHERE name = '{table}';"
        );
    }
    out.push_str(&render_create_table_body(
        &new_table,
        new_shape,
        Backend::Sqlite,
    ));
    let _ = writeln!(out, "INSERT INTO {new_table} ({col_list})");
    let _ = writeln!(out, "    SELECT {col_list} FROM {table};");
    let _ = writeln!(out, "DROP TABLE {table};");
    // Wrap the staging rename in `PRAGMA legacy_alter_table=ON`…`OFF`. Under the modern
    // default (`legacy_alter_table=OFF`), `ALTER TABLE ... RENAME` descends into every
    // dependent VIEW to rewrite references and aborts because the just-dropped table no
    // longer resolves ("error in view v: no such table"). The offline emitter cannot see
    // or recreate views, so it emits this blind, always-present pragma pair: it makes the
    // rename a "dumb" rename that does not validate views, so the migration applies and
    // the view re-validates against the recreated table. Unlike `foreign_keys`, this
    // pragma is honoured inside a transaction (proven by `sqlite_rebuild_survives_dependent_view`).
    let _ = writeln!(out, "PRAGMA legacy_alter_table=ON;");
    let _ = writeln!(out, "ALTER TABLE {new_table} RENAME TO {table};");
    let _ = writeln!(out, "PRAGMA legacy_alter_table=OFF;");
    let mut indexes: Vec<&Index> = new_shape.indexes.iter().collect();
    indexes.sort_by(|a, b| a.name.cmp(&b.name));
    for index in indexes {
        let _ = writeln!(out, "{}", index_sql(table, index));
    }
    if preserve_seq {
        let _ = writeln!(
            out,
            "UPDATE sqlite_sequence SET seq = (SELECT seq FROM _autumn_seq_{table})\n    WHERE name = '{table}' AND (SELECT seq FROM _autumn_seq_{table}) IS NOT NULL;"
        );
        let _ = writeln!(
            out,
            "INSERT INTO sqlite_sequence (name, seq)\n    SELECT '{table}', (SELECT seq FROM _autumn_seq_{table})\n    WHERE (SELECT seq FROM _autumn_seq_{table}) IS NOT NULL\n      AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = '{table}');"
        );
        let _ = writeln!(out, "DROP TABLE _autumn_seq_{table};");
    }
    let _ = writeln!(out, "PRAGMA foreign_key_check;");
    let _ = writeln!(out, "PRAGMA foreign_keys=ON;");
    out
}

/// Join statement groups with a blank line and a trailing newline (empty for an
/// empty plan).
fn join_groups(groups: &[String]) -> String {
    if groups.is_empty() {
        return String::new();
    }
    let mut out = groups.join("\n\n");
    out.push('\n');
    out
}

/// Order the changes into the canonical up-buckets (a valid dependency order),
/// stable within a bucket (`diff_schema` already emits a deterministic order).
///
/// Two orderings need context the plain per-change bucket cannot provide, so they
/// are handled here rather than in [`up_bucket`]:
///
/// * **New-table FK dependencies** — `CREATE TABLE`s are **topologically** sorted
///   so a table referenced by an inline `REFERENCES` is created before the table
///   that references it (a cycle is [`EmitError::CyclicTableDependencies`]).
/// * **Replaced indexes** — a same-named index that is both dropped and re-added
///   (a shape change) must `DROP INDEX` **before** the `CREATE INDEX`, else the
///   create collides with the still-existing old index; such a "replacement drop"
///   is ordered just before the `AddIndex` bucket instead of in the general
///   `DropIndex` bucket.
///
/// # Errors
///
/// Returns [`EmitError::CyclicTableDependencies`] when the new tables reference
/// each other in a cycle that inline foreign keys cannot express.
fn up_ordered(changes: &[SchemaChange]) -> Result<Vec<&SchemaChange>, EmitError> {
    let replaced = replaced_index_names(changes);

    // `CreateTable`s are the first bucket; order them topologically so referenced
    // tables precede their referencers.
    let mut creates: Vec<&SchemaChange> = changes
        .iter()
        .filter(|c| matches!(c, SchemaChange::CreateTable(_)))
        .collect();
    topo_sort_creates(&mut creates)?;

    // `DropTable`s are the last bucket; order them in REVERSE topological order so
    // a table that references another (via its baseline inline FK) is dropped
    // before the table it references. `emit_down_sql` reverses the whole plan, so
    // this reverse-topo up-order becomes a forward-topo (referenced-first) recreate
    // on the down leg.
    let mut drops: Vec<&SchemaChange> = changes
        .iter()
        .filter(|c| matches!(c, SchemaChange::DropTable(_)))
        .collect();
    topo_sort_drops(&mut drops)?;

    // Everything else keeps its bucket order; the replacement-drop key threads
    // through so a replaced index drops before its re-add.
    let mut rest: Vec<&SchemaChange> = changes
        .iter()
        .filter(|c| !matches!(c, SchemaChange::CreateTable(_) | SchemaChange::DropTable(_)))
        .collect();
    rest.sort_by_key(|c| up_sort_key(c, &replaced));

    let mut ordered = creates;
    ordered.extend(rest);
    ordered.extend(drops);
    Ok(ordered)
}

/// The set of index names that appear in **both** an `AddIndex` and a `DropIndex`
/// in the same plan — i.e. a same-named index whose shape changed
/// ([`diff_indexes`] emits it as a drop + a re-add). Such a drop must precede its
/// re-add in `up.sql`.
fn replaced_index_names(changes: &[SchemaChange]) -> BTreeSet<String> {
    let added: BTreeSet<&str> = changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::AddIndex { index, .. } => Some(index.name.as_str()),
            _ => None,
        })
        .collect();
    changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::DropIndex { index, .. } if added.contains(index.name.as_str()) => {
                Some(index.name.clone())
            }
            _ => None,
        })
        .collect()
}

/// The up-order sort key for a non-`CreateTable` change. A two-level key so a
/// **replacement** `DropIndex` (its name is also re-added) sorts *just before* the
/// `AddIndex` bucket while every other change keeps its plain bucket.
fn up_sort_key(change: &SchemaChange, replaced: &BTreeSet<String>) -> (u8, u8) {
    match change {
        // Replacement drop: same bucket as AddIndex, but ordered before it.
        SchemaChange::DropIndex { index, .. } if replaced.contains(&index.name) => (3, 0),
        SchemaChange::AddIndex { .. } => (3, 1),
        other => (up_bucket(other), 1),
    }
}

/// Topologically order `creates` (all `CreateTable` changes) so a table referenced
/// by another's inline `REFERENCES` is created first. Only intra-batch
/// dependencies matter — a reference to a pre-existing baseline table imposes no
/// ordering. Deterministic (Kahn's algorithm, always taking the
/// lexicographically-smallest ready table).
///
/// # Errors
///
/// Returns [`EmitError::CyclicTableDependencies`] if the new tables form a
/// reference cycle (inline foreign keys cannot express one).
fn topo_sort_creates(creates: &mut [&SchemaChange]) -> Result<(), EmitError> {
    if creates.len() < 2 {
        return Ok(());
    }
    let tables: BTreeMap<&str, &Table> = creates
        .iter()
        .filter_map(|c| match c {
            SchemaChange::CreateTable(t) => Some((t.name.as_str(), t)),
            _ => None,
        })
        .collect();
    let names: BTreeSet<&str> = tables.keys().copied().collect();

    // deps[name] = the in-batch tables `name` references (must be created first).
    let mut deps: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (&name, table) in &tables {
        let mut d = BTreeSet::new();
        for col in &table.columns {
            if let Some(fk) = &col.references {
                let target = fk.table.as_str();
                if target != name && names.contains(target) {
                    d.insert(target);
                }
            }
        }
        deps.insert(name, d);
    }

    // Kahn: repeatedly place the smallest name whose deps are all already placed.
    let mut remaining: BTreeSet<&str> = names;
    let mut order: BTreeMap<&str, usize> = BTreeMap::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .copied()
            .find(|n| deps[n].iter().all(|d| !remaining.contains(d)));
        if let Some(n) = ready {
            order.insert(n, order.len());
            remaining.remove(n);
        } else {
            let mut cyc: Vec<String> = remaining.iter().map(|s| (*s).to_owned()).collect();
            cyc.sort();
            return Err(EmitError::CyclicTableDependencies { tables: cyc });
        }
    }

    creates.sort_by_key(|c| match c {
        SchemaChange::CreateTable(t) => order.get(t.name.as_str()).copied().unwrap_or(usize::MAX),
        _ => usize::MAX,
    });
    Ok(())
}

/// Topologically order `drops` (all `DropTable` changes) in **reverse** dependency
/// order so a table that references another (via its own baseline inline FK) is
/// dropped **before** the table it references — PG rejects dropping a referenced
/// table while a referencing one still exists. The dependency graph is built from
/// the dropped tables' baseline FKs (carried on the [`SchemaChange::DropTable`]
/// payload). Only intra-batch dependencies matter: a retained referencer is a
/// separate concern handled by [`guard_plan`]'s inbound-FK refusal. Deterministic
/// (Kahn's algorithm, always taking the lexicographically-smallest ready table).
///
/// # Errors
///
/// Returns [`EmitError::CyclicTableDependencies`] if the dropped tables form a
/// reference cycle (the same refusal the [`SchemaChange::CreateTable`] path uses).
fn topo_sort_drops(drops: &mut [&SchemaChange]) -> Result<(), EmitError> {
    if drops.len() < 2 {
        return Ok(());
    }
    let tables: BTreeMap<&str, &Table> = drops
        .iter()
        .filter_map(|c| match c {
            SchemaChange::DropTable(t) => Some((t.name.as_str(), t)),
            _ => None,
        })
        .collect();
    let names: BTreeSet<&str> = tables.keys().copied().collect();

    // deps[name] = the in-batch tables that must be dropped BEFORE `name`, i.e.
    // the tables that reference `name` (referencers drop first). This is the edge
    // set of `topo_sort_creates` inverted.
    let mut deps: BTreeMap<&str, BTreeSet<&str>> =
        names.iter().map(|&n| (n, BTreeSet::new())).collect();
    for (&name, table) in &tables {
        for col in &table.columns {
            if let Some(fk) = &col.references {
                let target = fk.table.as_str();
                if target != name && names.contains(target) {
                    // `name` references `target` ⇒ `name` must drop before `target`.
                    deps.get_mut(target)
                        .expect("target is an in-batch name")
                        .insert(name);
                }
            }
        }
    }

    // Kahn: repeatedly place the smallest name whose deps are all already placed.
    let mut remaining: BTreeSet<&str> = names;
    let mut order: BTreeMap<&str, usize> = BTreeMap::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .copied()
            .find(|n| deps[n].iter().all(|d| !remaining.contains(d)));
        if let Some(n) = ready {
            order.insert(n, order.len());
            remaining.remove(n);
        } else {
            let mut cyc: Vec<String> = remaining.iter().map(|s| (*s).to_owned()).collect();
            cyc.sort();
            return Err(EmitError::CyclicTableDependencies { tables: cyc });
        }
    }

    drops.sort_by_key(|c| match c {
        SchemaChange::DropTable(t) => order.get(t.name.as_str()).copied().unwrap_or(usize::MAX),
        _ => usize::MAX,
    });
    Ok(())
}

/// The canonical up-order bucket for a change (lower = earlier).
const fn up_bucket(change: &SchemaChange) -> u8 {
    match change {
        SchemaChange::CreateTable(_) => 0,
        SchemaChange::AddColumn { .. } => 1,
        SchemaChange::AlterColumnType { .. }
        | SchemaChange::SetNotNull { .. }
        | SchemaChange::DropNotNull { .. }
        | SchemaChange::SetDefault { .. } => 2,
        SchemaChange::AddIndex { .. } => 3,
        SchemaChange::AddCheck { .. } | SchemaChange::AddForeignKey { .. } => 4,
        SchemaChange::DropIndex { .. } => 5,
        SchemaChange::DropColumn { .. } => 6,
        SchemaChange::DropTable(_) => 7,
        // Non-emittable markers; the guard refuses them before emission is reached.
        SchemaChange::PrimaryKeyChange { .. }
        | SchemaChange::ForeignKeyChange { .. }
        | SchemaChange::DropTableBlockedByInboundFk { .. }
        | SchemaChange::AlterColumnTypeBlockedByFk { .. }
        | SchemaChange::AddForeignKeyToExistingColumn { .. }
        | SchemaChange::CreateTableBlockedBySkippedField { .. } => 9,
    }
}

/// Render a single change's forward SQL.
fn emit_change_up(change: &SchemaChange, backend: Backend) -> Result<String, EmitError> {
    match change {
        SchemaChange::CreateTable(table) => Ok(emit_create_table(table, backend)),
        SchemaChange::DropTable(table) => Ok(format!(
            "-- autumn-safety: this DROP TABLE is not checked against generated \
             #[belongs_to(...)] association foreign keys, which the offline snapshot cannot see; \
             if a retained table has an invisible association FK to `{0}`, this will fail to apply \
             -- verify with schema introspection or drop the FK manually first\n\
             DROP TABLE {0};\n",
            table.name
        )),
        SchemaChange::AddColumn { table, column } => emit_add_column(table, column, backend),
        SchemaChange::DropColumn { table, column } => Ok(format!(
            "ALTER TABLE {table} DROP COLUMN {};\n",
            column.name
        )),
        SchemaChange::AlterColumnType {
            table, column, to, ..
        } => {
            require_pg(backend, "AlterColumnType")?;
            Ok(format!(
                "ALTER TABLE {table} ALTER COLUMN {column} TYPE {};\n",
                to.sql_type(backend)
            ))
        }
        SchemaChange::SetNotNull { table, column } => {
            require_pg(backend, "SetNotNull")?;
            Ok(format!(
                "-- autumn-safety: potentially-blocking -- existing NULLs must be backfilled first\n\
                 ALTER TABLE {table} ALTER COLUMN {column} SET NOT NULL;\n"
            ))
        }
        SchemaChange::DropNotNull { table, column } => {
            require_pg(backend, "DropNotNull")?;
            Ok(format!(
                "ALTER TABLE {table} ALTER COLUMN {column} DROP NOT NULL;\n"
            ))
        }
        SchemaChange::SetDefault {
            table, column, to, ..
        } => {
            require_pg(backend, "SetDefault")?;
            Ok(format!(
                "ALTER TABLE {table} ALTER COLUMN {column} SET DEFAULT {};\n",
                default_sql(to, backend)
            ))
        }
        SchemaChange::AddIndex { table, index } => Ok(format!("{}\n", index_sql(table, index))),
        SchemaChange::DropIndex { index, .. } => Ok(format!("DROP INDEX {};\n", index.name)),
        SchemaChange::AddCheck { table, check } => {
            require_pg(backend, "AddCheck")?;
            Ok(check.name.as_ref().map_or_else(
                || format!("ALTER TABLE {table} ADD CHECK ({});\n", check.expression),
                |name| {
                    format!(
                        "ALTER TABLE {table} ADD CONSTRAINT {name} CHECK ({});\n",
                        check.expression
                    )
                },
            ))
        }
        SchemaChange::AddForeignKey {
            table,
            column,
            foreign_key,
        } => {
            require_pg(backend, "AddForeignKey")?;
            let constraint = bounded_pg_identifier(&format!("{table}_{column}_fkey"));
            Ok(format!(
                "ALTER TABLE {table} ADD CONSTRAINT {constraint} \
                 FOREIGN KEY ({column}) REFERENCES {}({});\n",
                foreign_key.table, foreign_key.column
            ))
        }
        // Non-emittable markers: the guard refuses them, so they never reach here
        // in the command flow. Render nothing defensively rather than panicking.
        SchemaChange::PrimaryKeyChange { .. }
        | SchemaChange::ForeignKeyChange { .. }
        | SchemaChange::DropTableBlockedByInboundFk { .. }
        | SchemaChange::AlterColumnTypeBlockedByFk { .. }
        | SchemaChange::AddForeignKeyToExistingColumn { .. }
        | SchemaChange::CreateTableBlockedBySkippedField { .. } => Ok(String::new()),
    }
}

/// Render a single change's reverse SQL, with an `-- irreversible:` / `-- manual:`
/// marker where the round-trip is not clean.
fn emit_change_down(change: &SchemaChange, backend: Backend) -> Result<String, EmitError> {
    match change {
        SchemaChange::CreateTable(table) => Ok(format!("DROP TABLE {};\n", table.name)),
        SchemaChange::DropTable(table) => {
            let recreate = emit_create_table(table, backend);
            Ok(format!(
                "-- irreversible: table data dropped by this migration cannot be restored\n\
                 {recreate}"
            ))
        }
        SchemaChange::AddColumn { table, column } => Ok(format!(
            "ALTER TABLE {table} DROP COLUMN {};\n",
            column.name
        )),
        SchemaChange::DropColumn { table, column } => {
            let readd = emit_add_column(table, column, backend)?;
            Ok(format!(
                "-- irreversible: column data dropped by this migration cannot be restored\n\
                 {readd}"
            ))
        }
        SchemaChange::AlterColumnType {
            table,
            column,
            from,
            ..
        } => {
            require_pg(backend, "AlterColumnType")?;
            Ok(format!(
                "-- irreversible: a narrowing type change may have lost data; review before rolling back\n\
                 ALTER TABLE {table} ALTER COLUMN {column} TYPE {};\n",
                from.sql_type(backend)
            ))
        }
        SchemaChange::SetNotNull { table, column } => {
            require_pg(backend, "SetNotNull")?;
            Ok(format!(
                "ALTER TABLE {table} ALTER COLUMN {column} DROP NOT NULL;\n"
            ))
        }
        SchemaChange::DropNotNull { table, column } => {
            require_pg(backend, "DropNotNull")?;
            Ok(format!(
                "-- potentially-blocking: rolling back re-adds NOT NULL; existing NULL rows will block it\n\
                 ALTER TABLE {table} ALTER COLUMN {column} SET NOT NULL;\n"
            ))
        }
        SchemaChange::SetDefault {
            table,
            column,
            from,
            ..
        } => {
            require_pg(backend, "SetDefault")?;
            Ok(from.as_ref().map_or_else(
                || format!("ALTER TABLE {table} ALTER COLUMN {column} DROP DEFAULT;\n"),
                |d| {
                    format!(
                        "ALTER TABLE {table} ALTER COLUMN {column} SET DEFAULT {};\n",
                        default_sql(d, backend)
                    )
                },
            ))
        }
        SchemaChange::AddIndex { index, .. } => Ok(format!("DROP INDEX {};\n", index.name)),
        SchemaChange::DropIndex { table, index } => Ok(format!("{}\n", index_sql(table, index))),
        SchemaChange::AddCheck { table, check } => Ok(check.name.as_ref().map_or_else(
            || "-- manual: unnamed CHECK cannot be auto-dropped\n".to_owned(),
            |name| format!("ALTER TABLE {table} DROP CONSTRAINT {name};\n"),
        )),
        SchemaChange::AddForeignKey { table, column, .. } => {
            let constraint = bounded_pg_identifier(&format!("{table}_{column}_fkey"));
            Ok(format!(
                "ALTER TABLE {table} DROP CONSTRAINT {constraint};\n"
            ))
        }
        SchemaChange::PrimaryKeyChange { .. }
        | SchemaChange::ForeignKeyChange { .. }
        | SchemaChange::DropTableBlockedByInboundFk { .. }
        | SchemaChange::AlterColumnTypeBlockedByFk { .. }
        | SchemaChange::AddForeignKeyToExistingColumn { .. }
        | SchemaChange::CreateTableBlockedBySkippedField { .. } => Ok(String::new()),
    }
}

/// Guard the pg-only `ALTER`-family renderers: `SQLite` needs slice 5's
/// table-rebuild, so it is an explicit unsupported error here.
const fn require_pg(backend: Backend, kind: &'static str) -> Result<(), EmitError> {
    match backend {
        Backend::Postgres => Ok(()),
        Backend::Sqlite => Err(EmitError::UnsupportedOnBackend { kind, backend }),
    }
}

/// Render a `CREATE TABLE` (reconstructing a single int/uuid PK via
/// [`IdKind::pk_sql`] so `BIGSERIAL` / `gen_random_uuid()` are not lost), followed
/// by one `CREATE [UNIQUE] INDEX` per index (name-sorted).
///
/// A `CREATE TABLE` is always renderable (the portable `SQLite` subset covers it),
/// so this is infallible.
fn emit_create_table(table: &Table, backend: Backend) -> String {
    let mut out = render_create_table_body(&table.name, table, backend);

    // Indexes, name-sorted for determinism.
    let mut indexes: Vec<&Index> = table.indexes.iter().collect();
    indexes.sort_by(|a, b| a.name.cmp(&b.name));
    for index in indexes {
        let _ = writeln!(out, "{}", index_sql(&table.name, index));
    }
    out
}

/// Render just the `CREATE TABLE {name} (…columns/PK/checks…);` statement (no index
/// `CREATE`s) for `table`'s shape, under an arbitrary `name`.
///
/// Split out of [`emit_create_table`] so the `SQLite` table-rebuild path can emit
/// the `{table}__autumn_new` staging table (whose indexes are recreated separately,
/// after the rename). [`emit_create_table`] recomposes this body plus its indexes,
/// so its output is unchanged.
fn render_create_table_body(name: &str, table: &Table, backend: Backend) -> String {
    let single_pk = single_pk_column(table);
    let mut lines: Vec<String> = Vec::with_capacity(table.columns.len() + 2);

    for col in &table.columns {
        if let Some((pk_col, kind)) = &single_pk
            && pk_col.name == col.name
        {
            lines.push(format!("    {} {}", col.name, kind.pk_sql(backend)));
        } else {
            lines.push(format!("    {}", render_column_def(col, backend)));
        }
    }

    // Composite / exotic PK: a trailing table-level clause, columns rendered
    // normally above.
    if single_pk.is_none() && !table.primary_key.is_empty() {
        lines.push(format!(
            "    PRIMARY KEY ({})",
            table.primary_key.join(", ")
        ));
    }

    // Table-level checks (rare — the parser emits none today).
    for check in &table.checks {
        match &check.name {
            Some(cname) => lines.push(format!(
                "    CONSTRAINT {cname} CHECK ({})",
                check.expression
            )),
            None => lines.push(format!("    CHECK ({})", check.expression)),
        }
    }

    format!("CREATE TABLE {name} (\n{}\n);\n", lines.join(",\n"))
}

/// Render an `ALTER TABLE … ADD COLUMN`, mirroring the generator's house
/// convention for the `-- autumn-safety` comment on a `NOT NULL`-without-default
/// column.
///
/// **Index ownership:** this renderer emits **only** the column, never a
/// `CREATE INDEX`. A reference column's auto-index (`idx_<table>_<column>`) is
/// folded into the parser's table index set, so it arrives as its own
/// [`SchemaChange::AddIndex`] and is rendered by [`index_sql`] exactly once. If
/// `ADD COLUMN` also rendered the index inline it would be created twice and the
/// migration would fail. `diff_indexes`/`AddIndex` (and, for a brand-new table,
/// [`emit_create_table`]) is the single owner of index creation.
fn emit_add_column(table: &str, column: &Column, backend: Backend) -> Result<String, EmitError> {
    // `SQLite` rejects `ADD COLUMN … NOT NULL` without a DEFAULT — a slice-5 concern.
    if backend == Backend::Sqlite && !column.nullable && column.default.is_none() {
        return Err(EmitError::UnsupportedOnBackend {
            kind: "AddColumn (NOT NULL without a default on SQLite)",
            backend,
        });
    }

    let mut out = String::new();
    if !column.nullable && column.default.is_none() {
        let _ = writeln!(
            out,
            "-- autumn-safety: potentially-blocking -- add a DEFAULT or backfill existing rows before enforcing NOT NULL"
        );
    }
    let _ = writeln!(
        out,
        "ALTER TABLE {table} ADD COLUMN {};",
        render_column_def(column, backend)
    );
    Ok(out)
}

/// Render a column definition body: `{name} {type} {NOT NULL|NULL} [REFERENCES
/// t(c)] [DEFAULT d]`. Shared by `CREATE TABLE` (non-PK columns) and `ADD
/// COLUMN`.
fn render_column_def(column: &Column, backend: Backend) -> String {
    let mut def = format!(
        "{} {} {}",
        column.name,
        column.ty.sql_type(backend),
        nullability(column.nullable)
    );
    if let Some(fk) = &column.references {
        let _ = write!(def, " REFERENCES {}({})", fk.table, fk.column);
    }
    if let Some(default) = &column.default {
        let _ = write!(def, " DEFAULT {}", default_sql(default, backend));
    }
    def
}

/// `CREATE [UNIQUE] INDEX {name} ON {table} ({cols});`.
fn index_sql(table: &str, index: &Index) -> String {
    let unique = if index.unique { "UNIQUE " } else { "" };
    format!(
        "CREATE {unique}INDEX {} ON {table} ({});",
        index.name,
        index.columns.join(", ")
    )
}

/// The nullability clause for a column.
const fn nullability(nullable: bool) -> &'static str {
    if nullable { "NULL" } else { "NOT NULL" }
}

/// Render a column default to SQL for `backend` (`Now` → `NOW()` on Postgres,
/// `CURRENT_TIMESTAMP` on `SQLite`; `Sql(s)` verbatim).
fn default_sql(default: &ColumnDefault, backend: Backend) -> String {
    match default {
        ColumnDefault::Now => match backend {
            Backend::Postgres => "NOW()".to_owned(),
            Backend::Sqlite => "CURRENT_TIMESTAMP".to_owned(),
        },
        ColumnDefault::Sql(sql) => sql.clone(),
    }
}

/// The single-column primary key column and its reconstructed [`IdKind`], if the
/// table has exactly one PK column that is an int/uuid id.
///
/// The IR does not store `IdKind` on `Table` (a `BigSerial` PK is a
/// `Column { ty: Int64, primary_key: true, default: None }`), so naively emitting
/// it as `BIGINT` would silently drop auto-increment. This mirrors the
/// generator + parser conventions to recover it. Returns `None` for a composite
/// PK or a non-int/uuid PK → the caller falls back to a table-level `PRIMARY KEY
/// (…)` clause.
fn single_pk_column(table: &Table) -> Option<(&Column, IdKind)> {
    if table.primary_key.len() != 1 {
        return None;
    }
    let name = &table.primary_key[0];
    let column = table.columns.iter().find(|c| &c.name == name)?;
    pk_kind_for(column).map(|kind| (column, kind))
}

/// Derive the id-generation strategy for a single-column PK:
///   `Int64` PK, no default            → `BigSerial`
///   `Uuid`  PK                        → `Uuid`
/// Any other PK column → `None` (rendered normally with a table-level clause).
const fn pk_kind_for(column: &Column) -> Option<IdKind> {
    match &column.ty {
        ColumnType::Int64 if column.default.is_none() => Some(IdKind::BigSerial),
        ColumnType::Uuid => Some(IdKind::Uuid),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Human-readable summary
// ---------------------------------------------------------------------------

/// A human-readable plan summary for the default (no `--write-migration`) output.
#[must_use]
pub fn describe_plan(plan: &MigrationPlan) -> String {
    let backend = match plan.backend {
        Backend::Postgres => "postgres",
        Backend::Sqlite => "sqlite",
    };
    let mut out = format!(
        "Migration plan ({backend}): {} change(s)\n",
        plan.changes.len()
    );
    for change in &plan.changes {
        let _ = writeln!(out, "  {}", describe_change(change));
    }
    out
}

/// A one-line description of a single change.
fn describe_change(change: &SchemaChange) -> String {
    match change {
        SchemaChange::CreateTable(t) => format!("+ CREATE TABLE {}", t.name),
        SchemaChange::DropTable(t) => format!("- DROP TABLE {}", t.name),
        SchemaChange::AddColumn { table, column } => {
            format!("+ ADD COLUMN {table}.{}", column.name)
        }
        SchemaChange::DropColumn { table, column } => {
            format!("- DROP COLUMN {table}.{}", column.name)
        }
        SchemaChange::AlterColumnType {
            table,
            column,
            from,
            to,
        } => format!(
            "~ ALTER COLUMN {table}.{column} TYPE {} (was {})",
            to.sql_type(Backend::Postgres),
            from.sql_type(Backend::Postgres)
        ),
        SchemaChange::SetNotNull { table, column } => {
            format!("~ SET NOT NULL {table}.{column}")
        }
        SchemaChange::DropNotNull { table, column } => {
            format!("~ DROP NOT NULL {table}.{column}")
        }
        SchemaChange::SetDefault { table, column, .. } => {
            format!("~ SET DEFAULT {table}.{column}")
        }
        SchemaChange::AddForeignKey { table, column, .. } => {
            format!("+ ADD FOREIGN KEY {table}.{column}")
        }
        SchemaChange::AddIndex { table, index } => {
            format!("+ ADD INDEX {} ON {table}", index.name)
        }
        SchemaChange::DropIndex { table, index } => {
            format!("- DROP INDEX {} ON {table}", index.name)
        }
        SchemaChange::AddCheck { table, check } => format!(
            "+ ADD CHECK {} ON {table}",
            check.name.as_deref().unwrap_or("(unnamed)")
        ),
        SchemaChange::PrimaryKeyChange { table } => {
            format!("! PRIMARY KEY CHANGE on {table} (refused)")
        }
        SchemaChange::ForeignKeyChange { table, column } => {
            format!("! FOREIGN KEY RETARGET on {table}.{column} (refused)")
        }
        SchemaChange::DropTableBlockedByInboundFk {
            table,
            referencing_table,
            referencing_column,
        } => format!(
            "! DROP TABLE {table} blocked by retained FK {referencing_table}.{referencing_column} (refused)"
        ),
        SchemaChange::AlterColumnTypeBlockedByFk { table, column } => {
            format!(
                "! ALTER COLUMN {table}.{column} TYPE blocked by foreign-key constraint (refused)"
            )
        }
        SchemaChange::AddForeignKeyToExistingColumn { table, column } => {
            format!("! ADD FOREIGN KEY on pre-existing column {table}.{column} (refused)")
        }
        SchemaChange::CreateTableBlockedBySkippedField { table, fields } => {
            format!(
                "! CREATE TABLE {table} blocked by parser-skipped field(s) [{}] (refused)",
                fields.join(", ")
            )
        }
    }
}

#[cfg(test)]
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;
    use autumn_schema_core::{ForeignKey, Table};

    use crate::schema::parse::SchemaDiagnostic;

    // -- fixture helpers -----------------------------------------------------

    fn col(name: &str, ty: ColumnType) -> Column {
        Column::new(name, ty)
    }

    /// A `posts` table with an `id` `BigSerial` PK plus the given extra columns.
    fn posts_with(columns: Vec<Column>) -> Table {
        let mut t = Table::new("posts", Backend::Postgres);
        let mut id = col("id", ColumnType::Int64);
        id.primary_key = true;
        t.primary_key.push("id".to_owned());
        t.columns.push(id);
        t.columns.extend(columns);
        t
    }

    /// A managed table `name` with a `BigSerial` `id` PK plus one extra column
    /// (used to build cross-referencing new tables in the topo-order tests).
    fn posts_ref_table(name: &str, extra: Column) -> Table {
        let mut t = Table::new(name, Backend::Postgres);
        let mut id = col("id", ColumnType::Int64);
        id.primary_key = true;
        t.primary_key.push("id".to_owned());
        t.columns.push(id);
        t.columns.push(extra);
        t
    }

    fn parsed(tables: Vec<Table>, diagnostics: Vec<SchemaDiagnostic>) -> ParsedSchema {
        ParsedSchema {
            tables,
            diagnostics,
        }
    }

    fn diag(model: &str, table: &str, field: &str) -> SchemaDiagnostic {
        SchemaDiagnostic {
            model: model.to_owned(),
            table: table.to_owned(),
            field: field.to_owned(),
            rust_type: "Unknown".to_owned(),
            message: format!("skipped {model}.{field}"),
        }
    }

    const DEFAULT_OPTS: DiffOptions = DiffOptions {
        allow_destructive: false,
    };
    const ALLOW: DiffOptions = DiffOptions {
        allow_destructive: true,
    };

    // -- 13.1 structural diff ------------------------------------------------

    #[test]
    fn no_op_identical_returns_empty_plan() {
        let base = vec![posts_with(vec![col("body", ColumnType::Text)])];
        let want = parsed(
            vec![posts_with(vec![col("body", ColumnType::Text)])],
            vec![],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert!(plan.is_empty(), "identical schemas → empty plan: {plan:?}");
    }

    #[test]
    fn add_column_emits_add_column() {
        let base = vec![posts_with(vec![])];
        let mut bio = col("bio", ColumnType::Text);
        bio.nullable = true;
        let want = parsed(vec![posts_with(vec![bio.clone()])], vec![]);
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(plan.changes.len(), 1);
        match &plan.changes[0] {
            SchemaChange::AddColumn { table, column } => {
                assert_eq!(table, "posts");
                assert_eq!(column, &bio);
            }
            other => panic!("expected AddColumn, got {other:?}"),
        }
    }

    #[test]
    fn drop_column_present_in_plan_carries_baseline_column() {
        let nickname = col("nickname", ColumnType::Text);
        let base = vec![posts_with(vec![nickname.clone()])];
        let want = parsed(vec![posts_with(vec![])], vec![]);
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(plan.changes.len(), 1);
        match &plan.changes[0] {
            SchemaChange::DropColumn { table, column } => {
                assert_eq!(table, "posts");
                assert_eq!(column, &nickname, "carries the baseline column for down");
            }
            other => panic!("expected DropColumn, got {other:?}"),
        }
    }

    #[test]
    fn alter_column_type_int32_to_int64() {
        let base = vec![posts_with(vec![col("views", ColumnType::Int32)])];
        let want = parsed(
            vec![posts_with(vec![col("views", ColumnType::Int64)])],
            vec![],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::AlterColumnType {
                table: "posts".to_owned(),
                column: "views".to_owned(),
                from: ColumnType::Int32,
                to: ColumnType::Int64,
            }]
        );
    }

    #[test]
    fn nullable_to_not_null_emits_set_not_null() {
        let mut nullable_bio = col("bio", ColumnType::Text);
        nullable_bio.nullable = true;
        let base = vec![posts_with(vec![nullable_bio])];
        let want = parsed(vec![posts_with(vec![col("bio", ColumnType::Text)])], vec![]);
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::SetNotNull {
                table: "posts".to_owned(),
                column: "bio".to_owned(),
            }]
        );
    }

    #[test]
    fn not_null_to_nullable_emits_drop_not_null() {
        let base = vec![posts_with(vec![col("bio", ColumnType::Text)])];
        let mut nullable_bio = col("bio", ColumnType::Text);
        nullable_bio.nullable = true;
        let want = parsed(vec![posts_with(vec![nullable_bio])], vec![]);
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::DropNotNull {
                table: "posts".to_owned(),
                column: "bio".to_owned(),
            }]
        );
    }

    #[test]
    fn set_default_added() {
        let base = vec![posts_with(vec![col("created_at", ColumnType::Timestamp)])];
        let mut created = col("created_at", ColumnType::Timestamp);
        created.default = Some(ColumnDefault::Now);
        let want = parsed(vec![posts_with(vec![created])], vec![]);
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::SetDefault {
                table: "posts".to_owned(),
                column: "created_at".to_owned(),
                to: ColumnDefault::Now,
                from: None,
            }]
        );
    }

    #[test]
    fn add_index_and_drop_index_by_name() {
        let idx = Index {
            name: "idx_posts_body".to_owned(),
            columns: vec!["body".to_owned()],
            unique: false,
        };
        // desired gains the index.
        let base = vec![posts_with(vec![col("body", ColumnType::Text)])];
        let mut want_table = posts_with(vec![col("body", ColumnType::Text)]);
        want_table.indexes.push(idx.clone());
        let plan = diff_schema(&base, &parsed(vec![want_table], vec![]), DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::AddIndex {
                table: "posts".to_owned(),
                index: idx.clone(),
            }]
        );

        // baseline has it, desired dropped it — DropIndex carries the baseline.
        let mut base_table = posts_with(vec![col("body", ColumnType::Text)]);
        base_table.indexes.push(idx.clone());
        let want = parsed(
            vec![posts_with(vec![col("body", ColumnType::Text)])],
            vec![],
        );
        let plan = diff_schema(&[base_table], &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::DropIndex {
                table: "posts".to_owned(),
                index: idx,
            }]
        );
    }

    #[test]
    fn add_check_emitted_but_no_drop_check() {
        let check = CheckConstraint {
            name: Some("posts_body_len".to_owned()),
            expression: "length(body) > 0".to_owned(),
        };
        // desired gains a check → AddCheck.
        let base = vec![posts_with(vec![col("body", ColumnType::Text)])];
        let mut want_table = posts_with(vec![col("body", ColumnType::Text)]);
        want_table.checks.push(check.clone());
        let plan = diff_schema(&base, &parsed(vec![want_table], vec![]), DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::AddCheck {
                table: "posts".to_owned(),
                check,
            }]
        );

        // baseline-only check → NO change (rule A: never drop a check).
        let mut base_table = posts_with(vec![col("body", ColumnType::Text)]);
        base_table.checks.push(CheckConstraint {
            name: Some("posts_body_len".to_owned()),
            expression: "length(body) > 0".to_owned(),
        });
        let want = parsed(
            vec![posts_with(vec![col("body", ColumnType::Text)])],
            vec![],
        );
        let plan = diff_schema(&[base_table], &want, DEFAULT_OPTS);
        assert!(
            plan.is_empty(),
            "baseline-only check must not be dropped: {plan:?}"
        );
    }

    #[test]
    fn new_table_creates() {
        let base: Vec<Table> = vec![];
        let want_table = posts_with(vec![col("body", ColumnType::Text)]);
        let plan = diff_schema(
            &base,
            &parsed(vec![want_table.clone()], vec![]),
            DEFAULT_OPTS,
        );
        assert_eq!(plan.changes, vec![SchemaChange::CreateTable(want_table)]);
    }

    #[test]
    fn missing_table_drops_carrying_baseline() {
        let base_table = posts_with(vec![col("body", ColumnType::Text)]);
        let want = parsed(vec![], vec![]);
        let plan = diff_schema(std::slice::from_ref(&base_table), &want, DEFAULT_OPTS);
        assert_eq!(plan.changes, vec![SchemaChange::DropTable(base_table)]);
    }

    #[test]
    fn key_columns_diff_by_name_not_position() {
        // Same columns, reordered → empty plan (name-keyed, not positional).
        let base = vec![posts_with(vec![
            col("a", ColumnType::Text),
            col("b", ColumnType::Int32),
        ])];
        let want = parsed(
            vec![posts_with(vec![
                col("b", ColumnType::Int32),
                col("a", ColumnType::Text),
            ])],
            vec![],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert!(plan.is_empty(), "reordered columns → no change: {plan:?}");
    }

    // -- 13.2 parser-gap conservatism (rules A–E) ----------------------------

    #[test]
    fn baseline_enum_check_not_dropped() {
        let mut base_table = posts_with(vec![col("status", ColumnType::Text)]);
        base_table.checks.push(CheckConstraint {
            name: Some("posts_status_check".to_owned()),
            expression: "status IN ('draft','live')".to_owned(),
        });
        // desired (parser output) has no checks.
        let want = parsed(
            vec![posts_with(vec![col("status", ColumnType::Text)])],
            vec![],
        );
        let plan = diff_schema(&[base_table], &want, DEFAULT_OPTS);
        assert!(plan.is_empty(), "rule A: enum CHECK not dropped: {plan:?}");
    }

    #[test]
    fn baseline_association_fk_not_dropped() {
        let mut author = col("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "id"));
        let base = vec![posts_with(vec![author])];
        // desired same column, references None (parser can't resolve association FK).
        let want = parsed(
            vec![posts_with(vec![col("author_id", ColumnType::Int64)])],
            vec![],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert!(
            plan.is_empty(),
            "rule B: association FK not dropped: {plan:?}"
        );
    }

    #[test]
    fn baseline_non_convention_default_not_dropped() {
        let mut status = col("status", ColumnType::Text);
        status.default = Some(ColumnDefault::Sql("'draft'".to_owned()));
        let base = vec![posts_with(vec![status])];
        let want = parsed(
            vec![posts_with(vec![col("status", ColumnType::Text)])],
            vec![],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert!(
            plan.is_empty(),
            "rule C: non-convention default not dropped: {plan:?}"
        );
    }

    #[test]
    fn enum_skipped_column_not_dropped_via_diagnostic() {
        // baseline has an enum-ish `status` column; desired omits it but records a
        // diagnostic for Post.status → pluralize(pascal_to_snake("Post")) == "posts".
        let base = vec![posts_with(vec![col(
            "status",
            ColumnType::Enum {
                variants: vec!["draft".into(), "live".into()],
            },
        )])];
        let want = parsed(
            vec![posts_with(vec![])],
            vec![diag("Post", "posts", "status")],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert!(
            plan.is_empty(),
            "rules D+E: parser-skipped column not dropped: {plan:?}"
        );
    }

    #[test]
    fn custom_table_name_skipped_column_not_dropped_via_diagnostic() {
        // A managed model `#[model(table = "app_users")]` with a skipped
        // enum/assoc field records a diagnostic whose resolved `table` is the
        // CUSTOM name `app_users` — NOT the convention name `users`. The baseline
        // `app_users` carries the extra unmodelled `role` column; the suppression
        // must match on the resolved table name so the column is treated as
        // "present but unmodelled" and NOT dropped — even under
        // `--allow-destructive`, which would otherwise DROP the real column and
        // its data.
        let mut base_table = Table::new("app_users", Backend::Postgres);
        base_table.managed = true;
        let mut id = col("id", ColumnType::Int64);
        id.primary_key = true;
        base_table.columns.push(id);
        base_table.primary_key.push("id".to_owned());
        base_table.columns.push(col(
            "role",
            ColumnType::Enum {
                variants: vec!["admin".into(), "member".into()],
            },
        ));

        let mut desired_table = Table::new("app_users", Backend::Postgres);
        desired_table.managed = true;
        let mut did = col("id", ColumnType::Int64);
        did.primary_key = true;
        desired_table.columns.push(did);
        desired_table.primary_key.push("id".to_owned());

        // Model `User` → convention table `users`, but the resolved table is the
        // `#[model(table = "app_users")]` override.
        let want = parsed(vec![desired_table], vec![diag("User", "app_users", "role")]);

        // Destructive-allowed: proves the suppression, not the destructive guard,
        // is what keeps the column.
        let plan = diff_schema(&[base_table], &want, ALLOW);
        assert!(
            plan.is_empty(),
            "custom-table diagnostic must suppress the DropColumn: {plan:?}"
        );
    }

    #[test]
    fn drop_still_fires_without_matching_diagnostic() {
        // A genuinely-removed column (no diagnostic) is still a DropColumn — the
        // suppression is exact, not blanket.
        let base = vec![posts_with(vec![col("legacy", ColumnType::Text)])];
        let want = parsed(
            vec![posts_with(vec![])],
            vec![diag("Post", "posts", "something_else")],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(plan.changes.len(), 1);
        assert!(matches!(plan.changes[0], SchemaChange::DropColumn { .. }));
    }

    #[test]
    fn index_on_diagnostic_skipped_column_is_not_dropped() {
        // baseline: an enum `status` column carrying a UNIQUE index. The parser
        // skips the enum field (diagnostic) so desired omits BOTH the column and
        // its index. The `DropColumn` is already suppressed; the `DropIndex` must
        // be too — otherwise the retained column silently loses its UNIQUE
        // constraint and duplicate data becomes possible.
        let mut base_table = posts_with(vec![col(
            "status",
            ColumnType::Enum {
                variants: vec!["draft".into(), "live".into()],
            },
        )]);
        base_table.indexes.push(Index {
            name: "idx_posts_status".to_owned(),
            columns: vec!["status".to_owned()],
            unique: true,
        });
        let want = parsed(
            vec![posts_with(vec![])],
            vec![diag("Post", "posts", "status")],
        );
        let plan = diff_schema(&[base_table], &want, DEFAULT_OPTS);
        assert!(
            !plan
                .changes
                .iter()
                .any(|c| matches!(c, SchemaChange::DropIndex { .. })),
            "index on a parser-skipped column must not be dropped: {plan:?}"
        );
        assert!(
            plan.is_empty(),
            "column and its index both suppressed → empty plan: {plan:?}"
        );
    }

    #[test]
    fn index_on_normal_column_still_dropped() {
        // Control: an ordinary removed index (no diagnostic) still drops — the
        // suppression is exact, not blanket.
        let idx = Index {
            name: "idx_posts_body".to_owned(),
            columns: vec!["body".to_owned()],
            unique: false,
        };
        let mut base_table = posts_with(vec![col("body", ColumnType::Text)]);
        base_table.indexes.push(idx.clone());
        let want = parsed(
            vec![posts_with(vec![col("body", ColumnType::Text)])],
            vec![],
        );
        let plan = diff_schema(&[base_table], &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::DropIndex {
                table: "posts".to_owned(),
                index: idx,
            }]
        );
    }

    #[test]
    fn composite_index_touching_skipped_column_is_not_dropped() {
        // A composite UNIQUE index over (author_id, status) where `status` is
        // parser-skipped. If ANY column is skipped the whole drop is suppressed —
        // the parser cannot authoritatively say the index was removed, and
        // dropping it would silently lose the multi-column uniqueness.
        let mut base_table = posts_with(vec![
            col("author_id", ColumnType::Int64),
            col(
                "status",
                ColumnType::Enum {
                    variants: vec!["draft".into(), "live".into()],
                },
            ),
        ]);
        base_table.indexes.push(Index {
            name: "idx_posts_author_status".to_owned(),
            columns: vec!["author_id".to_owned(), "status".to_owned()],
            unique: true,
        });
        // desired retains `author_id` but the enum `status` is skipped (diagnostic),
        // so the parser never sees the composite index either.
        let want = parsed(
            vec![posts_with(vec![col("author_id", ColumnType::Int64)])],
            vec![diag("Post", "posts", "status")],
        );
        let plan = diff_schema(&[base_table], &want, DEFAULT_OPTS);
        assert!(
            !plan
                .changes
                .iter()
                .any(|c| matches!(c, SchemaChange::DropIndex { .. })),
            "composite index touching a skipped column must not be dropped: {plan:?}"
        );
    }

    #[test]
    fn create_table_with_diagnostic_skipped_field_is_refused() {
        // A brand-new managed table whose model has a skipped enum field parses to
        // a `Table` OMITTING that column (only a diagnostic is recorded). Emitting
        // `CREATE TABLE` from that partial output would create a table missing a
        // column the generated model queries → runtime "column does not exist". The
        // diff must surface the refused marker, and `guard_plan` must reject it
        // (with no override) naming the missing field.
        let mut new_table = Table::new("events", Backend::Postgres);
        new_table.managed = true;
        let mut id = col("id", ColumnType::Int64);
        id.primary_key = true;
        new_table.columns.push(id);
        new_table.primary_key.push("id".to_owned());
        // The `kind` enum field is skipped by the parser → not a column, only a diag.
        let want = parsed(vec![new_table], vec![diag("Event", "events", "kind")]);
        let plan = diff_schema(&[], &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::CreateTableBlockedBySkippedField {
                table: "events".to_owned(),
                fields: vec!["kind".to_owned()],
            }],
            "a diagnostic-skipped new table must not emit CREATE TABLE: {plan:?}"
        );
        match guard_plan(&plan, DEFAULT_OPTS) {
            Err(DiffError::CreateTableWithSkippedField { table, fields }) => {
                assert_eq!(table, "events");
                assert_eq!(fields, vec!["kind".to_owned()]);
            }
            other => panic!("expected CreateTableWithSkippedField refusal, got {other:?}"),
        }
        // No override — even --allow-destructive refuses (incomplete DDL, not lossy).
        assert!(matches!(
            guard_plan(&plan, ALLOW),
            Err(DiffError::CreateTableWithSkippedField { .. })
        ));
        // The message names both the table and the missing field.
        let msg = guard_plan(&plan, DEFAULT_OPTS).unwrap_err().to_string();
        assert!(
            msg.contains("events") && msg.contains("kind"),
            "refusal message names table + field: {msg}"
        );
    }

    #[test]
    fn create_table_without_diagnostics_is_allowed() {
        // Control: a clean new managed table (no skipped fields) still emits
        // CREATE TABLE and passes the guard.
        let new_table = posts_ref_table("events", col("title", ColumnType::Text));
        let want = parsed(vec![new_table.clone()], vec![]);
        let plan = diff_schema(&[], &want, DEFAULT_OPTS);
        assert_eq!(plan.changes, vec![SchemaChange::CreateTable(new_table)]);
        assert!(
            guard_plan(&plan, DEFAULT_OPTS).is_ok(),
            "a clean new table is emittable"
        );
    }

    #[test]
    fn add_fk_to_preexisting_column_is_refused() {
        // Finding 902: a PRE-EXISTING generated association column (`author_id`,
        // present in the baseline with `references: None` because the parser cannot
        // see the `#[belongs_to(...)]` association FK) is changed to an explicit
        // `#[references]`. The DB may already carry the inline `posts_author_id_fkey`
        // constraint from the original association-FK generation, so emitting
        // `ADD CONSTRAINT posts_author_id_fkey` would collide → an unappliable
        // migration. The diff must surface the refused marker, never an
        // `AddForeignKey`.
        let base = vec![posts_with(vec![col("author_id", ColumnType::Int64)])];
        let mut author = col("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "id"));
        let want = parsed(vec![posts_with(vec![author])], vec![]);
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::AddForeignKeyToExistingColumn {
                table: "posts".to_owned(),
                column: "author_id".to_owned(),
            }],
            "an FK added to a pre-existing column is the refused marker, never an AddForeignKey: \
             {plan:?}"
        );
        assert!(
            !plan
                .changes
                .iter()
                .any(|c| matches!(c, SchemaChange::AddForeignKey { .. })),
            "must never emit an AddForeignKey on a pre-existing column"
        );

        // The guard refuses it — with no override (the SQL may be unappliable).
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        assert!(
            matches!(err, DiffError::AddForeignKeyToPreexistingColumn { .. }),
            "guard refuses the pre-existing-column FK add: {err:?}"
        );
        assert!(
            err.to_string().contains("posts_author_id_fkey"),
            "the error explains the constraint-name collision: {err}"
        );
        // --allow-destructive does NOT override it (no safe way to skip a duplicate).
        assert!(matches!(
            guard_plan(&plan, ALLOW).unwrap_err(),
            DiffError::AddForeignKeyToPreexistingColumn { .. }
        ));
    }

    #[test]
    fn add_fk_on_new_column_is_allowed() {
        // Control for finding 902: a BRAND-NEW FK column (absent from the baseline)
        // is safe — there is no pre-existing constraint to collide with. It arrives
        // as an `AddColumn` whose `REFERENCES` renders inline in the `ADD COLUMN`
        // (never as a separate `AddForeignKey`), so it is neither refused nor a
        // marker, and the emitted SQL carries the foreign key.
        let base = vec![posts_with(vec![])];
        let mut author = col("author_id", ColumnType::Int64);
        author.nullable = true;
        author.references = Some(ForeignKey::new("users", "id"));
        let want = parsed(vec![posts_with(vec![author.clone()])], vec![]);
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::AddColumn {
                table: "posts".to_owned(),
                column: author,
            }],
            "a new FK column is a plain AddColumn, not a refused marker: {plan:?}"
        );
        // It passes the guard and emits the inline REFERENCES.
        guard_plan(&plan, DEFAULT_OPTS).expect("a new FK column is emittable");
        let up = emit_up_sql(&plan).expect("emit up");
        assert!(
            up.contains("ADD COLUMN author_id BIGINT NULL REFERENCES users(id)"),
            "the new FK column emits its inline REFERENCES: {up}"
        );
    }

    #[test]
    fn fk_retarget_is_a_refused_marker_not_a_duplicate_add() {
        // Regression (finding 1): baseline `author_id` already has an explicit FK
        // to users(id); desired retargets it to accounts(id). Emitting a plain
        // `AddForeignKey` would `ADD CONSTRAINT posts_author_id_fkey` a second
        // time and collide with the baseline constraint of the same name. The diff
        // must instead surface the refused `ForeignKeyChange` marker (never an
        // `AddForeignKey`).
        let mut base_author = col("author_id", ColumnType::Int64);
        base_author.references = Some(ForeignKey::new("users", "id"));
        let base = vec![posts_with(vec![base_author])];

        let mut want_author = col("author_id", ColumnType::Int64);
        want_author.references = Some(ForeignKey::new("accounts", "id"));
        let want = parsed(vec![posts_with(vec![want_author])], vec![]);

        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::ForeignKeyChange {
                table: "posts".to_owned(),
                column: "author_id".to_owned(),
            }],
            "an FK retarget is the refused marker, not an AddForeignKey: {plan:?}"
        );
        assert!(
            !plan
                .changes
                .iter()
                .any(|c| matches!(c, SchemaChange::AddForeignKey { .. })),
            "must never emit a duplicate AddForeignKey on retarget"
        );

        // The guard refuses it — with no override, like a PK change.
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        assert!(
            matches!(err, DiffError::ForeignKeyChange { .. }),
            "guard refuses the retarget: {err:?}"
        );
        assert!(
            err.to_string().contains("posts_author_id_fkey"),
            "the error explains the constraint-name collision: {err}"
        );
        // --allow-destructive does NOT override it (no safe drop+recreate).
        assert!(matches!(
            guard_plan(&plan, ALLOW).unwrap_err(),
            DiffError::ForeignKeyChange { .. }
        ));
    }

    #[test]
    fn fk_unchanged_target_is_no_change() {
        // Same explicit FK on both sides → nothing to do (not a retarget marker).
        let mut base_author = col("author_id", ColumnType::Int64);
        base_author.references = Some(ForeignKey::new("users", "id"));
        let mut want_author = col("author_id", ColumnType::Int64);
        want_author.references = Some(ForeignKey::new("users", "id"));
        let plan = diff_schema(
            &[posts_with(vec![base_author])],
            &parsed(vec![posts_with(vec![want_author])], vec![]),
            DEFAULT_OPTS,
        );
        assert!(plan.is_empty(), "unchanged FK → empty plan: {plan:?}");
    }

    #[test]
    fn colliding_over_long_fk_names_get_distinct_bounded_names() {
        // Deferred item #4: a >63-byte table name plus two added FK columns whose
        // raw `{table}_{column}_fkey` names share their first 63 bytes would, under
        // plain PG truncation, collide into one duplicate relation. The bounded
        // scheme appends a hash of the FULL untruncated name, so the two now get
        // DISTINCT valid ≤63-byte names and the plan is accepted, not refused.
        let long_table = "a".repeat(60); // {table}_{col}_fkey is well over 63 bytes.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::AddForeignKey {
                    table: long_table.clone(),
                    column: "author_id".to_owned(),
                    foreign_key: ForeignKey::new("users", "id"),
                },
                SchemaChange::AddForeignKey {
                    table: long_table.clone(),
                    column: "editor_id".to_owned(),
                    foreign_key: ForeignKey::new("users", "id"),
                },
            ],
        };
        guard_plan(&plan, DEFAULT_OPTS).expect("distinct bounded FK names are no longer refused");

        let author = bounded_pg_identifier(&format!("{long_table}_author_id_fkey"));
        let editor = bounded_pg_identifier(&format!("{long_table}_editor_id_fkey"));
        assert!(
            author.len() <= PG_MAX_IDENTIFIER_BYTES && editor.len() <= PG_MAX_IDENTIFIER_BYTES,
            "both bounded names fit the limit: {author} / {editor}"
        );
        assert_ne!(
            author, editor,
            "the hash suffix keeps the two colliding-prefix names distinct"
        );

        let up = emit_up_sql(&plan).expect("emit up");
        assert!(up.contains(&format!("ADD CONSTRAINT {author} ")), "{up}");
        assert!(up.contains(&format!("ADD CONSTRAINT {editor} ")), "{up}");
    }

    #[test]
    fn normal_length_fk_names_are_allowed() {
        // Ordinary table/column names produce a `posts_author_id_fkey` well under
        // 63 bytes — the guard passes and the plan emits.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddForeignKey {
                table: "posts".to_owned(),
                column: "author_id".to_owned(),
                foreign_key: ForeignKey::new("users", "id"),
            }],
        };
        assert!(
            guard_plan(&plan, DEFAULT_OPTS).is_ok(),
            "normal-length FK names must not be refused"
        );
    }

    #[test]
    fn over_long_index_name_is_refused() {
        // The same 63-byte hazard applies to generated index names emitted as
        // top-level `CREATE INDEX`. An index name over the limit is refused.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddIndex {
                table: "posts".to_owned(),
                index: Index {
                    name: format!("idx_{}", "x".repeat(70)),
                    columns: vec!["body".to_owned()],
                    unique: false,
                },
            }],
        };
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        assert!(
            matches!(err, DiffError::GeneratedIdentifierTooLong { .. }),
            "over-long index name is refused: {err:?}"
        );
    }

    #[test]
    fn over_long_generated_name_is_allowed_on_sqlite() {
        // SQLite has no comparable short identifier limit, so the check is
        // Postgres-only. (AddForeignKey does not render on SQLite anyway, but the
        // guard must not refuse the plan on backend grounds.)
        let plan = MigrationPlan {
            backend: Backend::Sqlite,
            changes: vec![SchemaChange::AddIndex {
                table: "posts".to_owned(),
                index: Index {
                    name: format!("idx_{}", "x".repeat(70)),
                    columns: vec!["body".to_owned()],
                    unique: false,
                },
            }],
        };
        assert!(
            guard_plan(&plan, DEFAULT_OPTS).is_ok(),
            "the 63-byte guard is Postgres-only"
        );
    }

    #[test]
    fn duplicate_generated_index_names_are_refused() {
        // A `#[unique] foo` field yields `idx_<table>_foo_unique` while a separate
        // field literally named `foo_unique` yields the *same* `idx_<table>_foo_unique`
        // (parse.rs). Both are within 63 bytes, so the truncation-collision path never
        // fires — but PG accepts the first `CREATE INDEX` and rejects the second as a
        // duplicate relation. The guard must refuse the plan and name the duplicate.
        let dup = "idx_posts_foo_unique".to_owned();
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::AddIndex {
                    table: "posts".to_owned(),
                    index: Index {
                        name: dup.clone(),
                        columns: vec!["foo".to_owned()],
                        unique: true,
                    },
                },
                SchemaChange::AddIndex {
                    table: "posts".to_owned(),
                    index: Index {
                        name: dup.clone(),
                        columns: vec!["foo_unique".to_owned()],
                        unique: false,
                    },
                },
            ],
        };
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        assert!(
            matches!(err, DiffError::GeneratedIdentifierTooLong { .. }),
            "an exact-duplicate generated index name is refused: {err:?}"
        );
        assert!(
            err.to_string().contains(&dup),
            "the error names the duplicated identifier: {err}"
        );
        // No override — the SQL is unappliable, not merely lossy.
        assert!(matches!(
            guard_plan(&plan, ALLOW).unwrap_err(),
            DiffError::GeneratedIdentifierTooLong { .. }
        ));
    }

    #[test]
    fn duplicate_generated_names_on_create_table_are_refused() {
        // The same collision emitted inline on a brand-new table's `CREATE TABLE`
        // (both indexes live on `table.indexes`) is likewise refused.
        let dup = "idx_posts_foo_unique".to_owned();
        let mut table = posts_with(vec![
            col("foo", ColumnType::Text),
            col("foo_unique", ColumnType::Text),
        ]);
        table.indexes = vec![
            Index {
                name: dup.clone(),
                columns: vec!["foo".to_owned()],
                unique: true,
            },
            Index {
                name: dup.clone(),
                columns: vec!["foo_unique".to_owned()],
                unique: false,
            },
        ];
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::CreateTable(table)],
        };
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        assert!(
            matches!(err, DiffError::GeneratedIdentifierTooLong { .. }),
            "duplicate inline index names on CREATE TABLE are refused: {err:?}"
        );
        assert!(err.to_string().contains(&dup), "names the duplicate: {err}");
    }

    #[test]
    fn distinct_index_names_are_allowed() {
        // Control: two legitimately-distinct index names (both well under 63 bytes)
        // must not be treated as a collision.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::AddIndex {
                    table: "posts".to_owned(),
                    index: Index {
                        name: "idx_posts_foo".to_owned(),
                        columns: vec!["foo".to_owned()],
                        unique: false,
                    },
                },
                SchemaChange::AddIndex {
                    table: "posts".to_owned(),
                    index: Index {
                        name: "idx_posts_bar".to_owned(),
                        columns: vec!["bar".to_owned()],
                        unique: false,
                    },
                },
            ],
        };
        assert!(
            guard_plan(&plan, DEFAULT_OPTS).is_ok(),
            "distinct index names must not be refused as a collision"
        );
    }

    #[test]
    fn unique_index_add_on_existing_column_is_refused() {
        // Marking a pre-existing column `#[unique]` yields a unique `AddIndex`
        // whose only column (`email`) already existed — `CREATE UNIQUE INDEX`
        // would fail if the table's existing rows hold duplicate emails. The
        // engine cannot dedup offline, so it refuses (no override — unappliable,
        // not merely destructive).
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddIndex {
                table: "posts".to_owned(),
                index: Index {
                    name: "idx_posts_email_unique".to_owned(),
                    columns: vec!["email".to_owned()],
                    unique: true,
                },
            }],
        };
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        assert!(
            matches!(err, DiffError::UniqueIndexRequiresDedup { .. }),
            "a unique index on a pre-existing column is refused: {err:?}"
        );
        assert!(
            err.to_string().contains("idx_posts_email_unique") && err.to_string().contains("posts"),
            "the error names the index and table: {err}"
        );
        // No override — the SQL is unappliable, not merely lossy.
        assert!(
            matches!(
                guard_plan(&plan, ALLOW).unwrap_err(),
                DiffError::UniqueIndexRequiresDedup { .. }
            ),
            "--allow-destructive does not override an unappliable unique-index add"
        );
    }

    #[test]
    fn unique_index_add_on_new_column_is_allowed() {
        // A brand-new nullable column that also carries `#[unique]` arrives as an
        // `AddColumn` plus a unique `AddIndex` on that same new column. Every
        // existing row is NULL in the new column, and Postgres treats NULLs as
        // distinct in a unique index, so `CREATE UNIQUE INDEX` cannot conflict —
        // the plan is emittable.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::AddColumn {
                    table: "posts".to_owned(),
                    column: {
                        let mut c = col("email", ColumnType::Text);
                        c.nullable = true;
                        c
                    },
                },
                SchemaChange::AddIndex {
                    table: "posts".to_owned(),
                    index: Index {
                        name: "idx_posts_email_unique".to_owned(),
                        columns: vec!["email".to_owned()],
                        unique: true,
                    },
                },
            ],
        };
        assert!(
            guard_plan(&plan, DEFAULT_OPTS).is_ok(),
            "a unique index on a newly-added column must not be refused"
        );
    }

    #[test]
    fn unique_index_on_new_table_is_allowed() {
        // A unique index on a brand-new table rides inline on `CreateTable` (an
        // empty table — no rows to conflict), never as a top-level `AddIndex`, so
        // it is never refused.
        let mut table = posts_with(vec![col("email", ColumnType::Text)]);
        table.indexes = vec![Index {
            name: "idx_posts_email_unique".to_owned(),
            columns: vec!["email".to_owned()],
            unique: true,
        }];
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::CreateTable(table)],
        };
        assert!(
            guard_plan(&plan, DEFAULT_OPTS).is_ok(),
            "a unique index on a brand-new table must not be refused"
        );
    }

    #[test]
    fn non_unique_index_add_on_existing_column_still_allowed() {
        // Control: a plain (non-unique) index on a pre-existing column is always
        // appliable and must still emit.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddIndex {
                table: "posts".to_owned(),
                index: Index {
                    name: "idx_posts_email".to_owned(),
                    columns: vec!["email".to_owned()],
                    unique: false,
                },
            }],
        };
        assert!(
            guard_plan(&plan, DEFAULT_OPTS).is_ok(),
            "a non-unique index on a pre-existing column must not be refused"
        );
    }

    // -- 13.3 guards ---------------------------------------------------------

    fn drop_column_plan() -> MigrationPlan {
        MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::DropColumn {
                table: "users".to_owned(),
                column: col("nickname", ColumnType::Text),
            }],
        }
    }

    #[test]
    fn drop_column_refused_without_flag() {
        let err = guard_plan(&drop_column_plan(), DEFAULT_OPTS).unwrap_err();
        match err {
            DiffError::Destructive { summary, ops } => {
                assert!(
                    summary.contains("users.nickname"),
                    "names the column: {summary}"
                );
                assert_eq!(
                    ops,
                    vec![DestructiveOp::Column {
                        table: "users".to_owned(),
                        column: "nickname".to_owned(),
                    }]
                );
            }
            other => panic!("expected Destructive, got {other:?}"),
        }
    }

    #[test]
    fn drop_column_allowed_with_flag() {
        assert!(guard_plan(&drop_column_plan(), ALLOW).is_ok());
    }

    #[test]
    fn drop_table_refused_then_allowed() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::DropTable(posts_with(vec![]))],
        };
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        assert!(matches!(err, DiffError::Destructive { .. }));
        assert!(guard_plan(&plan, ALLOW).is_ok());
    }

    #[test]
    fn possible_rename_refused() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::DropColumn {
                    table: "users".to_owned(),
                    column: col("nickname", ColumnType::Text),
                },
                SchemaChange::AddColumn {
                    table: "users".to_owned(),
                    column: col("handle", ColumnType::Text),
                },
            ],
        };
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        match err {
            DiffError::PossibleRename {
                table,
                dropped,
                added,
            } => {
                assert_eq!(table, "users");
                assert_eq!(dropped, vec!["nickname".to_owned()]);
                assert_eq!(added, vec!["handle".to_owned()]);
                assert!(
                    err_message_mentions_renamed_from(&DiffError::PossibleRename {
                        table: "users".to_owned(),
                        dropped: vec!["nickname".to_owned()],
                        added: vec!["handle".to_owned()],
                    }),
                    "message mentions #[renamed_from]"
                );
            }
            other => panic!("expected PossibleRename, got {other:?}"),
        }
    }

    fn err_message_mentions_renamed_from(err: &DiffError) -> bool {
        err.to_string().contains("#[renamed_from]")
    }

    #[test]
    fn possible_rename_overridden_by_allow_destructive() {
        // The added `handle` is nullable so it is itself appliable — this test
        // exercises the rename→independent-drop/add override in isolation, not the
        // orthogonal required-column guard (an independent add of a NOT NULL,
        // no-default column is unappliable and is refused even under --allow-destructive;
        // see `add_not_null_column_without_default_is_refused`).
        let mut handle = col("handle", ColumnType::Text);
        handle.nullable = true;
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::DropColumn {
                    table: "users".to_owned(),
                    column: col("nickname", ColumnType::Text),
                },
                SchemaChange::AddColumn {
                    table: "users".to_owned(),
                    column: handle,
                },
            ],
        };
        assert!(guard_plan(&plan, ALLOW).is_ok());
    }

    #[test]
    fn primary_key_change_refused() {
        // PK ["id"] → ["uuid_id"] on a both-present managed table.
        let base = vec![posts_with(vec![])];
        let mut want_table = Table::new("posts", Backend::Postgres);
        let mut uuid_id = col("uuid_id", ColumnType::Uuid);
        uuid_id.primary_key = true;
        want_table.primary_key.push("uuid_id".to_owned());
        want_table.columns.push(uuid_id);
        let plan = diff_schema(&base, &parsed(vec![want_table], vec![]), DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::PrimaryKeyChange {
                table: "posts".to_owned(),
            }]
        );
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        assert!(matches!(err, DiffError::PrimaryKeyChange { .. }));
        // No override — even --allow-destructive refuses.
        let err = guard_plan(&plan, ALLOW).unwrap_err();
        assert!(matches!(err, DiffError::PrimaryKeyChange { .. }));
    }

    // -- 13.4 managed scoping ------------------------------------------------

    #[test]
    fn unmanaged_desired_table_not_created() {
        let mut t = posts_with(vec![]);
        t.managed = false;
        let plan = diff_schema(&[], &parsed(vec![t], vec![]), DEFAULT_OPTS);
        assert!(
            plan.is_empty(),
            "unmanaged desired table not created: {plan:?}"
        );
    }

    #[test]
    fn unmanaged_baseline_table_not_dropped() {
        let mut t = posts_with(vec![]);
        t.managed = false;
        let plan = diff_schema(&[t], &parsed(vec![], vec![]), DEFAULT_OPTS);
        assert!(
            plan.is_empty(),
            "unmanaged baseline table not dropped: {plan:?}"
        );
    }

    // -- 13.5 SQL emission ---------------------------------------------------

    #[test]
    fn create_table_bigserial_pk_renders_bigserial() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::CreateTable(posts_with(vec![col(
                "body",
                ColumnType::Text,
            )]))],
        };
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("id BIGSERIAL PRIMARY KEY"),
            "bigserial PK: {up}"
        );
        assert!(
            !up.contains("id BIGINT"),
            "must not render BIGINT for the id PK: {up}"
        );
        assert!(up.contains("body TEXT NOT NULL"), "body column: {up}");
    }

    #[test]
    fn create_table_uuid_pk_renders_gen_random_uuid() {
        let mut t = Table::new("posts", Backend::Postgres);
        let mut id = col("id", ColumnType::Uuid);
        id.primary_key = true;
        t.primary_key.push("id".to_owned());
        t.columns.push(id);
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::CreateTable(t)],
        };
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("id UUID PRIMARY KEY DEFAULT gen_random_uuid()"),
            "uuid PK: {up}"
        );
    }

    #[test]
    fn create_table_composite_pk_uses_table_clause() {
        let mut t = Table::new("memberships", Backend::Postgres);
        let mut a = col("user_id", ColumnType::Int64);
        a.primary_key = true;
        let mut b = col("group_id", ColumnType::Int64);
        b.primary_key = true;
        t.columns.push(a);
        t.columns.push(b);
        t.primary_key.push("user_id".to_owned());
        t.primary_key.push("group_id".to_owned());
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::CreateTable(t)],
        };
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("user_id BIGINT NOT NULL"),
            "columns rendered normally: {up}"
        );
        assert!(
            up.contains("PRIMARY KEY (user_id, group_id)"),
            "table-level PK: {up}"
        );
    }

    #[test]
    fn add_column_not_null_no_default_has_safety_comment() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddColumn {
                table: "posts".to_owned(),
                column: col("title", ColumnType::Text),
            }],
        };
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("-- autumn-safety: potentially-blocking"),
            "safety comment: {up}"
        );
        assert!(
            up.contains("ALTER TABLE posts ADD COLUMN title TEXT NOT NULL;"),
            "{up}"
        );
    }

    #[test]
    fn add_column_nullable_no_safety_comment() {
        let mut bio = col("bio", ColumnType::Text);
        bio.nullable = true;
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddColumn {
                table: "posts".to_owned(),
                column: bio,
            }],
        };
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            !up.contains("autumn-safety"),
            "no safety comment for nullable: {up}"
        );
        assert!(
            up.contains("ALTER TABLE posts ADD COLUMN bio TEXT NULL;"),
            "{up}"
        );
    }

    #[test]
    fn add_not_null_column_without_default_is_refused() {
        // Adding a NOT NULL, no-default column to an *existing* table is unappliable
        // on a table that already has rows — refused, with no override.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddColumn {
                table: "posts".to_owned(),
                column: col("title", ColumnType::Text),
            }],
        };
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        match &err {
            DiffError::RequiredColumnWithoutDefault { table, column } => {
                assert_eq!(table, "posts");
                assert_eq!(column, "title");
            }
            other => panic!("expected RequiredColumnWithoutDefault, got {other:?}"),
        }
        assert!(
            err.to_string()
                .contains("cannot add required column `posts.title`"),
            "message: {err}"
        );
        // No override — --allow-destructive does not permit it (it is unappliable,
        // not merely destructive).
        assert!(
            matches!(
                guard_plan(&plan, ALLOW).unwrap_err(),
                DiffError::RequiredColumnWithoutDefault { .. }
            ),
            "must refuse even with --allow-destructive"
        );
    }

    #[test]
    fn add_not_null_column_with_default_is_allowed() {
        // A NOT NULL column WITH a default is appliable (existing rows get the
        // default) — emit it normally, do not refuse.
        let mut created = col("created_at", ColumnType::Timestamp);
        created.default = Some(ColumnDefault::Now);
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddColumn {
                table: "posts".to_owned(),
                column: created,
            }],
        };
        guard_plan(&plan, DEFAULT_OPTS).expect("NOT NULL with a default is appliable");
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains(
                "ALTER TABLE posts ADD COLUMN created_at TIMESTAMP NOT NULL DEFAULT NOW();"
            ),
            "{up}"
        );
    }

    #[test]
    fn add_nullable_column_is_allowed() {
        // A nullable added column is always appliable.
        let mut bio = col("bio", ColumnType::Text);
        bio.nullable = true;
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddColumn {
                table: "posts".to_owned(),
                column: bio,
            }],
        };
        guard_plan(&plan, DEFAULT_OPTS).expect("nullable add is appliable");
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("ALTER TABLE posts ADD COLUMN bio TEXT NULL;"),
            "{up}"
        );
    }

    #[test]
    fn create_table_with_not_null_no_default_column_is_not_refused() {
        // A NOT NULL, no-default column inside a brand-new CreateTable is fine —
        // the table is empty — so the required-column guard must NOT fire on it.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::CreateTable(posts_with(vec![col(
                "title",
                ColumnType::Text,
            )]))],
        };
        guard_plan(&plan, DEFAULT_OPTS).expect("CreateTable with a NOT NULL column is empty-safe");
        let up = emit_up_sql(&plan).expect("emit");
        assert!(up.contains("title TEXT NOT NULL"), "{up}");
    }

    #[test]
    fn set_not_null_on_existing_column_is_refused() {
        // Turning an existing nullable column non-null is unappliable on a table
        // whose column already holds NULLs — refused, with no override (the exact
        // sibling of the required-column refusal).
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::SetNotNull {
                table: "posts".to_owned(),
                column: "bio".to_owned(),
            }],
        };
        let err = guard_plan(&plan, DEFAULT_OPTS).unwrap_err();
        match &err {
            DiffError::SetNotNullRequiresBackfill { table, column } => {
                assert_eq!(table, "posts");
                assert_eq!(column, "bio");
            }
            other => panic!("expected SetNotNullRequiresBackfill, got {other:?}"),
        }
        assert!(
            err.to_string().contains("cannot set `posts.bio` NOT NULL"),
            "message: {err}"
        );
        // No override — --allow-destructive does not permit it (it is unappliable,
        // not merely destructive).
        assert!(
            matches!(
                guard_plan(&plan, ALLOW).unwrap_err(),
                DiffError::SetNotNullRequiresBackfill { .. }
            ),
            "must refuse even with --allow-destructive"
        );
    }

    #[test]
    fn drop_not_null_is_allowed() {
        // The inverse (non-null → nullable) is always appliable — emit it
        // normally, never refuse.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::DropNotNull {
                table: "posts".to_owned(),
                column: "bio".to_owned(),
            }],
        };
        guard_plan(&plan, DEFAULT_OPTS).expect("DROP NOT NULL is always appliable");
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("ALTER TABLE posts ALTER COLUMN bio DROP NOT NULL;"),
            "{up}"
        );
    }

    #[test]
    fn add_column_renders_only_the_column_not_the_index() {
        // The `AddColumn` renderer is NOT the index owner: a lone `AddColumn` for a
        // reference column emits the column + its FK clause but no `CREATE INDEX`.
        // The reference auto-index arrives as a separate `AddIndex` (see
        // `add_reference_column_emits_index_exactly_once`), so rendering it inline
        // here would double it and the migration would fail.
        let mut author = col("author_id", ColumnType::Int64);
        author.nullable = true;
        author.references = Some(ForeignKey::new("users", "id"));
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddColumn {
                table: "posts".to_owned(),
                column: author,
            }],
        };
        let up = emit_up_sql(&plan).expect("emit");
        assert!(up.contains("REFERENCES users(id)"), "fk clause: {up}");
        assert!(
            !up.contains("CREATE INDEX"),
            "the reference index is a separate AddIndex change, not inline: {up}"
        );
    }

    #[test]
    fn add_reference_column_emits_index_exactly_once() {
        // Regression (finding 2): the slice-2 parser folds `idx_<t>_<c>` into
        // `table.indexes` AND the column carries `references`. Diffing a new
        // reference column onto an existing table therefore yields both an
        // `AddColumn` and an `AddIndex` for that index — the emitted up.sql must
        // contain the `CREATE INDEX` exactly once, never a duplicate.
        let base = vec![posts_with(vec![])];
        let mut author = col("author_id", ColumnType::Int64);
        author.nullable = true;
        author.references = Some(ForeignKey::new("users", "id"));
        let mut want_table = posts_with(vec![author]);
        want_table.indexes.push(Index {
            name: "idx_posts_author_id".to_owned(),
            columns: vec!["author_id".to_owned()],
            unique: false,
        });
        let plan = diff_schema(&base, &parsed(vec![want_table], vec![]), DEFAULT_OPTS);
        let up = emit_up_sql(&plan).expect("emit");
        assert_eq!(
            up.matches("CREATE INDEX idx_posts_author_id").count(),
            1,
            "the reference index must be emitted exactly once: {up}"
        );
        // And the column itself is still added.
        assert!(
            up.contains("ALTER TABLE posts ADD COLUMN author_id BIGINT NULL REFERENCES users(id);"),
            "the column is still added: {up}"
        );
    }

    #[test]
    fn alter_type_and_set_not_null_templates() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::AlterColumnType {
                    table: "posts".to_owned(),
                    column: "views".to_owned(),
                    from: ColumnType::Int32,
                    to: ColumnType::Int64,
                },
                SchemaChange::SetNotNull {
                    table: "posts".to_owned(),
                    column: "views".to_owned(),
                },
            ],
        };
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("ALTER TABLE posts ALTER COLUMN views TYPE BIGINT;"),
            "{up}"
        );
        assert!(
            up.contains("ALTER TABLE posts ALTER COLUMN views SET NOT NULL;"),
            "{up}"
        );
    }

    #[test]
    fn create_unique_index_and_drop_index_templates() {
        let up = index_sql(
            "posts",
            &Index {
                name: "idx_posts_slug_unique".to_owned(),
                columns: vec!["slug".to_owned()],
                unique: true,
            },
        );
        assert_eq!(
            up,
            "CREATE UNIQUE INDEX idx_posts_slug_unique ON posts (slug);"
        );

        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::DropIndex {
                table: "posts".to_owned(),
                index: Index {
                    name: "idx_posts_slug".to_owned(),
                    columns: vec!["slug".to_owned()],
                    unique: false,
                },
            }],
        };
        // DropIndex is not destructive, so it emits without a guard.
        let sql = emit_up_sql(&plan).expect("emit");
        assert_eq!(sql, "DROP INDEX idx_posts_slug;\n");
    }

    #[test]
    fn up_ordering_is_canonical() {
        // A mixed plan should render create → add col → drop col → drop table.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::DropTable(Table::new("legacy", Backend::Postgres)),
                SchemaChange::DropColumn {
                    table: "posts".to_owned(),
                    column: col("old", ColumnType::Text),
                },
                SchemaChange::AddIndex {
                    table: "posts".to_owned(),
                    index: Index {
                        name: "idx_posts_new".to_owned(),
                        columns: vec!["new".to_owned()],
                        unique: false,
                    },
                },
                SchemaChange::AddColumn {
                    table: "posts".to_owned(),
                    column: {
                        let mut c = col("new", ColumnType::Text);
                        c.nullable = true;
                        c
                    },
                },
                SchemaChange::CreateTable(posts_with(vec![])),
            ],
        };
        let up = emit_up_sql(&plan).expect("emit");
        let create = up.find("CREATE TABLE posts").expect("create present");
        let add = up.find("ADD COLUMN new").expect("add present");
        let index = up.find("idx_posts_new").expect("index present");
        let drop_col = up.find("DROP COLUMN old").expect("drop col present");
        let drop_table = up.find("DROP TABLE legacy").expect("drop table present");
        assert!(create < add, "create before add");
        assert!(add < index, "add before index");
        assert!(index < drop_col, "index before drop col");
        assert!(drop_col < drop_table, "drop col before drop table");
    }

    #[test]
    fn replaced_index_drops_before_add_in_up_and_inverts_in_down() {
        // Regression (finding 3): a same-named index whose shape changed becomes a
        // DropIndex(old) + AddIndex(new). In up.sql the DROP must precede the
        // CREATE (same name) or PG rejects the create; in down.sql the inverse
        // must drop the new before recreating the old.
        let old = Index {
            name: "idx_posts_slug".to_owned(),
            columns: vec!["slug".to_owned()],
            unique: false,
        };
        let new = Index {
            name: "idx_posts_slug".to_owned(),
            columns: vec!["slug".to_owned()],
            unique: true,
        };
        let mut base_table = posts_with(vec![col("slug", ColumnType::Text)]);
        base_table.indexes.push(old);
        let mut want_table = posts_with(vec![col("slug", ColumnType::Text)]);
        want_table.indexes.push(new);

        let plan = diff_schema(
            &[base_table],
            &parsed(vec![want_table], vec![]),
            DEFAULT_OPTS,
        );
        // Sanity: it really is a drop+add of the same name.
        assert_eq!(plan.changes.len(), 2, "drop+add of the same name: {plan:?}");

        let up = emit_up_sql(&plan).expect("emit");
        let drop = up
            .find("DROP INDEX idx_posts_slug;")
            .expect("up drop present");
        let create = up
            .find("CREATE UNIQUE INDEX idx_posts_slug")
            .expect("up create present");
        assert!(
            drop < create,
            "DROP must precede CREATE for a replaced index: {up}"
        );

        let down = emit_down_sql(&plan).expect("emit");
        let d_drop = down
            .find("DROP INDEX idx_posts_slug;")
            .expect("down drop present");
        let d_create = down
            .find("CREATE INDEX idx_posts_slug ON posts (slug);")
            .expect("down recreate (old, non-unique) present");
        assert!(
            d_drop < d_create,
            "down: drop the new before recreating the old: {down}"
        );
    }

    #[test]
    fn unrelated_drop_index_still_after_add_index() {
        // A DropIndex whose name is NOT re-added keeps the general (late) drop
        // bucket, so an unrelated add still renders before it.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::DropIndex {
                    table: "posts".to_owned(),
                    index: Index {
                        name: "idx_posts_old".to_owned(),
                        columns: vec!["old".to_owned()],
                        unique: false,
                    },
                },
                SchemaChange::AddIndex {
                    table: "posts".to_owned(),
                    index: Index {
                        name: "idx_posts_new".to_owned(),
                        columns: vec!["new".to_owned()],
                        unique: false,
                    },
                },
            ],
        };
        let up = emit_up_sql(&plan).expect("emit");
        let add = up.find("CREATE INDEX idx_posts_new").expect("add present");
        let drop = up.find("DROP INDEX idx_posts_old;").expect("drop present");
        assert!(
            add < drop,
            "an unrelated add still precedes an unrelated drop: {up}"
        );
    }

    #[test]
    fn create_tables_topologically_ordered_by_fk() {
        // Regression (finding 4): two new managed tables where `comments`
        // references `posts`. Lexically `comments` < `posts`, so a naive sort
        // would CREATE comments (REFERENCES posts) before posts exists → invalid.
        // The referenced table must be created first, and dropped last.
        let posts = posts_with(vec![col("body", ColumnType::Text)]);
        let mut post_ref = col("post_id", ColumnType::Int64);
        post_ref.references = Some(ForeignKey::new("posts", "id"));
        let comments = posts_ref_table("comments", post_ref);

        let plan = diff_schema(&[], &parsed(vec![comments, posts], vec![]), DEFAULT_OPTS);
        let up = emit_up_sql(&plan).expect("emit");
        let p = up.find("CREATE TABLE posts").expect("posts present");
        let c = up.find("CREATE TABLE comments").expect("comments present");
        assert!(
            p < c,
            "referenced `posts` must be created before referencing `comments`: {up}"
        );

        let down = emit_down_sql(&plan).expect("emit");
        let dp = down.find("DROP TABLE posts").expect("down posts");
        let dc = down.find("DROP TABLE comments").expect("down comments");
        assert!(
            dc < dp,
            "down: drop referencing `comments` before referenced `posts`: {down}"
        );
    }

    #[test]
    fn create_table_fk_cycle_is_refused() {
        // Two new tables referencing each other via inline FKs → unsatisfiable
        // (inline REFERENCES cannot express a cycle) → refused, not invalid SQL.
        let mut a_b = col("b_id", ColumnType::Int64);
        a_b.references = Some(ForeignKey::new("b", "id"));
        let table_a = posts_ref_table("a", a_b);
        let mut b_a = col("a_id", ColumnType::Int64);
        b_a.references = Some(ForeignKey::new("a", "id"));
        let table_b = posts_ref_table("b", b_a);

        let plan = diff_schema(&[], &parsed(vec![table_a, table_b], vec![]), DEFAULT_OPTS);
        let err = emit_up_sql(&plan).unwrap_err();
        let EmitError::CyclicTableDependencies { tables } = &err else {
            panic!("expected CyclicTableDependencies, got {err:?}");
        };
        assert_eq!(
            *tables,
            vec!["a".to_owned(), "b".to_owned()],
            "names the cycle"
        );
    }

    #[test]
    fn down_add_column_inverts_to_drop_clean() {
        let mut bio = col("bio", ColumnType::Text);
        bio.nullable = true;
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddColumn {
                table: "posts".to_owned(),
                column: bio,
            }],
        };
        let down = emit_down_sql(&plan).expect("emit");
        assert_eq!(down, "ALTER TABLE posts DROP COLUMN bio;\n");
        assert!(!down.contains("irreversible"), "clean, no marker: {down}");
    }

    #[test]
    fn down_drop_column_reads_add_with_irreversible_marker() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::DropColumn {
                table: "posts".to_owned(),
                column: {
                    let mut c = col("nickname", ColumnType::Text);
                    c.nullable = true;
                    c
                },
            }],
        };
        let down = emit_down_sql(&plan).expect("emit");
        assert!(
            down.contains("-- irreversible: column data dropped"),
            "irreversible marker: {down}"
        );
        assert!(
            down.contains("ALTER TABLE posts ADD COLUMN nickname TEXT NULL;"),
            "re-adds from baseline: {down}"
        );
    }

    #[test]
    fn down_drop_table_recreates_with_marker() {
        let mut t = posts_with(vec![col("body", ColumnType::Text)]);
        t.indexes.push(Index {
            name: "idx_posts_body".to_owned(),
            columns: vec!["body".to_owned()],
            unique: false,
        });
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::DropTable(t)],
        };
        let down = emit_down_sql(&plan).expect("emit");
        assert!(
            down.contains("-- irreversible: table data dropped"),
            "marker: {down}"
        );
        assert!(
            down.contains("CREATE TABLE posts"),
            "recreates the table: {down}"
        );
        assert!(
            down.contains("id BIGSERIAL PRIMARY KEY"),
            "recreates PK: {down}"
        );
        assert!(
            down.contains("CREATE INDEX idx_posts_body ON posts (body);"),
            "recreates its indexes: {down}"
        );
    }

    #[test]
    fn down_alter_type_marked_irreversible() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AlterColumnType {
                table: "posts".to_owned(),
                column: "views".to_owned(),
                from: ColumnType::Int32,
                to: ColumnType::Int64,
            }],
        };
        let down = emit_down_sql(&plan).expect("emit");
        assert!(
            down.contains("-- irreversible: a narrowing type change"),
            "marker: {down}"
        );
        assert!(
            down.contains("ALTER TABLE posts ALTER COLUMN views TYPE INTEGER;"),
            "structural inverse restores the `from` type: {down}"
        );
    }

    #[test]
    fn down_set_default_from_none_drops_default() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::SetDefault {
                table: "posts".to_owned(),
                column: "created_at".to_owned(),
                to: ColumnDefault::Now,
                from: None,
            }],
        };
        let down = emit_down_sql(&plan).expect("emit");
        assert_eq!(
            down,
            "ALTER TABLE posts ALTER COLUMN created_at DROP DEFAULT;\n"
        );
    }

    #[test]
    fn down_set_default_from_some_restores_it() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::SetDefault {
                table: "posts".to_owned(),
                column: "status".to_owned(),
                to: ColumnDefault::Sql("'live'".to_owned()),
                from: Some(ColumnDefault::Sql("'draft'".to_owned())),
            }],
        };
        let down = emit_down_sql(&plan).expect("emit");
        assert_eq!(
            down,
            "ALTER TABLE posts ALTER COLUMN status SET DEFAULT 'draft';\n"
        );
    }

    #[test]
    fn clean_plan_down_has_no_markers() {
        let mut bio = col("bio", ColumnType::Text);
        bio.nullable = true;
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::CreateTable(posts_with(vec![])),
                SchemaChange::AddColumn {
                    table: "posts".to_owned(),
                    column: bio,
                },
                SchemaChange::AddIndex {
                    table: "posts".to_owned(),
                    index: Index {
                        name: "idx_posts_bio".to_owned(),
                        columns: vec!["bio".to_owned()],
                        unique: false,
                    },
                },
            ],
        };
        let down = emit_down_sql(&plan).expect("emit");
        assert!(!down.contains("irreversible"), "no markers: {down}");
        assert!(!down.contains("manual"), "no markers: {down}");
    }

    #[test]
    fn down_add_check_named_drops_constraint_unnamed_is_manual() {
        let named = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddCheck {
                table: "posts".to_owned(),
                check: CheckConstraint {
                    name: Some("posts_body_len".to_owned()),
                    expression: "length(body) > 0".to_owned(),
                },
            }],
        };
        assert_eq!(
            emit_down_sql(&named).expect("emit"),
            "ALTER TABLE posts DROP CONSTRAINT posts_body_len;\n"
        );

        let unnamed = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddCheck {
                table: "posts".to_owned(),
                check: CheckConstraint {
                    name: None,
                    expression: "length(body) > 0".to_owned(),
                },
            }],
        };
        assert!(
            emit_down_sql(&unnamed)
                .expect("emit")
                .contains("-- manual: unnamed CHECK cannot be auto-dropped"),
            "unnamed check → manual note"
        );
    }

    #[test]
    fn add_foreign_key_up_and_down() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddForeignKey {
                table: "posts".to_owned(),
                column: "author_id".to_owned(),
                foreign_key: ForeignKey::new("users", "id"),
            }],
        };
        let up = emit_up_sql(&plan).expect("emit");
        assert_eq!(
            up,
            "ALTER TABLE posts ADD CONSTRAINT posts_author_id_fkey \
             FOREIGN KEY (author_id) REFERENCES users(id);\n"
        );
        let down = emit_down_sql(&plan).expect("emit");
        assert_eq!(
            down,
            "ALTER TABLE posts DROP CONSTRAINT posts_author_id_fkey;\n"
        );
    }

    /// A `SQLite`-tagged table: a `BigSerial` `id` PK plus the given extra columns
    /// and indexes.
    fn sqlite_table(name: &str, extra: Vec<Column>, indexes: Vec<Index>) -> Table {
        let mut t = Table::new(name, Backend::Sqlite);
        let mut id = col("id", ColumnType::Int64);
        id.primary_key = true;
        t.primary_key.push("id".to_owned());
        t.columns.push(id);
        t.columns.extend(extra);
        t.indexes = indexes;
        t
    }

    /// The shared fixture for the `SQLite` rebuild golden/coalescing/real-DB tests:
    /// `posts` alters `views` `Int32` → `Int64` and adds an index on `body`.
    fn sqlite_rebuild_fixture() -> (MigrationPlan, SchemaContext) {
        let idx = Index {
            name: "idx_posts_body".to_owned(),
            columns: vec!["body".to_owned()],
            unique: false,
        };
        let baseline = sqlite_table(
            "posts",
            vec![
                col("views", ColumnType::Int32),
                col("body", ColumnType::Text),
            ],
            vec![],
        );
        let desired = sqlite_table(
            "posts",
            vec![
                col("views", ColumnType::Int64),
                col("body", ColumnType::Text),
            ],
            vec![idx.clone()],
        );
        let plan = MigrationPlan {
            backend: Backend::Sqlite,
            changes: vec![
                SchemaChange::AlterColumnType {
                    table: "posts".to_owned(),
                    column: "views".to_owned(),
                    from: ColumnType::Int32,
                    to: ColumnType::Int64,
                },
                SchemaChange::AddIndex {
                    table: "posts".to_owned(),
                    index: idx,
                },
            ],
        };
        let ctx = SchemaContext::from_tables(
            std::slice::from_ref(&desired),
            std::slice::from_ref(&baseline),
        );
        (plan, ctx)
    }

    #[test]
    fn sqlite_alter_type_renders_table_rebuild_up_golden() {
        let (plan, ctx) = sqlite_rebuild_fixture();
        let up = emit_up_sql_with_context(&plan, &ctx).expect("emit up");
        let expected = "\
-- autumn: SQLite table rebuild for `posts` — ALTER-family changes require a full table recreate.
-- Transaction semantics: this migration must run inside a single transaction (diesel wraps each
-- migration in one). NOTE: `PRAGMA foreign_keys` is a no-op inside a transaction, so the harness
-- must ensure foreign-key enforcement is disabled around the migration.
-- autumn-safety: this recreate copies columns and re-creates indexes only. DROP TABLE drops the table's TRIGGERS (not restored here); VIEWS that reference the table are not dropped and may be left dangling or block the rename — re-create triggers and repair dependent views in a manual migration.
-- autumn-safety: `PRAGMA foreign_key_check` below only REPORTS violation rows (it does not raise); the recreate copies existing values verbatim and introduces no new orphans, but the migration runner must inspect the pragma's output to treat any pre-existing orphan as an error.
PRAGMA foreign_keys=OFF;
-- preserve the AUTOINCREMENT high-water mark so IDs issued-then-deleted are never reused
CREATE TEMP TABLE _autumn_seq_posts AS SELECT seq FROM sqlite_sequence WHERE name = 'posts';
CREATE TABLE posts__autumn_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    views INTEGER NOT NULL,
    body TEXT NOT NULL
);
INSERT INTO posts__autumn_new (id, views, body)
    SELECT id, views, body FROM posts;
DROP TABLE posts;
PRAGMA legacy_alter_table=ON;
ALTER TABLE posts__autumn_new RENAME TO posts;
PRAGMA legacy_alter_table=OFF;
CREATE INDEX idx_posts_body ON posts (body);
UPDATE sqlite_sequence SET seq = (SELECT seq FROM _autumn_seq_posts)
    WHERE name = 'posts' AND (SELECT seq FROM _autumn_seq_posts) IS NOT NULL;
INSERT INTO sqlite_sequence (name, seq)
    SELECT 'posts', (SELECT seq FROM _autumn_seq_posts)
    WHERE (SELECT seq FROM _autumn_seq_posts) IS NOT NULL
      AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'posts');
DROP TABLE _autumn_seq_posts;
PRAGMA foreign_key_check;
PRAGMA foreign_keys=ON;
";
        assert_eq!(up, expected, "SQLite up rebuild golden:\n{up}");
    }

    #[test]
    fn sqlite_alter_type_renders_table_rebuild_down_golden() {
        let (plan, ctx) = sqlite_rebuild_fixture();
        let down = emit_down_sql_with_context(&plan, &ctx).expect("emit down");
        let expected = "\
-- irreversible: a SQLite table rebuild rolled back to the prior shape may not restore data lost by a narrowing type change or dropped column
-- autumn: SQLite table rebuild for `posts` — ALTER-family changes require a full table recreate.
-- Transaction semantics: this migration must run inside a single transaction (diesel wraps each
-- migration in one). NOTE: `PRAGMA foreign_keys` is a no-op inside a transaction, so the harness
-- must ensure foreign-key enforcement is disabled around the migration.
-- autumn-safety: this recreate copies columns and re-creates indexes only. DROP TABLE drops the table's TRIGGERS (not restored here); VIEWS that reference the table are not dropped and may be left dangling or block the rename — re-create triggers and repair dependent views in a manual migration.
-- autumn-safety: `PRAGMA foreign_key_check` below only REPORTS violation rows (it does not raise); the recreate copies existing values verbatim and introduces no new orphans, but the migration runner must inspect the pragma's output to treat any pre-existing orphan as an error.
PRAGMA foreign_keys=OFF;
-- preserve the AUTOINCREMENT high-water mark so IDs issued-then-deleted are never reused
CREATE TEMP TABLE _autumn_seq_posts AS SELECT seq FROM sqlite_sequence WHERE name = 'posts';
CREATE TABLE posts__autumn_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    views INTEGER NOT NULL,
    body TEXT NOT NULL
);
INSERT INTO posts__autumn_new (id, views, body)
    SELECT id, views, body FROM posts;
DROP TABLE posts;
PRAGMA legacy_alter_table=ON;
ALTER TABLE posts__autumn_new RENAME TO posts;
PRAGMA legacy_alter_table=OFF;
UPDATE sqlite_sequence SET seq = (SELECT seq FROM _autumn_seq_posts)
    WHERE name = 'posts' AND (SELECT seq FROM _autumn_seq_posts) IS NOT NULL;
INSERT INTO sqlite_sequence (name, seq)
    SELECT 'posts', (SELECT seq FROM _autumn_seq_posts)
    WHERE (SELECT seq FROM _autumn_seq_posts) IS NOT NULL
      AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'posts');
DROP TABLE _autumn_seq_posts;
PRAGMA foreign_key_check;
PRAGMA foreign_keys=ON;
";
        assert_eq!(down, expected, "SQLite down rebuild golden:\n{down}");
    }

    #[test]
    fn sqlite_down_rebuild_preserves_parser_invisible_column() {
        // A rebuild triggered by an UNRELATED change (`views` Int32 -> Int64) must,
        // on the DOWN leg, copy back a baseline column the partial parser cannot see
        // (here `status`, absent from `ctx.desired`). The post-migration table (what
        // is actually in the DB at rollback time) is `baseline + deltas`, which still
        // carries `status`, so the down-leg INSERT..SELECT source must be that shape —
        // not `ctx.desired`, whose partial view would omit `status` and lose its data.
        let mut status_baseline = col("status", ColumnType::Text);
        status_baseline.default = Some(ColumnDefault::Sql("'draft'".to_owned()));
        let baseline = sqlite_table(
            "posts",
            vec![
                col("views", ColumnType::Int32),
                col("body", ColumnType::Text),
                status_baseline,
            ],
            vec![],
        );
        // The parser's blind spot: `status` is absent from the desired view entirely.
        let desired = sqlite_table(
            "posts",
            vec![
                col("views", ColumnType::Int64),
                col("body", ColumnType::Text),
            ],
            vec![],
        );
        let plan = MigrationPlan {
            backend: Backend::Sqlite,
            changes: vec![SchemaChange::AlterColumnType {
                table: "posts".to_owned(),
                column: "views".to_owned(),
                from: ColumnType::Int32,
                to: ColumnType::Int64,
            }],
        };
        let ctx = SchemaContext::from_tables(
            std::slice::from_ref(&desired),
            std::slice::from_ref(&baseline),
        );

        let down = emit_down_sql_with_context(&plan, &ctx).expect("emit down");
        assert!(
            down.contains(
                "INSERT INTO posts__autumn_new (id, views, body, status)\n    \
                 SELECT id, views, body, status FROM posts;"
            ),
            "the down rollback copies back the parser-invisible retained column: {down}"
        );

        // Pin the up leg too: it copies from the baseline (which carries `status`) into
        // the baseline+deltas shape (which also carries it), so `status` round-trips.
        let up = emit_up_sql_with_context(&plan, &ctx).expect("emit up");
        assert!(
            up.contains(
                "INSERT INTO posts__autumn_new (id, views, body, status)\n    \
                 SELECT id, views, body, status FROM posts;"
            ),
            "the up rebuild also carries the parser-invisible retained column: {up}"
        );
    }

    #[test]
    fn postgres_alter_type_still_emits_plain_alter_column() {
        // The same logical change on Postgres emits the direct ALTER, never a rebuild.
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AlterColumnType {
                table: "posts".to_owned(),
                column: "views".to_owned(),
                from: ColumnType::Int32,
                to: ColumnType::Int64,
            }],
        };
        let up = emit_up_sql(&plan).expect("emit up");
        assert_eq!(
            up, "ALTER TABLE posts ALTER COLUMN views TYPE BIGINT;\n",
            "{up}"
        );
        assert!(!up.contains("__autumn_new"), "no rebuild on Postgres: {up}");
    }

    #[test]
    fn sqlite_rebuild_coalesces_alter_and_index_into_one_block() {
        // An AlterColumnType and an AddIndex on the same table produce exactly ONE
        // rebuild block; the index is recreated inside it (after the rename), never
        // as a separate top-level CREATE INDEX.
        let (plan, ctx) = sqlite_rebuild_fixture();
        let up = emit_up_sql_with_context(&plan, &ctx).expect("emit up");
        assert_eq!(
            up.matches("CREATE TABLE posts__autumn_new").count(),
            1,
            "exactly one rebuild block: {up}"
        );
        assert_eq!(
            up.matches("CREATE INDEX idx_posts_body").count(),
            1,
            "the index is created exactly once: {up}"
        );
        let rename = up.find("RENAME TO posts").expect("rename present");
        let index = up
            .find("CREATE INDEX idx_posts_body ON posts (body);")
            .expect("index recreated");
        assert!(
            index > rename,
            "the index is recreated as part of the rebuild, after the rename: {up}"
        );
    }

    #[test]
    fn sqlite_rebuild_copies_only_surviving_columns() {
        // A rebuild that also drops a column copies only the columns common to both
        // shapes — the dropped `legacy` column is never named in INSERT..SELECT.
        let mut legacy = col("legacy", ColumnType::Text);
        legacy.nullable = true;
        let baseline = sqlite_table(
            "posts",
            vec![col("views", ColumnType::Int32), legacy.clone()],
            vec![],
        );
        let desired = sqlite_table("posts", vec![col("views", ColumnType::Int64)], vec![]);
        let plan = MigrationPlan {
            backend: Backend::Sqlite,
            changes: vec![
                SchemaChange::AlterColumnType {
                    table: "posts".to_owned(),
                    column: "views".to_owned(),
                    from: ColumnType::Int32,
                    to: ColumnType::Int64,
                },
                SchemaChange::DropColumn {
                    table: "posts".to_owned(),
                    column: legacy,
                },
            ],
        };
        let ctx = SchemaContext::from_tables(
            std::slice::from_ref(&desired),
            std::slice::from_ref(&baseline),
        );
        let up = emit_up_sql_with_context(&plan, &ctx).expect("emit up");
        assert!(
            up.contains(
                "INSERT INTO posts__autumn_new (id, views)\n    SELECT id, views FROM posts;"
            ),
            "copies only the surviving columns: {up}"
        );
        // The dropped column is absent from the recreated table shape (and therefore
        // never copied). Match `legacy TEXT` (the column definition) specifically rather
        // than the bare substring `legacy`, which now also appears in the unrelated
        // `PRAGMA legacy_alter_table` rename guard.
        assert!(
            !up.contains("legacy TEXT"),
            "dropped column is absent from the recreated shape: {up}"
        );
    }

    #[test]
    fn sqlite_rebuild_preserves_parser_invisible_baseline_default() {
        // A rebuild triggered by an UNRELATED change (here `views` Int32 -> Int64)
        // must not silently drop a baseline facet the partial parser cannot see. The
        // baseline `status` column carries a non-convention default (`'draft'`) and
        // the table a hand-written CHECK; the desired (parser) view records neither
        // and emits NO SetDefault/DropDefault/DropCheck (the engine has no such
        // variant). Building the up-leg recreate shape from `baseline + deltas`
        // instead of the raw `desired` table preserves both.
        let mut status_baseline = col("status", ColumnType::Text);
        status_baseline.default = Some(ColumnDefault::Sql("'draft'".to_owned()));
        let mut baseline = sqlite_table(
            "posts",
            vec![col("views", ColumnType::Int32), status_baseline],
            vec![],
        );
        baseline.checks.push(CheckConstraint {
            name: None,
            expression: "length(status) > 0".to_owned(),
        });

        // The parser's blind spot: `status.default` recorded as None and the CHECK
        // absent, plus the unrelated `views` widening that triggers the rebuild.
        let desired = sqlite_table(
            "posts",
            vec![
                col("views", ColumnType::Int64),
                col("status", ColumnType::Text),
            ],
            vec![],
        );

        let plan = MigrationPlan {
            backend: Backend::Sqlite,
            changes: vec![SchemaChange::AlterColumnType {
                table: "posts".to_owned(),
                column: "views".to_owned(),
                from: ColumnType::Int32,
                to: ColumnType::Int64,
            }],
        };
        let ctx = SchemaContext::from_tables(
            std::slice::from_ref(&desired),
            std::slice::from_ref(&baseline),
        );

        let up = emit_up_sql_with_context(&plan, &ctx).expect("emit up");
        assert!(
            up.contains("status TEXT NOT NULL DEFAULT 'draft'"),
            "the parser-invisible baseline default is preserved in the recreate: {up}"
        );
        assert!(
            up.contains("CHECK (length(status) > 0)"),
            "the parser-invisible baseline CHECK is preserved in the recreate: {up}"
        );
    }

    #[test]
    fn sqlite_rebuild_without_context_is_a_clear_error() {
        // The context-free entry cannot build a rebuild; it returns a directed error
        // rather than a broken/partial rebuild.
        let (plan, _ctx) = sqlite_rebuild_fixture();
        let err = emit_up_sql(&plan).unwrap_err();
        assert!(
            matches!(err, EmitError::SqliteRebuildUnsupported { .. }),
            "missing-context rebuild is a clear error: {err:?}"
        );
        assert!(
            err.to_string().contains("emit_up_sql_with_context"),
            "the error directs the caller to the context-aware entry: {err}"
        );
    }

    #[test]
    fn sqlite_rebuild_refuses_on_staging_name_collision() {
        // A real table already named `posts__autumn_new` would be clobbered by the
        // rebuild's staging CREATE/RENAME, so the emitter refuses rather than emit
        // unappliable SQL.
        let (plan, ctx) = sqlite_rebuild_fixture();
        let collider = sqlite_table(
            "posts__autumn_new",
            vec![col("body", ColumnType::Text)],
            vec![],
        );
        let mut ctx = ctx;
        ctx.desired.insert(collider.name.clone(), collider.clone());
        ctx.baseline.insert(collider.name.clone(), collider);

        let err = emit_up_sql_with_context(&plan, &ctx).unwrap_err();
        assert!(
            matches!(err, EmitError::SqliteRebuildUnsupported { .. }),
            "staging-name collision is refused: {err:?}"
        );
        assert!(
            err.to_string().contains("posts__autumn_new") && err.to_string().contains("collides"),
            "the error names the colliding staging table: {err}"
        );
    }

    #[test]
    fn sqlite_rebuild_preserves_rows_on_real_sqlite() {
        // Exercise the generated UP rebuild against a real in-memory SQLite DB
        // (diesel's `sqlite` backend is a workspace dependency), asserting the rows
        // survive with the new shape and the integrity/foreign-key checks are clean.
        use diesel::connection::SimpleConnection as _;
        use diesel::prelude::*;

        #[derive(QueryableByName)]
        struct PostRow {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            id: i64,
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            views: i64,
            #[diesel(sql_type = diesel::sql_types::Text)]
            body: String,
        }

        // `PRAGMA foreign_key_check` REPORTS violation rows (child table / rowid /
        // parent / fkid); an empty result set means no violations.
        #[derive(QueryableByName)]
        struct FkViolation {
            #[diesel(sql_type = diesel::sql_types::Text, column_name = "table")]
            _child_table: String,
        }

        // `PRAGMA integrity_check` returns a single `TEXT` row; a clean DB reports `ok`.
        #[derive(QueryableByName)]
        struct IntegrityRow {
            #[diesel(sql_type = diesel::sql_types::Text, column_name = "integrity_check")]
            status: String,
        }

        let mut conn = SqliteConnection::establish(":memory:").expect("open in-memory sqlite");
        conn.batch_execute(
            "CREATE TABLE posts (\n    id INTEGER PRIMARY KEY AUTOINCREMENT,\n    \
             views INTEGER NOT NULL,\n    body TEXT NOT NULL\n);\n\
             INSERT INTO posts (views, body) VALUES (3, 'hello'), (7, 'world');",
        )
        .expect("seed the original table");

        let (plan, ctx) = sqlite_rebuild_fixture();
        let up = emit_up_sql_with_context(&plan, &ctx).expect("emit up");
        conn.batch_execute(&up)
            .expect("apply the generated rebuild");

        let rows: Vec<PostRow> = diesel::sql_query("SELECT id, views, body FROM posts ORDER BY id")
            .load(&mut conn)
            .expect("query the rebuilt table");
        assert_eq!(rows.len(), 2, "both rows survived the rebuild");
        assert_eq!(
            (rows[0].id, rows[0].views, rows[0].body.as_str()),
            (1, 3, "hello")
        );
        assert_eq!(
            (rows[1].id, rows[1].views, rows[1].body.as_str()),
            (2, 7, "world")
        );

        // `PRAGMA foreign_key_check` only REPORTS violation rows — it does not raise —
        // so actually READ its output and assert it is empty, rather than firing it via
        // `batch_execute`, which discards the rows and would pass even on a violation.
        let violations: Vec<FkViolation> = diesel::sql_query("PRAGMA foreign_key_check")
            .load(&mut conn)
            .expect("run PRAGMA foreign_key_check");
        assert!(
            violations.is_empty(),
            "foreign_key_check reported {} violation row(s)",
            violations.len()
        );

        let integrity: Vec<IntegrityRow> = diesel::sql_query("PRAGMA integrity_check")
            .load(&mut conn)
            .expect("run PRAGMA integrity_check");
        assert_eq!(integrity.len(), 1, "integrity_check returns one row");
        assert_eq!(integrity[0].status, "ok", "integrity_check is clean");
    }

    #[test]
    fn sqlite_rebuild_survives_dependent_view() {
        // A SQLite view referencing the rebuilt table makes the post-`DROP TABLE`
        // `ALTER TABLE ... RENAME` fail ("error in view v: no such table: main.posts")
        // under the modern default `legacy_alter_table=OFF`. The offline emitter cannot
        // see or recreate views, so the rebuild wraps the rename with
        // `PRAGMA legacy_alter_table=ON`…`OFF`, a blind "dumb" rename that does not
        // descend into / validate views — so the migration applies and the view
        // re-validates against the recreated table. This exercises the REAL in-transaction
        // path (diesel wraps each migration in one) to confirm the pragma is not a
        // mid-transaction no-op like `foreign_keys`.
        use diesel::connection::SimpleConnection as _;
        use diesel::prelude::*;

        #[derive(QueryableByName)]
        struct ViewRow {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            id: i64,
            #[diesel(sql_type = diesel::sql_types::Text)]
            body: String,
        }

        let mut conn = SqliteConnection::establish(":memory:").expect("open in-memory sqlite");
        conn.batch_execute(
            "CREATE TABLE posts (\n    id INTEGER PRIMARY KEY AUTOINCREMENT,\n    \
             views INTEGER NOT NULL,\n    body TEXT NOT NULL\n);\n\
             INSERT INTO posts (views, body) VALUES (3, 'hello');\n\
             CREATE VIEW posts_v AS SELECT id, body FROM posts;",
        )
        .expect("seed the original table and a dependent view");

        let (plan, ctx) = sqlite_rebuild_fixture();
        let up = emit_up_sql_with_context(&plan, &ctx).expect("emit up");

        // Run the generated rebuild INSIDE an explicit transaction — the real path diesel
        // uses — so a mid-transaction pragma no-op would surface here as a rename error.
        conn.transaction::<_, diesel::result::Error, _>(|conn| conn.batch_execute(&up))
            .expect("the rebuild applies inside a transaction with a dependent view present");

        // The view is still valid against the recreated table and the row survived.
        let rows: Vec<ViewRow> = diesel::sql_query("SELECT id, body FROM posts_v ORDER BY id")
            .load(&mut conn)
            .expect("the dependent view is still queryable after the rebuild");
        assert_eq!(rows.len(), 1, "the view still resolves one row");
        assert_eq!((rows[0].id, rows[0].body.as_str()), (1, "hello"));
    }

    #[test]
    fn sqlite_rebuild_uuid_pk_emits_no_sequence_preservation() {
        // A table whose single PK is `Uuid` has no `sqlite_sequence` row, so the
        // rebuild must emit none of the high-water preservation statements — those
        // apply only to `BigSerial` (`INTEGER PRIMARY KEY AUTOINCREMENT`) PKs.
        let uuid_table = |name: &str| -> Table {
            let mut t = Table::new(name, Backend::Sqlite);
            let mut id = col("id", ColumnType::Uuid);
            id.primary_key = true;
            t.primary_key.push("id".to_owned());
            t.columns.push(id);
            t.columns.push(col("body", ColumnType::Text));
            t
        };
        let baseline = uuid_table("posts");
        let desired = uuid_table("posts");
        assert!(
            matches!(single_pk_column(&desired), Some((_, IdKind::Uuid))),
            "the fixture PK resolves to Uuid"
        );
        for leg in [RebuildLeg::Up, RebuildLeg::Down] {
            let sql = render_sqlite_rebuild("posts", &desired, &baseline, leg);
            assert!(
                !sql.contains("sqlite_sequence"),
                "no sqlite_sequence statements for a Uuid PK ({leg:?}): {sql}"
            );
            assert!(
                !sql.contains("_autumn_seq_"),
                "no high-water temp table for a Uuid PK ({leg:?}): {sql}"
            );
        }
    }

    #[test]
    fn sqlite_rebuild_preserves_autoincrement_sequence() {
        // The definitive non-reuse proof: after a table recreate, an id that was
        // issued-then-deleted must NOT be handed out again. Seed ids 1..5, delete
        // 2..5 (the `sqlite_sequence` high-water stays 5), apply the generated
        // rebuild, then insert a new row and assert its id is 6 — not the reused 2.
        use diesel::connection::SimpleConnection as _;
        use diesel::prelude::*;

        #[derive(QueryableByName)]
        struct BigRow {
            #[diesel(sql_type = diesel::sql_types::BigInt, column_name = "v")]
            v: i64,
        }

        // `PRAGMA integrity_check` returns a single `TEXT` row; a clean DB reports `ok`.
        #[derive(QueryableByName)]
        struct IntegrityRow {
            #[diesel(sql_type = diesel::sql_types::Text, column_name = "integrity_check")]
            status: String,
        }

        let mut conn = SqliteConnection::establish(":memory:").expect("open in-memory sqlite");
        conn.batch_execute(
            "CREATE TABLE posts (\n    id INTEGER PRIMARY KEY AUTOINCREMENT,\n    \
             views INTEGER NOT NULL,\n    body TEXT NOT NULL\n);\n\
             INSERT INTO posts (views, body) VALUES \
             (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e');",
        )
        .expect("seed five rows -> ids 1..5");

        // The AUTOINCREMENT high-water is 5 after five inserts.
        let seq: Vec<BigRow> =
            diesel::sql_query("SELECT seq AS v FROM sqlite_sequence WHERE name = 'posts'")
                .load(&mut conn)
                .expect("read sqlite_sequence");
        assert_eq!(seq.len(), 1, "sqlite_sequence has a row for posts");
        assert_eq!(seq[0].v, 5, "high-water is 5 before the delete");

        // Delete every row but id=1; the high-water in sqlite_sequence stays 5.
        conn.batch_execute("DELETE FROM posts WHERE id > 1;")
            .expect("delete ids 2..5");

        let (plan, ctx) = sqlite_rebuild_fixture();
        let up = emit_up_sql_with_context(&plan, &ctx).expect("emit up");
        conn.batch_execute(&up)
            .expect("apply the generated rebuild");

        // A fresh insert must NOT reuse a deleted id: the next id is 6, not 2.
        conn.batch_execute("INSERT INTO posts (views, body) VALUES (9, 'new');")
            .expect("insert a new row after the rebuild");
        let max: Vec<BigRow> = diesel::sql_query("SELECT MAX(id) AS v FROM posts")
            .load(&mut conn)
            .expect("max id after rebuild");
        assert_eq!(
            max[0].v, 6,
            "the next id after the rebuild does not reuse a deleted id"
        );

        // The surviving id=1 row is untouched by the rebuild.
        let survivors: Vec<BigRow> =
            diesel::sql_query("SELECT COUNT(*) AS v FROM posts WHERE id = 1")
                .load(&mut conn)
                .expect("count id=1");
        assert_eq!(survivors[0].v, 1, "the surviving id=1 row is intact");

        let integrity: Vec<IntegrityRow> = diesel::sql_query("PRAGMA integrity_check")
            .load(&mut conn)
            .expect("run PRAGMA integrity_check");
        assert_eq!(integrity.len(), 1, "integrity_check returns one row");
        assert_eq!(integrity[0].status, "ok", "integrity_check is clean");
    }

    #[test]
    fn sqlite_create_table_and_add_column_render() {
        // CreateTable + a nullable AddColumn are within the portable `SQLite` subset.
        let mut t = Table::new("posts", Backend::Sqlite);
        let mut id = col("id", ColumnType::Int64);
        id.primary_key = true;
        t.primary_key.push("id".to_owned());
        t.columns.push(id);
        t.columns.push(col("body", ColumnType::Text));
        let mut bio = col("bio", ColumnType::Text);
        bio.nullable = true;
        let plan = MigrationPlan {
            backend: Backend::Sqlite,
            changes: vec![
                SchemaChange::CreateTable(t),
                SchemaChange::AddColumn {
                    table: "posts".to_owned(),
                    column: bio,
                },
            ],
        };
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"),
            "sqlite PK: {up}"
        );
        assert!(up.contains("ADD COLUMN bio TEXT NULL"), "{up}");
    }

    #[test]
    fn sqlite_add_not_null_without_default_is_unsupported() {
        let plan = MigrationPlan {
            backend: Backend::Sqlite,
            changes: vec![SchemaChange::AddColumn {
                table: "posts".to_owned(),
                column: col("title", ColumnType::Text),
            }],
        };
        assert!(matches!(
            emit_up_sql(&plan).unwrap_err(),
            EmitError::UnsupportedOnBackend { .. }
        ));
    }

    #[test]
    fn down_drops_fk_column_before_dropping_referenced_table() {
        // Deferred item #1: baseline `posts(id)` only; desired adds a `target` table
        // plus `posts.target_id` (nullable FK → target.id). The down leg must DROP
        // COLUMN target_id BEFORE DROP TABLE target, else the rollback is unappliable.
        // Pins `emit_down_sql` as a pure reverse of `up_ordered` for the create-table
        // + add-FK-column pattern.
        let baseline = vec![posts_with(vec![])];

        let mut target_id = col("target_id", ColumnType::Int64);
        target_id.nullable = true;
        target_id.references = Some(ForeignKey::new("target", "id"));
        let desired_posts = posts_with(vec![target_id]);

        let mut target = Table::new("target", Backend::Postgres);
        let mut tid = col("id", ColumnType::Int64);
        tid.primary_key = true;
        target.primary_key.push("id".to_owned());
        target.columns.push(tid);

        let desired = parsed(vec![desired_posts, target], vec![]);
        let plan = diff_schema(&baseline, &desired, DEFAULT_OPTS);
        guard_plan(&plan, DEFAULT_OPTS).expect("emittable");
        let down = emit_down_sql(&plan).expect("emit down");

        let drop_col = down
            .find("ALTER TABLE posts DROP COLUMN target_id")
            .expect("down drops the FK column");
        let drop_table = down
            .find("DROP TABLE target")
            .expect("down drops the referenced table");
        assert!(
            drop_col < drop_table,
            "DROP COLUMN target_id must precede DROP TABLE target:\n{down}"
        );
    }

    #[test]
    fn over_long_fkey_constraint_name_is_bounded_not_refused() {
        // Deferred item #4: an engine-generated FK-constraint name over 63 bytes is
        // truncate+hashed to a valid ≤63-byte identifier — the SAME name in the up
        // ADD and the down DROP — and the guard no longer refuses it.
        let table = "a".repeat(40);
        let column = "b".repeat(40);
        let raw = format!("{table}_{column}_fkey");
        assert!(
            raw.len() > PG_MAX_IDENTIFIER_BYTES,
            "the raw name is over the limit"
        );

        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddForeignKey {
                table,
                column,
                foreign_key: ForeignKey::new("targets", "id"),
            }],
        };
        guard_plan(&plan, DEFAULT_OPTS)
            .expect("the length guard no longer refuses a bounded FK name");

        let bounded = bounded_pg_identifier(&raw);
        assert!(
            bounded.len() <= PG_MAX_IDENTIFIER_BYTES,
            "the bounded name fits the limit: {bounded}"
        );
        assert_ne!(bounded, raw, "the over-long name is actually shortened");

        let up = emit_up_sql(&plan).expect("emit up");
        let down = emit_down_sql(&plan).expect("emit down");
        assert!(
            up.contains(&format!("ADD CONSTRAINT {bounded} ")),
            "up uses the bounded name: {up}"
        );
        assert!(
            down.contains(&format!("DROP CONSTRAINT {bounded};")),
            "down uses the SAME bounded name: {down}"
        );
    }

    #[test]
    fn short_fkey_constraint_name_is_unchanged() {
        // A normal (short) FK-constraint name passes through unchanged, so Postgres
        // output stays byte-stable.
        assert_eq!(
            bounded_pg_identifier("posts_author_id_fkey"),
            "posts_author_id_fkey"
        );
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::AddForeignKey {
                table: "posts".to_owned(),
                column: "author_id".to_owned(),
                foreign_key: ForeignKey::new("users", "id"),
            }],
        };
        let up = emit_up_sql(&plan).expect("emit up");
        assert!(
            up.contains(
                "ADD CONSTRAINT posts_author_id_fkey FOREIGN KEY (author_id) REFERENCES users(id);"
            ),
            "{up}"
        );
        let down = emit_down_sql(&plan).expect("emit down");
        assert!(
            down.contains("DROP CONSTRAINT posts_author_id_fkey;"),
            "{down}"
        );
    }

    #[test]
    fn describe_plan_lists_each_change() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![
                SchemaChange::CreateTable(posts_with(vec![])),
                SchemaChange::AddColumn {
                    table: "posts".to_owned(),
                    column: col("body", ColumnType::Text),
                },
            ],
        };
        let text = describe_plan(&plan);
        assert!(text.contains("postgres"), "names the backend: {text}");
        assert!(text.contains("2 change(s)"), "counts changes: {text}");
        assert!(text.contains("CREATE TABLE posts"), "{text}");
        assert!(text.contains("ADD COLUMN posts.body"), "{text}");
    }

    // -- 13.6 Codex round-2 regressions -------------------------------------

    /// Finding A: dropping a table while a RETAINED table still holds a baseline
    /// FK to it emits an unappliable `DROP TABLE`. Must be refused — even under
    /// `--allow-destructive` (that flag permits losing the dropped table's own
    /// data, not breaking another table's referential integrity).
    #[test]
    fn drop_table_with_retained_inbound_fk_is_refused_even_with_allow_destructive() {
        // baseline: users (dropped) + posts (retained) with posts.user_id → users.
        let users = Table::new("users", Backend::Postgres);
        let mut user_fk = col("user_id", ColumnType::Int64);
        user_fk.references = Some(ForeignKey::new("users", "id"));
        let posts = posts_ref_table("posts", user_fk);
        let baseline = vec![users, posts.clone()];
        // desired keeps posts unchanged; users is gone.
        let plan = diff_schema(&baseline, &parsed(vec![posts], vec![]), ALLOW);
        let err = guard_plan(&plan, ALLOW)
            .expect_err("retained posts.user_id → users blocks DROP TABLE users");
        let msg = err.to_string();
        assert!(
            msg.contains("users") && msg.contains("posts.user_id"),
            "names the dropped table and the retained referencer: {msg}"
        );
    }

    /// Finding 667 (documented limitation): the slice-2 parser cannot see a
    /// generated `#[belongs_to(...)]` association FK — a `<name>_id: i64` column is
    /// lifted as a plain `Int64` with `references: None` and NO diagnostic — so an
    /// invisible inbound association FK on a *retained* table cannot be detected
    /// offline, and this engine emits an unchecked `DROP TABLE` for it. A precise
    /// guard is impossible without schema introspection (a future slice); a blanket
    /// refuse-all-drops would make the common case useless. Instead every emitted
    /// `DROP TABLE` carries an advisory `-- autumn-safety:` comment naming this gap.
    /// This test pins that advisory (no behavioral test is possible for the
    /// invisible case — by definition the engine has no signal for it). The
    /// *visible*-FK protection is unaffected — see
    /// `drop_table_with_retained_inbound_fk_is_refused_even_with_allow_destructive`
    /// above, which still refuses a drop blocked by an observable `references`.
    #[test]
    fn drop_table_emits_invisible_association_fk_advisory() {
        let plan = MigrationPlan {
            backend: Backend::Postgres,
            changes: vec![SchemaChange::DropTable(posts_with(vec![]))],
        };
        guard_plan(&plan, ALLOW).expect("a plain DROP TABLE (no visible inbound FK) is allowed");
        let up = emit_up_sql(&plan).expect("emit up");
        assert!(
            up.contains("-- autumn-safety:")
                && up.contains("#[belongs_to(...)]")
                && up.contains("the offline snapshot cannot see"),
            "the DROP TABLE carries the invisible-association-FK advisory: {up}"
        );
        // The advisory precedes the statement, and the real statement is still emitted.
        let advisory = up.find("-- autumn-safety:").expect("advisory present");
        let stmt = up.find("DROP TABLE posts;").expect("statement present");
        assert!(
            advisory < stmt,
            "advisory comment precedes the DROP TABLE: {up}"
        );
    }

    /// Finding A: a self-referential FK on the dropped table does NOT block its
    /// own drop, and neither does an FK from another table that is also dropped.
    #[test]
    fn drop_table_self_ref_and_co_dropped_ref_do_not_block() {
        // users has a self-FK (parent_id → users) and is dropped alone.
        let mut parent = col("parent_id", ColumnType::Int64);
        parent.references = Some(ForeignKey::new("users", "id"));
        let users = posts_ref_table("users", parent);
        let plan = diff_schema(&[users], &parsed(vec![], vec![]), ALLOW);
        guard_plan(&plan, ALLOW).expect("self-referential FK goes away with the table");
    }

    /// Round 3 / Finding Y: a DESIRED-side FK to a table being dropped in the
    /// same plan is refused. Drop `users` while adding `posts.author_id ->
    /// users`; the emitted `CREATE TABLE posts ... REFERENCES users` would depend
    /// on the dropped `users`, so PG rejects. The round-2 baseline-only guard
    /// missed this — only desired-side references catch it.
    #[test]
    fn desired_side_fk_to_dropped_table_is_refused() {
        // baseline: users (managed, no inbound FK). desired: users gone, new
        // posts.author_id -> users.
        let users = Table::new("users", Backend::Postgres);
        let mut author_fk = col("author_id", ColumnType::Int64);
        author_fk.references = Some(ForeignKey::new("users", "id"));
        let posts = posts_ref_table("posts", author_fk);
        let plan = diff_schema(&[users], &parsed(vec![posts], vec![]), ALLOW);
        let err = guard_plan(&plan, ALLOW)
            .expect_err("new posts.author_id -> users blocks DROP TABLE users");
        let msg = err.to_string();
        assert!(
            msg.contains("users") && msg.contains("posts.author_id"),
            "names the dropped table and the desired-side referencer: {msg}"
        );
    }

    /// Round 3 / Finding Y control: a desired-side FK pointing at a RETAINED
    /// table is NOT a blocker even while an unrelated table is dropped — only
    /// references to a *dropped* table are refused (no over-refusal).
    #[test]
    fn desired_side_fk_to_retained_table_is_allowed() {
        // baseline: users (retained) + stale (dropped). desired: users retained,
        // stale gone, new posts.author_id -> users (a retained table).
        let users = Table::new("users", Backend::Postgres);
        let stale = Table::new("stale", Backend::Postgres);
        let mut author_fk = col("author_id", ColumnType::Int64);
        author_fk.references = Some(ForeignKey::new("users", "id"));
        let posts = posts_ref_table("posts", author_fk);
        let plan = diff_schema(
            &[users.clone(), stale],
            &parsed(vec![users, posts], vec![]),
            ALLOW,
        );
        guard_plan(&plan, ALLOW)
            .expect("FK to a retained table is fine even while another table is dropped");
    }

    /// Finding B: a non-implicit type change (`TEXT` → `INTEGER`) has no implicit
    /// PG cast, so a bare `ALTER COLUMN ... TYPE` is unappliable. Must be refused
    /// (needs a manual `USING`), regardless of `--allow-destructive`.
    #[test]
    fn non_implicit_type_conversion_is_refused() {
        let base = vec![posts_with(vec![col("views", ColumnType::Text)])];
        let want = parsed(
            vec![posts_with(vec![col("views", ColumnType::Int32)])],
            vec![],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        let err = guard_plan(&plan, ALLOW).expect_err("TEXT→INTEGER needs a USING clause");
        let msg = err.to_string();
        assert!(
            msg.contains("USING") && msg.contains("views"),
            "explains the USING requirement and names the column: {msg}"
        );
    }

    /// Finding B: an implicit widening (`Int32` → `Int64`, i.e. int4 → int8) IS
    /// emittable as a bare `ALTER COLUMN ... TYPE` — the classifier must not
    /// over-refuse.
    #[test]
    fn implicit_widening_type_conversion_is_allowed() {
        let base = vec![posts_with(vec![col("views", ColumnType::Int32)])];
        let want = parsed(
            vec![posts_with(vec![col("views", ColumnType::Int64)])],
            vec![],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        guard_plan(&plan, DEFAULT_OPTS).expect("Int32→Int64 is an implicit widening");
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("ALTER TABLE posts ALTER COLUMN views TYPE BIGINT;"),
            "emits the bare ALTER TYPE for the widening: {up}"
        );
    }

    /// Finding S: a type change on a **referencing** FK column
    /// (`posts.account_id` → `accounts.id`, widened `Int32` → `Int64`) is an
    /// implicit widening the round-2 classifier ALLOWS, but PG rejects
    /// `ALTER COLUMN ... TYPE` on a column bound by an FK. Must be refused (no
    /// override) — the bare ALTER is unappliable.
    #[test]
    fn type_change_on_referencing_fk_column_is_refused() {
        let accounts = posts_ref_table("accounts", col("name", ColumnType::Text));
        let mut fk_base = col("account_id", ColumnType::Int32);
        fk_base.references = Some(ForeignKey::new("accounts", "id"));
        let base = vec![accounts.clone(), posts_with(vec![fk_base])];
        let mut fk_want = col("account_id", ColumnType::Int64);
        fk_want.references = Some(ForeignKey::new("accounts", "id"));
        let want = parsed(vec![accounts, posts_with(vec![fk_want])], vec![]);
        let plan = diff_schema(&base, &want, ALLOW);
        let err = guard_plan(&plan, ALLOW)
            .expect_err("type change on a referencing FK column is unappliable");
        let msg = err.to_string();
        assert!(
            msg.contains("posts.account_id") && msg.contains("foreign-key"),
            "names the FK column and the FK reason: {msg}"
        );
    }

    /// Finding S: a type change on a **referenced key** column (`accounts.id`
    /// widened `Int32` → `Int64`) while a retained `posts.account_id` → `accounts`
    /// FK survives is likewise refused — PG rejects altering the referenced key
    /// while the inbound FK exists, and this engine cannot drop+recreate it.
    #[test]
    fn type_change_on_referenced_key_column_is_refused() {
        // accounts.id: Int32 → Int64 (PK unchanged, so no PrimaryKeyChange).
        let mut acc_id_base = col("id", ColumnType::Int32);
        acc_id_base.primary_key = true;
        let mut accounts_base = Table::new("accounts", Backend::Postgres);
        accounts_base.primary_key.push("id".to_owned());
        accounts_base.columns.push(acc_id_base);

        let mut acc_id_want = col("id", ColumnType::Int64);
        acc_id_want.primary_key = true;
        let mut accounts_want = Table::new("accounts", Backend::Postgres);
        accounts_want.primary_key.push("id".to_owned());
        accounts_want.columns.push(acc_id_want);

        let mut fk = col("account_id", ColumnType::Int64);
        fk.references = Some(ForeignKey::new("accounts", "id"));
        let posts = posts_ref_table("posts", fk);

        let base = vec![accounts_base, posts.clone()];
        let want = parsed(vec![accounts_want, posts], vec![]);
        let plan = diff_schema(&base, &want, ALLOW);
        let err = guard_plan(&plan, ALLOW)
            .expect_err("type change on a referenced key column is unappliable");
        let msg = err.to_string();
        assert!(
            msg.contains("accounts.id") && msg.contains("foreign-key"),
            "names the referenced key column and the FK reason: {msg}"
        );
    }

    /// Finding S control: an implicit widening on a **plain** (non-FK) column
    /// still emits the bare `ALTER COLUMN ... TYPE` — the FK guard must not
    /// over-refuse.
    #[test]
    fn type_change_on_plain_column_still_allowed() {
        let base = vec![posts_with(vec![col("views", ColumnType::Int32)])];
        let want = parsed(
            vec![posts_with(vec![col("views", ColumnType::Int64)])],
            vec![],
        );
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        guard_plan(&plan, DEFAULT_OPTS).expect("plain-column widening is not FK-bound");
        let up = emit_up_sql(&plan).expect("emit");
        assert!(
            up.contains("ALTER TABLE posts ALTER COLUMN views TYPE BIGINT;"),
            "emits the bare ALTER TYPE for the plain-column widening: {up}"
        );
    }

    /// Finding R: dropping a retained table's FK column AND its referenced table in
    /// the SAME plan is a valid combined cleanup — `up_ordered` emits every
    /// `DROP COLUMN` (which removes the FK constraint) before every `DROP TABLE`,
    /// so `DROP COLUMN posts.user_id` runs before `DROP TABLE users`. The
    /// inbound-FK guard must NOT over-refuse it under `--allow-destructive`.
    #[test]
    fn drop_fk_column_and_target_table_together_is_allowed_with_allow_destructive() {
        let users = Table::new("users", Backend::Postgres);
        let mut user_fk = col("user_id", ColumnType::Int64);
        user_fk.references = Some(ForeignKey::new("users", "id"));
        let baseline = vec![users, posts_with(vec![user_fk])];
        // desired: users gone, posts.user_id dropped.
        let desired = parsed(vec![posts_with(vec![])], vec![]);
        let plan = diff_schema(&baseline, &desired, ALLOW);
        guard_plan(&plan, ALLOW)
            .expect("co-dropping the FK column and its target table is valid SQL");
        let up = emit_up_sql(&plan).expect("emit");
        let drop_col = up
            .find("DROP COLUMN user_id")
            .expect("emits DROP COLUMN user_id");
        let drop_tbl = up.find("DROP TABLE users").expect("emits DROP TABLE users");
        assert!(
            drop_col < drop_tbl,
            "DROP COLUMN posts.user_id must precede DROP TABLE users:\n{up}"
        );
    }

    /// Finding C: two managed tables both dropped, `audit_logs.account_id` →
    /// `accounts`. Lexically `accounts` < `audit_logs`, so a naive order would
    /// `DROP TABLE accounts` (referenced) before `audit_logs` (referencing) → PG
    /// rejects. Drops must be REVERSE-topological (referencing first); the down
    /// leg recreates FORWARD (referenced first).
    #[test]
    fn drop_tables_reverse_topologically_ordered_by_fk() {
        let accounts = posts_ref_table("accounts", col("name", ColumnType::Text));
        let mut acct_fk = col("account_id", ColumnType::Int64);
        acct_fk.references = Some(ForeignKey::new("accounts", "id"));
        let audit = posts_ref_table("audit_logs", acct_fk);
        let baseline = vec![accounts, audit];
        let plan = diff_schema(&baseline, &parsed(vec![], vec![]), ALLOW);
        // Both tables are dropped, so nothing retained references either.
        guard_plan(&plan, ALLOW).expect("both dropped — no retained inbound reference");

        let up = emit_up_sql(&plan).expect("emit");
        let referenced = up.find("DROP TABLE accounts").expect("accounts dropped");
        let referencing = up
            .find("DROP TABLE audit_logs")
            .expect("audit_logs dropped");
        assert!(
            referencing < referenced,
            "drop referencing `audit_logs` before referenced `accounts`: {up}"
        );

        let down = emit_down_sql(&plan).expect("emit");
        let d_referenced = down.find("CREATE TABLE accounts").expect("down accounts");
        let d_referencing = down
            .find("CREATE TABLE audit_logs")
            .expect("down audit_logs");
        assert!(
            d_referenced < d_referencing,
            "down recreates referenced `accounts` before referencing `audit_logs`: {down}"
        );
    }

    /// Finding C: a cycle among dropped tables is refused (reuses the existing
    /// `CyclicTableDependencies` error), never emitted as unorderable SQL.
    #[test]
    fn drop_tables_fk_cycle_is_refused() {
        let mut a_b = col("b_id", ColumnType::Int64);
        a_b.references = Some(ForeignKey::new("b", "id"));
        let table_a = posts_ref_table("a", a_b);
        let mut b_a = col("a_id", ColumnType::Int64);
        b_a.references = Some(ForeignKey::new("a", "id"));
        let table_b = posts_ref_table("b", b_a);
        let plan = diff_schema(&[table_a, table_b], &parsed(vec![], vec![]), ALLOW);
        // (The inbound-FK guard excludes co-dropped referencers, so the cycle
        // surfaces at emission, mirroring the CreateTable cycle case.)
        guard_plan(&plan, ALLOW).expect("co-dropped tables never trip the inbound guard");
        let err = emit_up_sql(&plan).unwrap_err();
        let EmitError::CyclicTableDependencies { tables } = &err else {
            panic!("expected CyclicTableDependencies, got {err:?}");
        };
        assert_eq!(
            *tables,
            vec!["a".to_owned(), "b".to_owned()],
            "names the cycle"
        );
    }
}
