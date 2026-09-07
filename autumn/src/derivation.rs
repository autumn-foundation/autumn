//! Maintained derived read models — `#[derivation]` (issue #1769).
//!
//! A derivation is a denormalised value on a parent row that the framework
//! keeps correct by construction: `posts.published_comment_count`,
//! `posts.visible_score`. Declaring it on the child —
//!
//! ```rust,ignore
//! #[autumn_web::model(table = "comments")]
//! #[belongs_to(Post, fk = post_id)]
//! #[derivation(Post, column = "published_comment_count", filter = published)]
//! #[derivation(Post, column = "visible_score", transform = sum(score), filter = published && score > 0)]
//! pub struct Comment { /* … */ }
//! ```
//!
//! — makes every generated mutation path maintain both columns inside the same
//! transaction as the row mutation.
//!
//! # Why this is the counter cache
//!
//! A derivation is a [counter cache](crate::counter_cache) with two extra
//! pieces: a per-row *contribution* (1 for a count, the field for a sum, 0 for a
//! row the filter rejects) and a *filter* lowered to SQL. `#[model]` emits both
//! into the same [`CounterCacheSpec`](crate::counter_cache::CounterCacheSpec),
//! so the fifteen mutation paths the repository macro already dispatches to keep
//! derivations current with no new dispatch point. A plain counter cache is the
//! unfiltered special case, and its generated SQL is unchanged.
//!
//! # What this module adds
//!
//! The counter cache is maintained from the moment it is declared, because the
//! column and the code ship together. A derivation over an existing table does
//! not have that luxury, so this module owns the part the specs cannot:
//!
//! * **Content addressing.** [`DerivationDef::definition_hash`] hashes the
//!   lowered shape — tables, columns, transform, filter SQL. A changed filter
//!   changes the hash; a rename or a reformat does not.
//! * **Reconciliation.** [`ensure_derivations`] compares each registered
//!   derivation's hash against `_autumn_derivations` and enqueues a backfill for
//!   the ones that changed, leaving the rest alone.
//! * **Resumable repair.** [`run_backfill`] rebuilds parents in checkpointed
//!   batches, committing each batch and its checkpoint together, so a killed
//!   process resumes rather than restarts.
//! * **Observability.** [`derivation_status`] reports each derivation's state
//!   and its drift from the source of truth; `/actuator/derivations` serves it.

use std::collections::HashMap;
use std::fmt::Write as _;

use diesel::sql_types::{BigInt, Nullable, Text};
use diesel_async::RunQueryDsl as _;
use scoped_futures::ScopedFutureExt as _;
use serde::{Deserialize, Serialize};

use crate::counter_cache::SqlView;
use crate::db::{RuntimeConnection, scoped_immediate_transaction};
use crate::{AutumnError, AutumnResult};

/// Narrow framework migration set that creates `_autumn_derivations`.
///
/// Backend-forked exactly like
/// [`VERSION_HISTORY_MIGRATIONS`](crate::version_history::VERSION_HISTORY_MIGRATIONS):
/// the Postgres DDL (`TIMESTAMPTZ`/`NOW()`) is not valid `SQLite`, so the
/// `SQLite` build embeds a parallel set under the same version dir name, keeping
/// `__diesel_schema_migrations` bookkeeping identical across backends.
#[cfg(not(feature = "sqlite"))]
pub const DERIVATION_MIGRATIONS: diesel_migrations::EmbeddedMigrations =
    diesel_migrations::embed_migrations!("derivation_migrations");

/// `SQLite` variant of [`DERIVATION_MIGRATIONS`]. See that item for the
/// backend-fork rationale.
#[cfg(feature = "sqlite")]
pub const DERIVATION_MIGRATIONS: diesel_migrations::EmbeddedMigrations =
    diesel_migrations::embed_migrations!("derivation_migrations_sqlite");

/// The state table. A framework table, hence the `_autumn_` prefix.
const STATE_TABLE: &str = "_autumn_derivations";

