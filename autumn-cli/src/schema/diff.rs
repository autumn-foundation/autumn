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
//! # `SQLite` boundary
//!
//! Slice 4 is **pg-first**: it renders Postgres fully and the portable `SQLite`
//! subset (`CREATE TABLE`, `DROP TABLE`, `ADD COLUMN`, `CREATE`/`DROP INDEX`).
//! The `ALTER`-family on `SQLite` (`ALTER COLUMN`, `ADD CONSTRAINT`) needs the
//! 12-step table-rebuild that slice 5 owns, so those return
//! [`EmitError::UnsupportedOnBackend`] here.

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
    /// An explicit foreign key added to a column (added, or newly-explicit).
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
            // Desired only — create it if Autumn owns it.
            (None, Some(want)) => {
                if want.managed {
                    changes.push(SchemaChange::CreateTable((*want).clone()));
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
/// with their table. Markers are deduplicated (a pre-existing FK on a retained
/// table appears on both sides) and, via the `BTreeSet`, deterministically sorted
/// by (dropped, referencing table, referencing column).
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

    diff_indexes(&want.name, base, want, changes);
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

    // Rule B: only emit the *add* direction, and only for a column that did not
    // already have an explicit foreign key. A desired `None` is "unknown,
    // retained" — never a DropForeignKey.
    //
    // The three sub-cases when the desired side has an explicit FK:
    //   * baseline had none            → a genuinely-new FK  → `AddForeignKey`.
    //   * baseline had the same FK     → no change.
    //   * baseline had a *different* FK → a retarget we cannot safely emit
    //     (there is no `DropForeignKey`, and re-`ADD CONSTRAINT`-ing the default
    //     `<table>_<column>_fkey` name would collide) → the refused
    //     `ForeignKeyChange` marker, mirroring `PrimaryKeyChange`.
    if let Some(fk) = &want.references {
        match &base.references {
            None => changes.push(SchemaChange::AddForeignKey {
                table: table.to_owned(),
                column: want.name.clone(),
                foreign_key: fk.clone(),
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
fn diff_indexes(table: &str, base: &Table, want: &Table, changes: &mut Vec<SchemaChange>) {
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
        if !want_by_name.contains_key(base_idx.name.as_str()) {
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

    // 5. Possible rename — a single table that both dropped and added columns.
    //    Overridable, so it is only enforced when destructive changes are not
    //    allowed. Deliberately checked *before* the required-column guard below so
    //    a rename-shaped drop+add reports the more-actionable `#[renamed_from]`
    //    guidance rather than "add a default". Grouped in a BTreeMap so the
    //    reported table is deterministic (sorted).
    if !opts.allow_destructive {
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
        if let Some((table, (dropped, added))) = per_table
            .into_iter()
            .find(|(_, (dropped, added))| !dropped.is_empty() && !added.is_empty())
        {
            return Err(DiffError::PossibleRename {
                table,
                dropped,
                added,
            });
        }
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

/// The [`DiffError::NonImplicitTypeConversion`] refusal for the first
/// non-implicit `AlterColumnType` in `plan`, if any. Postgres-only: on `SQLite`
/// every `AlterColumnType` already returns [`EmitError::UnsupportedOnBackend`] at
/// emit time (slice 5's table-rebuild path owns it), so the classifier stays out
/// of the `SQLite` boundary.
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
// SQL emission (pg-first)
// ---------------------------------------------------------------------------

/// Render the plan's forward SQL (pg-first). One statement group per change, in
/// canonical up-order, groups separated by a blank line.
///
/// # Errors
///
/// Returns [`EmitError::UnsupportedOnBackend`] for an `ALTER`-family change on
/// `SQLite` (slice 5 owns the `SQLite` table-rebuild path).
pub fn emit_up_sql(plan: &MigrationPlan) -> Result<String, EmitError> {
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

/// Render the plan's reverse SQL: the changes reversed and individually inverted,
/// with `-- irreversible:` markers where data cannot round-trip.
///
/// # Errors
///
/// Returns [`EmitError::UnsupportedOnBackend`] for an `ALTER`-family change on
/// `SQLite`.
pub fn emit_down_sql(plan: &MigrationPlan) -> Result<String, EmitError> {
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
        | SchemaChange::DropTableBlockedByInboundFk { .. } => 9,
    }
}

/// Render a single change's forward SQL.
fn emit_change_up(change: &SchemaChange, backend: Backend) -> Result<String, EmitError> {
    match change {
        SchemaChange::CreateTable(table) => Ok(emit_create_table(table, backend)),
        SchemaChange::DropTable(table) => Ok(format!("DROP TABLE {};\n", table.name)),
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
            Ok(format!(
                "ALTER TABLE {table} ADD CONSTRAINT {table}_{column}_fkey \
                 FOREIGN KEY ({column}) REFERENCES {}({});\n",
                foreign_key.table, foreign_key.column
            ))
        }
        // Non-emittable markers: the guard refuses them, so they never reach here
        // in the command flow. Render nothing defensively rather than panicking.
        SchemaChange::PrimaryKeyChange { .. }
        | SchemaChange::ForeignKeyChange { .. }
        | SchemaChange::DropTableBlockedByInboundFk { .. } => Ok(String::new()),
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
        SchemaChange::AddForeignKey { table, column, .. } => Ok(format!(
            "ALTER TABLE {table} DROP CONSTRAINT {table}_{column}_fkey;\n"
        )),
        SchemaChange::PrimaryKeyChange { .. }
        | SchemaChange::ForeignKeyChange { .. }
        | SchemaChange::DropTableBlockedByInboundFk { .. } => Ok(String::new()),
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
            Some(name) => lines.push(format!(
                "    CONSTRAINT {name} CHECK ({})",
                check.expression
            )),
            None => lines.push(format!("    CHECK ({})", check.expression)),
        }
    }

    let mut out = format!("CREATE TABLE {} (\n{}\n);\n", table.name, lines.join(",\n"));

    // Indexes, name-sorted for determinism.
    let mut indexes: Vec<&Index> = table.indexes.iter().collect();
    indexes.sort_by(|a, b| a.name.cmp(&b.name));
    for index in indexes {
        let _ = writeln!(out, "{}", index_sql(&table.name, index));
    }
    out
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
    fn fk_added_when_desired_explicit() {
        let base = vec![posts_with(vec![col("author_id", ColumnType::Int64)])];
        let mut author = col("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "id"));
        let want = parsed(vec![posts_with(vec![author])], vec![]);
        let plan = diff_schema(&base, &want, DEFAULT_OPTS);
        assert_eq!(
            plan.changes,
            vec![SchemaChange::AddForeignKey {
                table: "posts".to_owned(),
                column: "author_id".to_owned(),
                foreign_key: ForeignKey::new("users", "id"),
            }]
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

    #[test]
    fn sqlite_alter_type_is_unsupported_error() {
        let plan = MigrationPlan {
            backend: Backend::Sqlite,
            changes: vec![SchemaChange::AlterColumnType {
                table: "posts".to_owned(),
                column: "views".to_owned(),
                from: ColumnType::Int32,
                to: ColumnType::Int64,
            }],
        };
        let err = emit_up_sql(&plan).unwrap_err();
        assert!(matches!(
            err,
            EmitError::UnsupportedOnBackend {
                backend: Backend::Sqlite,
                ..
            }
        ));
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
