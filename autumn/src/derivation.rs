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
//! A counter cache is correct from its first row, because the column and the
//! code that maintains it ship together. A derivation is usually declared over
//! a table that already holds data, so the existing rows have to be repaired.
//! This module owns that part:
//!
//! * **Content addressing.** [`DerivationDef::definition_hash`] hashes the
//!   lowered shape: tables, columns, transform, filter SQL. A changed filter
//!   changes the hash. A rename or a reformat does not.
//! * **Reconciliation.** [`ensure_derivations`] compares each registered
//!   derivation's hash against `_autumn_derivations` and enqueues a backfill for
//!   the ones that changed. It leaves the rest alone.
//! * **Resumable repair.** [`run_backfill`] rebuilds parents in checkpointed
//!   batches. Each batch is one transaction that locks the state row, pages from
//!   the checkpoint it finds there, repairs the page and advances the
//!   checkpoint. A killed process resumes, and several replicas cooperate on one
//!   sweep instead of racing.
//! * **Observability.** [`derivation_status`] reports each derivation's state
//!   and its drift from the source of truth. `/actuator/derivations` serves it.

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

// Row lock on the state row. On Postgres `FOR UPDATE` blocks any other
// transaction that wants the same row. On `SQLite` the enclosing
// `BEGIN IMMEDIATE` already excludes every other writer in the database, so the
// clause degrades to nothing and the read is a cheap indexed lookup.
#[cfg(not(feature = "sqlite"))]
const FOR_UPDATE: &str = " FOR UPDATE";
#[cfg(feature = "sqlite")]
const FOR_UPDATE: &str = "";

/// The cap on a drift scan, in parent rows.
///
/// A drift figure equal to this value means "at least this many", not "exactly
/// this many". See [`DerivationStatus::drift`].
pub const DRIFT_SCAN_LIMIT: i64 = 10_000;

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