/// `CURRENT_TIMESTAMP` rather than `NOW()`: both backends spell it that way, so
/// one statement serves both.
const NOW: &str = "CURRENT_TIMESTAMP";

// Backend-forked placeholder. Postgres numbers its binds, `SQLite` does not;
// binds are pushed in the same order on both, so one template with swapped
// placeholder text serves both.
#[cfg(not(feature = "sqlite"))]
fn ph(n: usize) -> String {
    format!("${n}")
}
#[cfg(feature = "sqlite")]
fn ph(_n: usize) -> String {
    "?".to_owned()
}

/// NULL-safe inequality, as [`crate::counter_cache`] spells it. Only the SQL
/// builder assertions need it here; the statements themselves come from there.
#[cfg(all(test, not(feature = "sqlite")))]
const IS_DISTINCT_FROM: &str = "IS DISTINCT FROM";
#[cfg(all(test, feature = "sqlite"))]
const IS_DISTINCT_FROM: &str = "IS NOT";

// ── Definition ───────────────────────────────────────────────────────────────

/// One `#[derivation]`, produced at compile time by `#[model]`.
///
/// Framework plumbing; not constructed by hand. Every field is `pub` and
/// const-constructible because `#[model]` emits a `static` of this type.
#[derive(Debug)]
pub struct DerivationDef {
    /// Stable identity, `"{parent_table}.{column}"` unless overridden. This is
    /// the `_autumn_derivations` primary key and the name the actuator reports.
    pub name: &'static str,
    /// The child model's type name, for diagnostics.
    pub model: &'static str,
    /// The child's table.
    pub child_table: &'static str,
    /// The child's primary-key column.
    pub child_pk: &'static str,
    /// Whether the child carries a `deleted_at` column, so the derivation
    /// reflects live rows only.
    pub child_soft_delete: bool,
    /// The child's foreign-key column naming the parent.
    pub fk_column: &'static str,
    /// The parent's table.
    pub parent_table: &'static str,
    /// The parent's primary-key column.
    pub parent_pk: &'static str,
    /// The maintained column on the parent.
    pub column: &'static str,
    /// The aggregate as declared: `"count"` or `"sum(<field>)"`.
    pub transform: &'static str,
    /// The filter's source text, `""` when there is none. Reported, never
    /// executed — [`Self::filter_sql`] is what runs.
    pub filter: &'static str,
    /// The filter lowered to SQL: `""`, or ` AND (<pred>)` using `{c}` for the
    /// child alias.
    pub filter_sql: &'static str,
    /// The per-row contribution in SQL: `"1"`, or a child column reference.
    pub contrib_sql: &'static str,
    /// The tenant-discriminator column, from `tenant = "<column>"`.
    pub tenant_column: Option<&'static str>,
    /// The module the derivation was declared in, for diagnostics.
    pub module_path: &'static str,
    /// The source file, for diagnostics.
    pub file: &'static str,
    /// The source line, for diagnostics.
    pub line: u32,
}

impl DerivationDef {
    /// A content address for the derivation's *shape*.
    ///
    /// Lowercase hex SHA-256 over the length-prefixed, labelled fields that
    /// decide what the maintained value is: tables, keys, columns, transform,
    /// lowered filter, contribution and tenant column. Deliberately **not** the
    /// name, model, module path, file, line or filter source, so renaming a
    /// derivation or reformatting its filter does not enqueue a backfill of an
    /// unchanged value — while changing the filter, the transform or the column
    /// always does.
    #[must_use]
    pub fn definition_hash(&self) -> String {
        use sha2::Digest as _;

        let mut hasher = sha2::Sha256::new();
        push_component(&mut hasher, "child_table", self.child_table.as_bytes());
        push_component(&mut hasher, "child_pk", self.child_pk.as_bytes());
        push_component(
            &mut hasher,
            "child_soft_delete",
            if self.child_soft_delete { b"1" } else { b"0" },
        );
        push_component(&mut hasher, "fk_column", self.fk_column.as_bytes());
        push_component(&mut hasher, "parent_table", self.parent_table.as_bytes());
        push_component(&mut hasher, "parent_pk", self.parent_pk.as_bytes());
        push_component(&mut hasher, "column", self.column.as_bytes());
        push_component(&mut hasher, "transform", self.transform.as_bytes());
        push_component(&mut hasher, "filter_sql", self.filter_sql.as_bytes());
        push_component(&mut hasher, "contrib_sql", self.contrib_sql.as_bytes());
        push_component(
            &mut hasher,
            "tenant_column",
            self.tenant_column.unwrap_or("").as_bytes(),
        );
        hex_lower(hasher.finalize())
    }