/// Reject a registry that cannot be reconciled.
///
/// Both collisions below are programming errors, like a duplicate route, so
/// every entry point checks them rather than only the boot path.
fn check_registry(defs: &[&DerivationDef]) -> AutumnResult<()> {
    check_unique_names(defs)?;
    check_unique_columns(defs)
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
                "two derivations are both named `{}`: {}::{} and {}::{}. Give one \
                 a `name = \"...\"` so each has its own backfill state",
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

/// Reject two derivations maintaining one parent column.
///
/// Every mutation path applies each derivation's own delta, so one column with
/// two derivations counts twice. No repair can fix that: the two definitions
/// disagree on what the column means, so each sweep would undo the other.
fn check_unique_columns(defs: &[&DerivationDef]) -> AutumnResult<()> {
    let mut seen: HashMap<(&str, &str), &DerivationDef> = HashMap::new();
    for def in defs {
        if let Some(first) = seen.insert((def.parent_table, def.column), def) {
            return Err(AutumnError::from(std::io::Error::other(format!(
                "two derivations both maintain `{}.{}`: `{}` on {}::{} and `{}` on \
                 {}::{}. The column would count twice, so remove one or point it \
                 at another column",
                def.parent_table,
                def.column,
                first.name,
                first.module_path,
                first.model,
                def.name,
                def.module_path,
                def.model,
            ))));
        }
    }
    Ok(())
}

/// Check the linked registry without touching a database.
///
/// The boot path calls this before it opens a connection, so a duplicate name
/// or two derivations on one parent column stop the process rather than reach
/// the data. Both are programming errors.
///
/// # Errors
///
/// Returns an error naming both offenders and their module paths.
pub fn check_registered_derivations() -> AutumnResult<()> {
    check_registry(&registered_derivations())
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
    /// A `_autumn_derivations` row whose derivation this binary does not
    /// declare, left behind by a removed or renamed definition. Never stored:
    /// the state table's `CHECK` does not admit the spelling.
    Unregistered,
}

impl BackfillState {
    /// The spelling stored in `_autumn_derivations.backfill_state`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Unregistered => "unregistered",
        }
    }

    /// Parse a stored spelling. `unregistered` is deliberately not accepted: it
    /// is a report-only state, so a row carrying it is a corrupt row.
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "complete" => Some(Self::Complete),
            _ => None,
        }
    }

    /// Whether a backfill sweep still has work to do for this state.
    const fn is_sweepable(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
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
    /// The hash of the definition this process has linked, or `None` for a
    /// state row this binary declares no derivation for
    /// ([`BackfillState::Unregistered`]).
    pub definition_hash: Option<String>,
    /// The hash recorded in `_autumn_derivations`, or `None` when no row exists
    /// yet. A value different from `definition_hash` means a backfill is due.
    pub stored_hash: Option<String>,
    /// The recorded backfill state, or `None` when no row exists yet.
    pub backfill_state: Option<BackfillState>,
    /// The last repaired parent primary key. It stays populated after the
    /// backfill completes, because it records the last position the sweep
    /// applied rather than an in-flight cursor.
    pub checkpoint: Option<i64>,
    /// How many parent rows the backfill has visited. The checkpoint pages the
    /// parents and the repair assigns the ground truth to each, so this counts
    /// visits, not writes: a page that already agreed is visited and not
    /// written.
    pub backfilled_rows: i64,
    /// When the row last changed, as the database rendered it.
    pub updated_at: Option<String>,
    /// How many parent rows disagree with the source of truth right now. `0` is
    /// the healthy value; anything else is drift [`recompute`] repairs.
    ///
    /// The scan stops at [`DRIFT_SCAN_LIMIT`], so a value equal to that limit
    /// means "at least that many". `None` when the scan could not run
    /// (see [`Self::drift_error`]) or when the derivation is unregistered.
    pub drift: Option<i64>,
    /// Why the drift scan did not run, when it did not.
    ///
    /// A missing derived column is the common case: the migration that adds it
    /// has not been applied yet. The other derivations are still reported, so
    /// one broken derivation cannot hide the rest.
    pub drift_error: Option<String>,
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
/// Returns an error when two registered derivations share a name, when two
/// maintain the same parent column, or when the state table cannot be read or
/// written. The first two are programming errors, and the boot path treats them
/// as fatal: a column with two derivations double counts, which is data
/// corruption rather than staleness.
pub async fn ensure_derivations(conn: &mut RuntimeConnection) -> AutumnResult<Vec<&'static str>> {
    let defs = registered_derivations();
    check_registry(&defs)?;

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
    /// Every derivation still pending or running when the call returned,
    /// because [`BackfillOptions::max_batches`] stopped it. Each keeps its
    /// committed checkpoint, so the next call resumes rather than restarts.
    pub in_progress: Vec<String>,
    /// Parent rows actually repaired. A value that already agreed with the
    /// source of truth is neither counted here nor written.
    pub rows_repaired: usize,
}

/// Advance one derivation's checkpoint. Runs inside the batch's transaction.
///
/// Guarded by name **and** hash. Another replica may have re-enqueued this
/// derivation under a new definition between the lock and this write only if the
/// lock was released, which cannot happen inside the transaction; the guard is
/// what makes that reasoning independent of the lock, so a checkpoint can never
/// describe a definition other than the one that produced it.
async fn advance_checkpoint(
    conn: &mut RuntimeConnection,
    name: &str,
    hash: &str,
    checkpoint: i64,
    rows: i64,
) -> AutumnResult<()> {
    let sql = format!(
        "UPDATE {STATE_TABLE} SET checkpoint = {checkpoint}, \
         backfilled_rows = backfilled_rows + {rows}, backfill_state = 'running', \
         updated_at = {NOW} WHERE name = {} AND definition_hash = {}",
        ph(1),
        ph(2)
    );
    diesel::sql_query(sql)
        .bind::<Text, _>(name.to_owned())
        .bind::<Text, _>(hash.to_owned())
        .execute(conn)
        .await
        .map_err(AutumnError::from)?;
    Ok(())
}