    /// The derivation as the SQL builders in [`crate::counter_cache`] see it.
    ///
    /// The repair paths (recompute, backfill, drift) are set-based and need no
    /// model type, so they run the *same* builders the delta paths do — one
    /// definition of the ground truth, not two that can disagree.
    pub(crate) const fn sql_view(&self) -> SqlView {
        SqlView {
            child_table: self.child_table,
            child_pk: self.child_pk,
            child_soft_delete: self.child_soft_delete,
            fk_column: self.fk_column,
            parent_table: self.parent_table,
            parent_pk: self.parent_pk,
            counter_column: self.column,
            contrib_sql: self.contrib_sql,
            filter_sql: self.filter_sql,
            tenant_column: self.tenant_column,
        }
    }
}

/// Length-prefix one labelled component into the hash.
///
/// The length prefix is what stops two different field splits from hashing the
/// same, so `column = "ab"` + `transform = "c"` cannot collide with
/// `column = "a"` + `transform = "bc"`.
fn push_component(hasher: &mut sha2::Sha256, label: &str, value: &[u8]) {
    use sha2::Digest as _;

    hasher.update(label.as_bytes());
    hasher.update(b":");
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(b":");
    hasher.update(value);
    hasher.update(b";");
}

fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(
        String::with_capacity(bytes.as_ref().len() * 2),
        |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}

// ── Registry ────────────────────────────────────────────────────────────────

/// Link-time registration of one [`DerivationDef`], emitted by `#[model]`.
#[doc(hidden)]
pub struct DerivationDescriptor {
    /// The registered definition.
    pub def: &'static DerivationDef,
}

inventory::collect!(DerivationDescriptor);

/// Every `#[derivation]` linked into this binary, sorted by name.
///
/// Sorted so the reconciliation order, the backfill order and the actuator
/// listing are the same on every process and every boot.
#[must_use]
pub fn registered_derivations() -> Vec<&'static DerivationDef> {
    let mut defs: Vec<&'static DerivationDef> = inventory::iter::<DerivationDescriptor>
        .into_iter()
        .map(|descriptor| descriptor.def)
        .collect();
    defs.sort_unstable_by_key(|def| def.name);
    defs
}

/// Whether this binary links any `#[derivation]` at all.
///
/// Startup uses it to decide whether to apply the state-table migration and run
/// the reconciliation, so an app with no derivation pays for none of it.
pub(crate) fn has_derivation_descriptors() -> bool {
    inventory::iter::<DerivationDescriptor>
        .into_iter()
        .next()
        .is_some()
}

/// The registered derivation named `name`.
fn find(name: &str) -> Option<&'static DerivationDef> {
    registered_derivations()
        .into_iter()
        .find(|def| def.name == name)
}

/// Reject two derivations claiming one name.
///
/// They would share a `_autumn_derivations` row, so each boot would see the
/// other's hash and enqueue a backfill forever. Naming both module paths is what
/// makes the collision fixable.
fn check_unique_names(defs: &[&DerivationDef]) -> AutumnResult<()> {
    for pair in defs.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(AutumnError::from(std::io::Error::other(format!(
                "two derivations are both named `{}`: {}::{} and {}::{} — give one \
                 a `name = \"…\"` so each has its own backfill state",
                pair[0].name,
                pair[0].module_path,
                pair[0].model,
                pair[1].module_path,
                pair[1].model,
            ))));
        }
    }
    Ok(())
}