/// Mark one derivation's backfill finished, guarded by name and hash.
///
/// The hash guard is the important half: marking a derivation complete records
/// "every parent now matches this definition". Writing that against a row that
/// carries a different definition would declare the new definition complete with
/// values the old one produced.
async fn mark_complete(conn: &mut RuntimeConnection, name: &str, hash: &str) -> AutumnResult<()> {
    let sql = format!(
        "UPDATE {STATE_TABLE} SET backfill_state = 'complete', updated_at = {NOW} \
         WHERE name = {} AND definition_hash = {}",
        ph(1),
        ph(2)
    );
    diesel::sql_query(sql)
        .bind::<Text, _>(name.to_owned())
        .bind::<Text, _>(hash.to_owned())
        .execute(conn)
        .await
        .map_err(AutumnError::from)?;
    Ok(())
}

/// The state row as one batch transaction re-reads it under its lock.
#[derive(diesel::QueryableByName)]
struct LockedRow {
    #[diesel(sql_type = Text)]
    definition_hash: String,
    #[diesel(sql_type = Text)]
    backfill_state: String,
    #[diesel(sql_type = Nullable<BigInt>)]
    checkpoint: Option<i64>,
}

/// What one batch transaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Batch {
    /// The state row no longer asks this process to sweep. It is gone, it
    /// carries another definition, or it is already complete.
    Stopped,
    /// The page was empty, so the sweep reached the end of the parent table and
    /// the row is now `complete`.
    Completed,
    /// One page was repaired and the checkpoint moved past it.
    Advanced {
        /// Parent rows this page actually wrote.
        repaired: usize,
    },
}

/// One backfill batch, as one transaction.
///
/// The transaction is the whole design. It locks the state row first, so the row
/// is both the cursor and the mutex: any number of replicas can call this
/// concurrently and they take turns on one sweep instead of each running their
/// own. Every step after the lock reads the state the lock protects:
///
/// 1. lock the state row and re-read hash, state and checkpoint;
/// 2. stop when the row is gone, carries another definition, or is complete;
/// 3. page parent ids after the checkpoint the row carries, inside this
///    transaction;
/// 4. an empty page means the end of the table: mark complete and stop;
/// 5. otherwise lock the parents in ascending id order, assign the ground truth,
///    then advance the checkpoint and the visited-row count.
///
/// Steps 4 and 5 guard their writes by name and hash, so a definition that
/// changed under this process cannot be marked complete or advanced with values
/// the previous definition produced.
async fn run_one_batch(
    conn: &mut RuntimeConnection,
    def: &'static DerivationDef,
    batch_size: i64,
) -> AutumnResult<Batch> {
    let view = def.sql_view();
    let name = def.name;
    let hash = def.definition_hash();
    scoped_immediate_transaction::<Batch, AutumnError, _>(conn, move |conn| {
        async move {
            let lock_sql = format!(
                "SELECT definition_hash, backfill_state, checkpoint FROM {STATE_TABLE} \
                 WHERE name = {}{FOR_UPDATE}",
                ph(1)
            );
            let locked = diesel::sql_query(lock_sql)
                .bind::<Text, _>(name)
                .load::<LockedRow>(&mut *conn)
                .await
                .map_err(AutumnError::from)?
                .into_iter()
                .next();

            let Some(row) = locked else {
                return Ok(Batch::Stopped);
            };
            if row.definition_hash != hash {
                return Ok(Batch::Stopped);
            }
            if !BackfillState::parse(&row.backfill_state).is_some_and(BackfillState::is_sweepable) {
                return Ok(Batch::Stopped);
            }

            let ids =
                crate::counter_cache::parent_id_page(&mut *conn, &view, row.checkpoint, batch_size)
                    .await?;
            let Some(&last) = ids.last() else {
                mark_complete(&mut *conn, name, &hash).await?;
                return Ok(Batch::Completed);
            };
            let visited = i64::try_from(ids.len()).unwrap_or(i64::MAX);
            let repaired =
                crate::counter_cache::recompute_batch_statements(&mut *conn, &view, &ids).await?;
            advance_checkpoint(&mut *conn, name, &hash, last, visited).await?;
            Ok(Batch::Advanced { repaired })
        }
        .scope_boxed()
    })
    .await
}