// ── State ───────────────────────────────────────────────────────────────────

/// How far a derivation's backfill has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackfillState {
    /// Enqueued, not started. No parent has been repaired yet.
    Pending,
    /// Part-way through: `checkpoint` names the last repaired parent.
    Running,
    /// Every parent has been repaired at least once. The delta paths keep it
    /// current from here.
    Complete,
}

impl BackfillState {
    /// The spelling stored in `_autumn_derivations.backfill_state`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Complete => "complete",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "complete" => Some(Self::Complete),
            _ => None,
        }
    }
}

impl std::fmt::Display for BackfillState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One derivation as `/actuator/derivations` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct DerivationStatus {
    /// The derivation's name.
    pub name: String,
    /// The hash of the definition this process has linked.
    pub definition_hash: String,
    /// The hash recorded in `_autumn_derivations`, or `None` when no row exists
    /// yet. A value different from `definition_hash` means a backfill is due.
    pub stored_hash: Option<String>,
    /// The recorded backfill state, or `None` when no row exists yet.
    pub backfill_state: Option<BackfillState>,
    /// The last repaired parent primary key, when a backfill is in progress.
    pub checkpoint: Option<i64>,
    /// How many parent rows the backfill has repaired in total.
    pub backfilled_rows: i64,
    /// When the row last changed, as the database rendered it.
    pub updated_at: Option<String>,
    /// How many parent rows disagree with the source of truth right now. `0` is
    /// the healthy value; anything else is drift [`recompute`] repairs.
    pub drift: i64,
}

#[derive(diesel::QueryableByName)]
struct StateRow {
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Text)]
    definition_hash: String,
    #[diesel(sql_type = Text)]
    backfill_state: String,
    #[diesel(sql_type = Nullable<BigInt>)]
    checkpoint: Option<i64>,
    #[diesel(sql_type = BigInt)]
    backfilled_rows: i64,
    #[diesel(sql_type = Nullable<Text>)]
    updated_at: Option<String>,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

/// Read every state row, keyed by name.
///
/// `updated_at` is cast to text in SQL rather than decoded as a timestamp: the
/// column is `TIMESTAMPTZ` on Postgres and `TEXT` on `SQLite`, and this is a
/// status field for a human, so one portable statement beats two decoders.
async fn load_state(conn: &mut RuntimeConnection) -> AutumnResult<HashMap<String, StateRow>> {
    let sql = format!(
        "SELECT name, definition_hash, backfill_state, checkpoint, backfilled_rows, \
         CAST(updated_at AS TEXT) AS updated_at FROM {STATE_TABLE} ORDER BY name"
    );
    let rows: Vec<StateRow> = diesel::sql_query(sql)
        .load::<StateRow>(conn)
        .await
        .map_err(AutumnError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| (row.name.clone(), row))
        .collect())
}

/// Enqueue `def` for a backfill from the start.
///
/// One upsert, so a first boot and a definition change take the same path. It
/// resets the checkpoint and the repaired-row count: a changed definition
/// invalidates every parent the previous definition repaired.
async fn enqueue(conn: &mut RuntimeConnection, def: &DerivationDef) -> AutumnResult<()> {
    let sql = format!(
        "INSERT INTO {STATE_TABLE} \
           (name, definition_hash, backfill_state, checkpoint, backfilled_rows, updated_at) \
         VALUES ({}, {}, 'pending', NULL, 0, {NOW}) \
         ON CONFLICT (name) DO UPDATE SET \
           definition_hash = excluded.definition_hash, \
           backfill_state = 'pending', checkpoint = NULL, backfilled_rows = 0, \
           updated_at = {NOW}",
        ph(1),
        ph(2)
    );
    diesel::sql_query(sql)
        .bind::<Text, _>(def.name)
        .bind::<Text, _>(def.definition_hash())
        .execute(conn)
        .await
        .map_err(AutumnError::from)?;
    Ok(())
}

/// Reconcile the registered derivations against `_autumn_derivations`.
///
/// A derivation with no row, or with a stored hash different from the one this
/// binary computes, is enqueued as `pending` with its checkpoint cleared. A
/// derivation whose hash matches is left exactly as it is — which is what keeps
/// a boot from re-backfilling everything it already backfilled.
///
/// Returns the names enqueued, in name order.
///
/// # Errors
///
/// Returns an error when two registered derivations share a name, or when the
/// state table cannot be read or written.
pub async fn ensure_derivations(conn: &mut RuntimeConnection) -> AutumnResult<Vec<&'static str>> {
    let defs = registered_derivations();
    check_unique_names(&defs)?;

    let state = load_state(conn).await?;
    let mut enqueued = Vec::new();
    for def in defs {
        let hash = def.definition_hash();
        if state
            .get(def.name)
            .is_some_and(|row| row.definition_hash == hash)
        {
            continue;
        }
        enqueue(conn, def).await?;
        enqueued.push(def.name);
    }
    Ok(enqueued)
}

// ── Backfill ────────────────────────────────────────────────────────────────

/// How a backfill sweep is paced.
#[derive(Debug, Clone, Copy)]
pub struct BackfillOptions {
    /// Parents repaired per transaction. Each batch takes a row lock on every
    /// parent it rebuilds and holds it until commit, so this bounds how long
    /// concurrent writers to those rows can be blocked.
    pub batch_size: i64,
    /// Stop after this many batches across all derivations, leaving the rest for
    /// the next call. `None` runs to completion.
    pub max_batches: Option<usize>,
}

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            max_batches: None,
        }
    }
}

/// What one [`run_backfill`] call did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BackfillReport {
    /// Derivations that reached `complete` in this call.
    pub completed: Vec<String>,
    /// Derivations still `running` when the call returned, because
    /// [`BackfillOptions::max_batches`] stopped it. Each keeps its checkpoint.
    pub in_progress: Vec<String>,
    /// Parent rows actually repaired — a value that already agreed with the
    /// source of truth is not counted, and is not written.
    pub rows_repaired: usize,
}

/// Advance one derivation's checkpoint. Runs inside the batch's transaction.
async fn advance_checkpoint(
    conn: &mut RuntimeConnection,
    name: &str,
    checkpoint: i64,
    rows: i64,
) -> AutumnResult<()> {
    let sql = format!(
        "UPDATE {STATE_TABLE} SET checkpoint = {checkpoint}, \
         backfilled_rows = backfilled_rows + {rows}, backfill_state = 'running', \
         updated_at = {NOW} WHERE name = {}",
        ph(1)
    );
    diesel::sql_query(sql)
        .bind::<Text, _>(name.to_owned())
        .execute(conn)
        .await
        .map_err(AutumnError::from)?;
    Ok(())
}

/// Mark one derivation's backfill finished.
async fn mark_complete(conn: &mut RuntimeConnection, name: &str) -> AutumnResult<()> {
    let sql = format!(
        "UPDATE {STATE_TABLE} SET backfill_state = 'complete', updated_at = {NOW} \
         WHERE name = {}",
        ph(1)
    );
    diesel::sql_query(sql)
        .bind::<Text, _>(name.to_owned())
        .execute(conn)
        .await
        .map_err(AutumnError::from)?;
    Ok(())
}