/// Repair every parent of every enqueued derivation, in resumable batches.
///
/// Each batch is **one** transaction that locks the derivation's state row,
/// re-reads its hash, state and checkpoint, pages the parents after that
/// checkpoint, repairs them and advances the checkpoint. See [`run_one_batch`]
/// for the exact sequence.
///
/// The state-row lock is the cross-process mutex. Several replicas booting a new
/// definition therefore cooperate on one sweep: each takes the row in turn, sees
/// the checkpoint the previous one committed, and repairs the next page. No
/// advisory lock is involved, `backfilled_rows` stays exact, and no page is
/// repaired twice.
///
/// A definition that changed under this process is dropped rather than repaired.
/// Every state write is guarded by name **and** hash, so the process that
/// re-enqueued it owns the sweep and this one cannot mark the new definition
/// complete with values the old one produced.
///
/// [`BackfillOptions::max_batches`] bounds the call, not the sweep. When the
/// budget runs out, every derivation still pending or running is reported in
/// [`BackfillReport::in_progress`] and the next call resumes from the committed
/// checkpoints.
///
/// # Errors
///
/// Propagates any database error from the paging, repair or checkpoint
/// statements, and returns an error when the registry carries a duplicate name
/// or two derivations on one parent column.
pub async fn run_backfill(
    conn: &mut RuntimeConnection,
    options: &BackfillOptions,
) -> AutumnResult<BackfillReport> {
    debug_assert!(options.batch_size > 0, "a backfill batch must hold a row");
    let defs = registered_derivations();
    check_registry(&defs)?;

    let mut report = BackfillReport::default();
    let mut batches = 0usize;

    // One read outside the transactions, only to pick the candidates. Each batch
    // re-reads its row under the row lock, so this snapshot never decides a
    // write.
    let state = load_state(conn).await?;
    let candidates: Vec<&'static DerivationDef> = defs
        .into_iter()
        .filter(|def| {
            state.get(def.name).is_some_and(|row| {
                row.definition_hash == def.definition_hash()
                    && BackfillState::parse(&row.backfill_state)
                        .is_some_and(BackfillState::is_sweepable)
            })
        })
        .collect();

    let mut budget_spent = false;
    for def in candidates {
        // The budget stops the call, not the report: a derivation this call
        // never reached is still pending, so it belongs in `in_progress`.
        if budget_spent {
            report.in_progress.push(def.name.to_owned());
            continue;
        }
        loop {
            if options.max_batches.is_some_and(|max| batches >= max) {
                budget_spent = true;
                report.in_progress.push(def.name.to_owned());
                break;
            }
            match run_one_batch(conn, def, options.batch_size).await? {
                Batch::Stopped => break,
                Batch::Completed => {
                    report.completed.push(def.name.to_owned());
                    break;
                }
                Batch::Advanced { repaired } => {
                    report.rows_repaired += repaired;
                    batches += 1;
                }
            }
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

/// How many parent rows disagree with the source of truth, up to
/// [`DRIFT_SCAN_LIMIT`].
///
/// One aggregate statement. The cap is what keeps it usable on a large table: a
/// figure equal to the limit means "at least that many", which is all an
/// operator needs to decide to recompute. Still an operator measurement rather
/// than a request-path one.
///
/// # Errors
///
/// Propagates any database error from the `SELECT`. A missing derived column is
/// the common case, and it is reported per derivation rather than failing the
/// whole status read (see [`derivation_status`]).
pub async fn drift(conn: &mut RuntimeConnection, def: &DerivationDef) -> AutumnResult<i64> {
    let sql = crate::counter_cache::drift_sql(&def.sql_view(), DRIFT_SCAN_LIMIT);
    Ok(diesel::sql_query(sql)
        .get_result::<CountRow>(conn)
        .await
        .map_err(AutumnError::from)?
        .count)
}

/// Report every registered derivation: its definition, its recorded backfill
/// state, and its current drift. Then report every state row this binary
/// declares no derivation for.
///
/// A derivation with no state row reports `stored_hash: None` and
/// `backfill_state: None`, which is the shape a binary that has not booted
/// against this database yet produces. A state row with no derivation reports
/// [`BackfillState::Unregistered`] and `definition_hash: None`, which is what a
/// removed or renamed definition leaves behind. Such a row is reported rather
/// than deleted, because only an operator can tell a removed derivation apart
/// from a rolling deploy that has not finished.
///
/// A failing drift scan is reported on its own row in
/// [`DerivationStatus::drift_error`] and does not stop the others. A derived
/// column that is not there yet is the common case, and it must not hide the
/// derivations that are healthy.
///
/// Rows come back sorted by name.
///
/// # Errors
///
/// Propagates any database error from the state read, and returns an error when
/// the registry carries a duplicate name or two derivations on one parent
/// column.
pub async fn derivation_status(
    conn: &mut RuntimeConnection,
) -> AutumnResult<Vec<DerivationStatus>> {
    let defs = registered_derivations();
    check_registry(&defs)?;

    let state = load_state(conn).await?;
    let mut out = Vec::with_capacity(defs.len() + state.len());
    for def in &defs {
        let row = state.get(def.name);
        let (drifted, drift_error) = match drift(conn, def).await {
            Ok(count) => (Some(count), None),
            Err(error) => (None, Some(error.to_string())),
        };
        out.push(DerivationStatus {
            name: def.name.to_owned(),
            definition_hash: Some(def.definition_hash()),
            stored_hash: row.map(|row| row.definition_hash.clone()),
            backfill_state: row.and_then(|row| BackfillState::parse(&row.backfill_state)),
            checkpoint: row.and_then(|row| row.checkpoint),
            backfilled_rows: row.map_or(0, |row| row.backfilled_rows),
            updated_at: row.and_then(|row| row.updated_at.clone()),
            drift: drifted,
            drift_error,
        });
    }
    for (name, row) in &state {
        if defs.iter().any(|def| def.name == name.as_str()) {
            continue;
        }
        out.push(DerivationStatus {
            name: name.clone(),
            definition_hash: None,
            stored_hash: Some(row.definition_hash.clone()),
            backfill_state: Some(BackfillState::Unregistered),
            checkpoint: row.checkpoint,
            backfilled_rows: row.backfilled_rows,
            updated_at: row.updated_at.clone(),
            drift: None,
            drift_error: None,
        });
    }
    out.sort_by(|left, right| left.name.cmp(&right.name));
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
        let sql = crate::counter_cache::drift_sql(&sum_def().sql_view(), DRIFT_SCAN_LIMIT);
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
            assert!(state.is_sweepable() != (state == BackfillState::Complete));
        }
        assert_eq!(BackfillState::parse("done"), None);

        // `unregistered` is reported, never stored: the state table's `CHECK`
        // does not admit it, so a row carrying it is a corrupt row.
        assert_eq!(BackfillState::Unregistered.as_str(), "unregistered");
        assert_eq!(BackfillState::parse("unregistered"), None);
        assert!(!BackfillState::Unregistered.is_sweepable());
        assert_eq!(
            serde_json::to_string(&BackfillState::Unregistered).expect("serialize"),
            "\"unregistered\""
        );
    }

    #[test]
    fn two_derivations_on_one_parent_column_are_rejected() {
        // Both would apply their own delta on every mutation, so the column
        // would count twice. That is data corruption, not staleness, which is
        // why the boot path refuses to start on it.
        let first = count_def();
        let mut second = count_def();
        second.name = "dv_posts.published_comment_count_again";
        second.model = "DvOtherComment";
        second.module_path = "other::module";
        second.filter_sql = "";
        let err = check_unique_columns(&[&first, &second])
            .expect_err("one column cannot carry two derivations");
        let message = err.to_string();
        assert!(message.contains("dv_posts.published_comment_count"), "{message}");
        assert!(message.contains("other::module"), "{message}");
        assert!(message.contains("count twice"), "{message}");

        // A second derivation on another column of the same parent is fine.
        let mut sibling = count_def();
        sibling.name = "dv_posts.visible_score";
        sibling.column = "visible_score";
        check_unique_columns(&[&first, &sibling]).expect("two columns, two derivations");

        // `check_registry` runs both checks, so it catches this one too.
        check_registry(&[&first, &second]).expect_err("the registry check covers columns");
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
        let sql = crate::counter_cache::drift_sql(&sum_def().sql_view(), DRIFT_SCAN_LIMIT);
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
            assert!(state.is_sweepable() != (state == BackfillState::Complete));
        }
        assert_eq!(BackfillState::parse("done"), None);

        // `unregistered` is reported, never stored: the state table's `CHECK`
        // does not admit it, so a row carrying it is a corrupt row.
        assert_eq!(BackfillState::Unregistered.as_str(), "unregistered");
        assert_eq!(BackfillState::parse("unregistered"), None);
        assert!(!BackfillState::Unregistered.is_sweepable());
        assert_eq!(
            serde_json::to_string(&BackfillState::Unregistered).expect("serialize"),
            "\"unregistered\""
        );
    }

    #[test]
    fn two_derivations_on_one_parent_column_are_rejected() {
        // Both would apply their own delta on every mutation, so the column
        // would count twice. That is data corruption, not staleness, which is
        // why the boot path refuses to start on it.
        let first = count_def();
        let mut second = count_def();
        second.name = "dv_posts.published_comment_count_again";
        second.model = "DvOtherComment";
        second.module_path = "other::module";
        second.filter_sql = "";
        let err = check_unique_columns(&[&first, &second])
            .expect_err("one column cannot carry two derivations");
        let message = err.to_string();
        assert!(message.contains("dv_posts.published_comment_count"), "{message}");
        assert!(message.contains("other::module"), "{message}");
        assert!(message.contains("count twice"), "{message}");

        // A second derivation on another column of the same parent is fine.
        let mut sibling = count_def();
        sibling.name = "dv_posts.visible_score";
        sibling.column = "visible_score";
        check_unique_columns(&[&first, &sibling]).expect("two columns, two derivations");

        // `check_registry` runs both checks, so it catches this one too.
        check_registry(&[&first, &second]).expect_err("the registry check covers columns");
    }

    #[test]
    fn a_status_row_serializes_every_field_the_actuator_documents() {
        let status = DerivationStatus {
            name: "dv_posts.published_comment_count".to_owned(),
            definition_hash: Some(count_def().definition_hash()),
            stored_hash: None,
            backfill_state: Some(BackfillState::Pending),
            checkpoint: None,
            backfilled_rows: 0,
            updated_at: None,
            drift: Some(DRIFT_SCAN_LIMIT),
            drift_error: None,
        };
        let json = serde_json::to_value(&status).expect("serialize");
        let object = json.as_object().expect("a status is a JSON object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "backfill_state",
                "backfilled_rows",
                "checkpoint",
                "definition_hash",
                "drift",
                "drift_error",
                "name",
                "stored_hash",
                "updated_at",
            ]
        );
        // An absent value is reported as `null` rather than dropped, so a
        // consumer can tell "not measured" from "zero".
        assert!(object["stored_hash"].is_null());
        assert!(object["drift_error"].is_null());
        assert_eq!(object["drift"], serde_json::json!(DRIFT_SCAN_LIMIT));
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