/// Repair every parent of every enqueued derivation, in resumable batches.
///
/// Parent ids are paged **outside** any transaction; each page is then repaired
/// and its checkpoint advanced inside **one** transaction. That pairing is what
/// makes a killed backfill safe: the checkpoint can never describe a batch that
/// did not commit, and the repair *assigns* the ground truth rather than
/// adjusting it, so re-running a batch is idempotent anyway.
///
/// Safe against live traffic for the same reason a recompute is: each batch
/// locks the parents it is about to rebuild before reading their children, so it
/// can neither clobber a committed delta nor read a half-applied one.
///
/// A derivation whose stored hash no longer matches this binary's is skipped:
/// another process has re-enqueued it under a different definition, and
/// repairing to the old shape would write values that process would only have to
/// undo.
///
/// # Errors
///
/// Propagates any database error from the paging, repair or checkpoint
/// statements.
pub async fn run_backfill(
    conn: &mut RuntimeConnection,
    options: &BackfillOptions,
) -> AutumnResult<BackfillReport> {
    debug_assert!(options.batch_size > 0, "a backfill batch must hold a row");
    let mut report = BackfillReport::default();
    let mut batches = 0usize;

    let state = load_state(conn).await?;
    let mut pending: Vec<(&'static DerivationDef, Option<i64>)> = Vec::new();
    for (name, row) in &state {
        let Some(state) = BackfillState::parse(&row.backfill_state) else {
            continue;
        };
        if state == BackfillState::Complete {
            continue;
        }
        let Some(def) = find(name) else { continue };
        if def.definition_hash() != row.definition_hash {
            continue;
        }
        pending.push((def, row.checkpoint));
    }
    pending.sort_unstable_by_key(|(def, _)| def.name);

    for (def, stored_checkpoint) in pending {
        let view = def.sql_view();
        let mut cursor = stored_checkpoint;
        loop {
            if options.max_batches.is_some_and(|max| batches >= max) {
                report.in_progress.push(def.name.to_owned());
                return Ok(report);
            }
            let ids = crate::counter_cache::parent_id_page(conn, &view, cursor, options.batch_size)
                .await?;
            let Some(&last) = ids.last() else {
                mark_complete(conn, def.name).await?;
                report.completed.push(def.name.to_owned());
                break;
            };
            cursor = Some(last);
            let name = def.name;
            let rows = i64::try_from(ids.len()).unwrap_or(i64::MAX);
            let repaired =
                scoped_immediate_transaction::<usize, AutumnError, _>(conn, move |conn| {
                    async move {
                        let repaired =
                            crate::counter_cache::recompute_batch_statements(conn, &view, &ids)
                                .await?;
                        advance_checkpoint(conn, name, last, rows).await?;
                        Ok(repaired)
                    }
                    .scope_boxed()
                })
                .await?;
            report.rows_repaired += repaired;
            batches += 1;
        }
    }
    Ok(report)
}

// ── Repair and status ───────────────────────────────────────────────────────

/// Rebuild one derivation's column from the source of truth, everywhere.
///
/// The same batched, lock-then-assign sweep `recompute_counter_caches` runs, so
/// it is idempotent and safe against live traffic. Returns the number of parent
/// rows actually repaired — `0` for a healthy derivation, which also writes
/// nothing.
///
/// # Errors
///
/// Returns an error when `name` is not a registered derivation, and propagates
/// any database error from the sweep.
pub async fn recompute(conn: &mut RuntimeConnection, name: &str) -> AutumnResult<usize> {
    let Some(def) = find(name) else {
        return Err(AutumnError::from(std::io::Error::other(format!(
            "`{name}` is not a derivation registered in this binary"
        ))));
    };
    crate::counter_cache::recompute_view(conn, &def.sql_view(), None).await
}

/// How many parent rows disagree with the source of truth.
///
/// One aggregate statement, but a full scan of the parent table — an operator
/// measurement, not a request-path one.
///
/// # Errors
///
/// Propagates any database error from the `SELECT`.
pub async fn drift(conn: &mut RuntimeConnection, def: &DerivationDef) -> AutumnResult<i64> {
    let sql = crate::counter_cache::drift_sql(&def.sql_view());
    Ok(diesel::sql_query(sql)
        .get_result::<CountRow>(conn)
        .await
        .map_err(AutumnError::from)?
        .count)
}

/// Report every registered derivation: its definition, its recorded backfill
/// state, and its current drift.
///
/// A derivation with no state row reports `stored_hash: None` and
/// `backfill_state: None` — the shape a binary that has not booted against this
/// database yet produces.
///
/// # Errors
///
/// Propagates any database error from the state read or the drift scans.
pub async fn derivation_status(
    conn: &mut RuntimeConnection,
) -> AutumnResult<Vec<DerivationStatus>> {
    let state = load_state(conn).await?;
    let mut out = Vec::new();
    for def in registered_derivations() {
        let row = state.get(def.name);
        out.push(DerivationStatus {
            name: def.name.to_owned(),
            definition_hash: def.definition_hash(),
            stored_hash: row.map(|row| row.definition_hash.clone()),
            backfill_state: row.and_then(|row| BackfillState::parse(&row.backfill_state)),
            checkpoint: row.and_then(|row| row.checkpoint),
            backfilled_rows: row.map_or(0, |row| row.backfilled_rows),
            updated_at: row.and_then(|row| row.updated_at.clone()),
            drift: drift(conn, def).await?,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_def() -> DerivationDef {
        DerivationDef {
            name: "dv_posts.published_comment_count",
            model: "DvComment",
            child_table: "dv_comments",
            child_pk: "id",
            child_soft_delete: false,
            fk_column: "post_id",
            parent_table: "dv_posts",
            parent_pk: "id",
            column: "published_comment_count",
            transform: "count",
            filter: "published",
            filter_sql: " AND ({c}.\"published\" = TRUE)",
            contrib_sql: "1",
            tenant_column: None,
            module_path: "tests::model_derivation",
            file: "model_derivation.rs",
            line: 42,
        }
    }

    fn sum_def() -> DerivationDef {
        DerivationDef {
            name: "dv_posts.visible_score",
            column: "visible_score",
            transform: "sum(score)",
            filter: "published && score > 0",
            filter_sql: " AND ({c}.\"published\" = TRUE) AND ({c}.\"score\" > 0)",
            contrib_sql: "{c}.\"score\"",
            ..count_def()
        }
    }

    #[test]
    fn the_definition_hash_is_stable_for_identical_definitions() {
        assert_eq!(count_def().definition_hash(), count_def().definition_hash());
        assert_eq!(
            count_def().definition_hash().len(),
            64,
            "sha256 renders as 64 hex characters"
        );
        assert!(
            count_def()
                .definition_hash()
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "the hash is lowercase hex"
        );
        assert_ne!(count_def().definition_hash(), sum_def().definition_hash());
    }

    #[test]
    fn the_definition_hash_tracks_every_part_of_the_lowered_shape() {
        let base = count_def().definition_hash();

        // The filter is the whole point: dropping it changes which rows count.
        let mut unfiltered = count_def();
        unfiltered.filter_sql = "";
        assert_ne!(unfiltered.definition_hash(), base, "filter_sql");

        // The contribution decides the weight.
        let mut weighted = count_def();
        weighted.contrib_sql = "{c}.\"score\"";
        assert_ne!(weighted.definition_hash(), base, "contrib_sql");

        // The output column and the aggregate are both part of the shape.
        let mut renamed_column = count_def();
        renamed_column.column = "other_count";
        assert_ne!(renamed_column.definition_hash(), base, "column");

        let mut summed = count_def();
        summed.transform = "sum(score)";
        assert_ne!(summed.definition_hash(), base, "transform");

        // So are the tables it reads and the tenant it is confined to.
        let mut tenanted = count_def();
        tenanted.tenant_column = Some("tenant_id");
        assert_ne!(tenanted.definition_hash(), base, "tenant_column");

        let mut soft = count_def();
        soft.child_soft_delete = true;
        assert_ne!(soft.definition_hash(), base, "child_soft_delete");
    }

    #[test]
    fn the_definition_hash_ignores_where_the_derivation_was_written() {
        // A rename, a move to another file, or a reformatted filter must not
        // enqueue a backfill of a value that did not change.
        let base = count_def().definition_hash();
        let mut moved = count_def();
        moved.name = "renamed";
        moved.model = "OtherComment";
        moved.module_path = "elsewhere";
        moved.file = "elsewhere.rs";
        moved.line = 9001;
        moved.filter = "published /* reformatted */";
        assert_eq!(moved.definition_hash(), base);
    }

    #[test]
    fn a_filtered_count_recompute_counts_only_matching_rows() {
        let sql = crate::counter_cache::recompute_update_sql(&count_def().sql_view(), "1,2");
        assert_eq!(
            sql,
            "UPDATE \"dv_posts\" SET \"published_comment_count\" = \
             (SELECT COUNT(*) FROM \"dv_comments\" AS __autumn_cc_child \
              WHERE __autumn_cc_child.\"post_id\" = \"dv_posts\".\"id\" \
                AND (__autumn_cc_child.\"published\" = TRUE)) \
             WHERE \"dv_posts\".\"id\" IN (1,2) \
               AND \"dv_posts\".\"published_comment_count\" "
                .to_owned()
                + IS_DISTINCT_FROM
                + " (SELECT COUNT(*) FROM \"dv_comments\" AS __autumn_cc_child \
              WHERE __autumn_cc_child.\"post_id\" = \"dv_posts\".\"id\" \
                AND (__autumn_cc_child.\"published\" = TRUE))"
        );
    }

    #[test]
    fn a_sum_recompute_sums_the_contribution() {
        let sql = crate::counter_cache::recompute_update_sql(&sum_def().sql_view(), "5");
        assert!(
            sql.starts_with(
                "UPDATE \"dv_posts\" SET \"visible_score\" = \
                 (SELECT COALESCE(SUM(__autumn_cc_child.\"score\"), 0) \
                  FROM \"dv_comments\" AS __autumn_cc_child"
            ),
            "{sql}"
        );
        assert!(
            sql.contains(
                "AND (__autumn_cc_child.\"published\" = TRUE) \
                 AND (__autumn_cc_child.\"score\" > 0))"
            ),
            "both conjuncts of the filter must survive: {sql}"
        );
        assert!(sql.contains("\"dv_posts\".\"id\" IN (5)"), "{sql}");
    }

    #[test]
    fn drift_is_one_aggregate_over_the_parent_table() {
        let sql = crate::counter_cache::drift_sql(&sum_def().sql_view());
        assert!(
            sql.starts_with("SELECT COUNT(*) AS count FROM \"dv_posts\""),
            "{sql}"
        );
        assert!(
            sql.contains(&format!("\"visible_score\" {IS_DISTINCT_FROM}")),
            "{sql}"
        );
    }

    #[test]
    fn a_backfill_pages_a_thousand_parents_at_a_time_by_default() {
        let options = BackfillOptions::default();
        assert_eq!(options.batch_size, 1000);
        assert!(options.max_batches.is_none(), "the default runs to the end");
    }

    #[test]
    fn a_backfill_state_round_trips_through_its_stored_spelling() {
        for state in [
            BackfillState::Pending,
            BackfillState::Running,
            BackfillState::Complete,
        ] {
            assert_eq!(BackfillState::parse(state.as_str()), Some(state));
            assert_eq!(
                serde_json::to_string(&state).expect("serialize"),
                format!("\"{state}\"")
            );
        }
        assert_eq!(BackfillState::parse("done"), None);
    }

    #[test]
    fn two_derivations_sharing_a_name_are_rejected_with_both_module_paths() {
        let first = count_def();
        let mut second = count_def();
        second.model = "DvOther";
        second.module_path = "other::module";
        let err = check_unique_names(&[&first, &second])
            .expect_err("one name cannot carry two backfill states");
        let message = err.to_string();
        assert!(message.contains("tests::model_derivation"), "{message}");
        assert!(message.contains("other::module"), "{message}");
    }

    #[test]
    fn a_binary_with_no_derivation_registers_none() {
        // The runtime cost of the feature is gated on this: no descriptor means
        // no state table, no reconciliation and no boot task.
        assert!(
            registered_derivations().is_empty(),
            "the library's own unit-test binary declares no derivation"
        );
        assert!(!has_derivation_descriptors());
    }
}
