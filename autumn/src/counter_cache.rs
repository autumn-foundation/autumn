//! Declarative counter caches (issue #1325).
//!
//! A counter cache is a denormalised `{child}_count` column on a parent row
//! that the framework keeps current: `posts.comment_count`,
//! `subreddits.subscriber_count`, `teams.member_count`. Declaring it on the
//! child's association —
//!
//! ```rust,ignore
//! #[autumn_web::model]
//! #[belongs_to(Post, counter_cache)]
//! pub struct Comment { /* … */ }
//! ```
//!
//! — makes the generated `#[repository]` maintain `posts.comment_count` on
//! every insert, delete, soft delete, restore and foreign-key reassignment,
//! **inside the same transaction as the row mutation**, with a single atomic
//! `UPDATE posts SET comment_count = comment_count + $1` (never a
//! read-modify-write, so concurrent inserts cannot lose updates).
//!
//! # Why the maintenance lives on the child
//!
//! The `#[model]` and `#[repository]` derives are separate proc-macro
//! invocations, so the repository macro never sees the model struct's
//! `#[belongs_to]` attributes — exactly the problem
//! [`crate::repository::AutumnDependents`] already solves for dependent
//! cascades. [`AutumnCounterCaches`] is the same bridge: `#[model]` emits an
//! *inherent* `counter_caches()` that shadows the empty blanket, and the
//! generated repository calls `Model::counter_caches()` by concrete path.
//!
//! Because the shadow is inherent, it is **not** visible through a generic
//! `M: AutumnCounterCaches` bound (a generic call would resolve to the blanket
//! default). Every helper in this module therefore takes the spec slice as an
//! argument rather than recovering it from a bound — call them as
//! `counter_cache_after_insert(conn, Comment::counter_caches(), &record)`.
//!
//! # Drift and repair
//!
//! Counters are not clamped: a drifted counter can go negative, and that is
//! deliberate — a negative count is a visible signal, where a silent `GREATEST(0,
//! …)` would hide the bug. [`counter_cache_recompute`] rebuilds the column from
//! the source of truth and is idempotent; the generated repository exposes it as
//! `recompute_counter_caches()` / `recompute_counter_caches_for(parent_id)`.
//!
//! # Identifier safety
//!
//! Every table/column name in the SQL below arrives as a `&'static str` emitted
//! by `#[model]`, and the only user-controlled one (`counter_cache = "…"`) is
//! validated at macro time to be a plain identifier. [`is_plain_identifier`]
//! re-checks that at run time under `debug_assertions` so a hand-constructed
//! spec cannot smuggle SQL through `format!`.

use std::collections::HashMap;

use diesel::sql_types::{BigInt, Nullable};
use diesel_async::RunQueryDsl as _;

use crate::db::RuntimeConnection;
use crate::{AutumnError, AutumnResult};

// Backend-forked placeholders: Postgres numbers its binds, `SQLite` does not.
// Binds are pushed in the same order on both, so one statement template with
// swapped placeholder text serves both backends.
#[cfg(not(feature = "sqlite"))]
const PH1: &str = "$1";
#[cfg(not(feature = "sqlite"))]
const PH2: &str = "$2";
#[cfg(feature = "sqlite")]
const PH1: &str = "?";
#[cfg(feature = "sqlite")]
const PH2: &str = "?";

/// Row lock appended to the sub-selects that read a child row's foreign key.
///
/// Without it there is a window between "read the child's current parent" and
/// "write the child's new parent" in which a concurrent writer can re-parent the
/// row, so the counter would be moved off the wrong parent. Taking the lock on
/// the child row closes it: the concurrent writer blocks until this transaction
/// commits. The lock is on the same row the surrounding mutation locks anyway,
/// so it introduces no new lock-ordering edge.
///
/// `SQLite` has no `SELECT … FOR UPDATE` and needs none: generated write paths
/// begin with `BEGIN IMMEDIATE`, which excludes every other writer for the
/// duration — the same reason `maybe_for_update!` is the identity there.
#[cfg(not(feature = "sqlite"))]
const FOR_UPDATE: &str = " FOR UPDATE";
#[cfg(feature = "sqlite")]
const FOR_UPDATE: &str = "";

/// The alias the generated SQL gives the *child* table whenever it appears in a
/// sub-select. Always aliasing keeps a **self-referential** counter cache
/// (a `Comment` that `belongs_to` a parent `Comment` maintaining `reply_count`)
/// unambiguous, where a bare table name would bind to the outer `UPDATE` target.
const CHILD_ALIAS: &str = "__autumn_cc_child";

/// The soft-delete marker column. Autumn's `soft_delete` convention is fixed, so
/// the predicate is a constant rather than another spec field.
const DELETED_AT: &str = "deleted_at";

/// One counter-cached `belongs_to` leg, produced at compile time by `#[model]`
/// and consulted at run time by the child's generated repository.
///
/// Framework plumbing; not constructed by hand.
#[derive(Debug)]
pub struct CounterCacheSpec<M: 'static> {
    /// The child's table, e.g. `comments`.
    pub child_table: &'static str,
    /// The child's primary-key column, e.g. `id`.
    pub child_pk: &'static str,
    /// Whether the child model carries a `deleted_at` column. When true the
    /// count reflects **live rows only**: the decrement is scoped to a live
    /// child (so a second soft delete moves nothing) and the recompute filters
    /// soft-deleted rows out.
    pub child_soft_delete: bool,
    /// The child's foreign-key column pointing at the parent, e.g. `post_id`.
    pub fk_column: &'static str,
    /// The parent's table, e.g. `posts`.
    pub parent_table: &'static str,
    /// The parent's primary-key column, e.g. `id`.
    pub parent_pk: &'static str,
    /// The maintained column on the parent, e.g. `comment_count`.
    pub counter_column: &'static str,
    /// Reads this leg's foreign key off a child record. `None` for a nullable
    /// foreign key that is unset — an unparented child moves no counter.
    pub fk_of: fn(&M) -> Option<i64>,
    /// Reads the child's own primary key off a record. Used by the bulk paths
    /// to match a post-update record back to the foreign keys captured for it
    /// before the update.
    pub pk_of: fn(&M) -> i64,
}

// A manual `Clone`/`Copy` (rather than a derive) because the derive would add a
// `M: Clone` bound the model types need not satisfy.
impl<M: 'static> Clone for CounterCacheSpec<M> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<M: 'static> Copy for CounterCacheSpec<M> {}

/// Exposes a model's counter-cache specs to its generated repository (#1325).
///
/// A blanket impl returns an empty slice (and `HAS_COUNTER_CACHES = false`) for
/// every type; `#[model]` emits **inherent** items that shadow both when the
/// model declares at least one `#[belongs_to(…, counter_cache)]`. This mirrors
/// [`crate::repository::AutumnDependents`], and carries the same caveat: the
/// shadow is only visible through a *concrete* path (`Comment::counter_caches()`),
/// which is exactly how the generated code names it.
pub trait AutumnCounterCaches: Sized + 'static {
    /// Whether this model has any counter cache at all.
    ///
    /// The generated repository branches on this to decide whether a mutation
    /// path that would otherwise run without a transaction needs to open one.
    /// It is a `const`, so for the overwhelmingly common `false` case the whole
    /// counter-cache path is dead code the optimizer drops.
    const HAS_COUNTER_CACHES: bool = false;

    /// The model's counter-cache specs, in declaration order.
    #[must_use]
    fn counter_caches() -> &'static [CounterCacheSpec<Self>] {
        &[]
    }
}

// Blanket fallback — any type without inherent counter-cache items (i.e. a model
// with no counter-cached association, or a type not built through `#[model]`).
impl<T: Sized + 'static> AutumnCounterCaches for T {}

/// Whether `s` is a plain SQL/Rust identifier: ASCII alphanumerics and
/// underscores, not starting with a digit, non-empty.
///
/// Names reaching the `format!`ed SQL below are macro-emitted and already
/// validated at macro time; this is the run-time backstop for that invariant.
#[must_use]
pub fn is_plain_identifier(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Debug-only guard that every identifier a spec splices into SQL is plain.
fn debug_assert_spec_idents<M: 'static>(spec: &CounterCacheSpec<M>) {
    debug_assert!(
        is_plain_identifier(spec.child_table)
            && is_plain_identifier(spec.child_pk)
            && is_plain_identifier(spec.fk_column)
            && is_plain_identifier(spec.parent_table)
            && is_plain_identifier(spec.parent_pk)
            && is_plain_identifier(spec.counter_column),
        "counter-cache spec carries a non-identifier name; it would be spliced \
         verbatim into SQL"
    );
}

/// `AND __autumn_cc_child.deleted_at IS NULL` (or `IS NOT NULL`) for a
/// soft-deleting child, empty for a child with no `deleted_at` column.
fn live_predicate<M: 'static>(spec: &CounterCacheSpec<M>, want_live: bool) -> String {
    if !spec.child_soft_delete {
        return String::new();
    }
    let op = if want_live { "IS NULL" } else { "IS NOT NULL" };
    format!(" AND {CHILD_ALIAS}.{DELETED_AT} {op}")
}

/// Apply `delta` to one parent's counter with a single atomic statement.
///
/// This is the primitive AC5 rests on: `SET c = c + $1` is resolved by the
/// database, so N concurrent callers commute and none can lose another's update
/// the way a `SELECT` + `UPDATE` round trip would.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`.
pub async fn counter_cache_apply_delta<M: 'static>(
    conn: &mut RuntimeConnection,
    spec: &CounterCacheSpec<M>,
    parent_id: i64,
    delta: i64,
) -> AutumnResult<()> {
    debug_assert_spec_idents(spec);
    let CounterCacheSpec {
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = spec;
    let sql = format!(
        "UPDATE {parent_table} SET {counter_column} = {counter_column} + {PH1} \
         WHERE {parent_pk} = {PH2}"
    );
    diesel::sql_query(sql)
        .bind::<BigInt, _>(delta)
        .bind::<BigInt, _>(parent_id)
        .execute(conn)
        .await
        .map_err(AutumnError::from)?;
    Ok(())
}

/// Apply `delta` to the parent of the child row `child_id`, resolving the parent
/// through a sub-select so no round trip is needed and a `NULL` foreign key is a
/// no-op (`IN (NULL)` matches nothing).
///
/// `require_live` scopes the sub-select to a live child (`deleted_at IS NULL`)
/// for a soft-deleting model, which is what makes a repeated soft delete — or a
/// hard delete of an already-soft-deleted row — decrement exactly once.
/// `require_soft_deleted` is its mirror, used by `restore`.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`.
pub async fn counter_cache_apply_delta_by_child_id<M: 'static>(
    conn: &mut RuntimeConnection,
    spec: &CounterCacheSpec<M>,
    child_id: i64,
    delta: i64,
    child_state: ChildState,
) -> AutumnResult<()> {
    debug_assert_spec_idents(spec);
    let CounterCacheSpec {
        child_table,
        child_pk,
        fk_column,
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = spec;
    let state_predicate = match child_state {
        ChildState::Any => String::new(),
        ChildState::Live => live_predicate(spec, true),
        ChildState::SoftDeleted => live_predicate(spec, false),
    };
    let sql = format!(
        "UPDATE {parent_table} SET {counter_column} = {counter_column} + {PH1} \
         WHERE {parent_table}.{parent_pk} IN \
           (SELECT {CHILD_ALIAS}.{fk_column} FROM {child_table} AS {CHILD_ALIAS} \
            WHERE {CHILD_ALIAS}.{child_pk} = {PH2} \
              AND {CHILD_ALIAS}.{fk_column} IS NOT NULL{state_predicate}{FOR_UPDATE})"
    );
    diesel::sql_query(sql)
        .bind::<BigInt, _>(delta)
        .bind::<BigInt, _>(child_id)
        .execute(conn)
        .await
        .map_err(AutumnError::from)?;
    Ok(())
}

/// Which soft-delete state the child row must be in for a by-id delta to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildState {
    /// No `deleted_at` predicate at all.
    Any,
    /// Only when the child is live (`deleted_at IS NULL`). Used by the
    /// decrement, so a repeated soft delete moves nothing.
    Live,
    /// Only when the child is soft-deleted. Used by `restore`, so restoring an
    /// already-live row moves nothing.
    SoftDeleted,
}

/// Increment every counter-cached parent of a freshly inserted child record.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_after_insert<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    record: &M,
) -> AutumnResult<()> {
    for spec in specs {
        if let Some(parent_id) = (spec.fk_of)(record) {
            counter_cache_apply_delta(conn, spec, parent_id, 1).await?;
        }
    }
    Ok(())
}

/// [`counter_cache_after_insert`] over a batch of records (`save_many`).
///
/// Children sharing a parent are folded into **one** `+ n` statement per parent,
/// so inserting a 1000-row chunk issues one `UPDATE` per distinct parent rather
/// than one per row — still a single atomic statement each, so the concurrency
/// guarantee is unchanged.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_after_insert_many<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    records: &[M],
) -> AutumnResult<()> {
    for spec in specs {
        let mut deltas: HashMap<i64, i64> = HashMap::new();
        for record in records {
            if let Some(parent_id) = (spec.fk_of)(record) {
                *deltas.entry(parent_id).or_insert(0) += 1;
            }
        }
        // Deterministic order so concurrent batches touching the same parents
        // take row locks in a consistent sequence and cannot deadlock.
        let mut parents: Vec<(i64, i64)> = deltas.into_iter().collect();
        parents.sort_unstable();
        for (parent_id, delta) in parents {
            counter_cache_apply_delta(conn, spec, parent_id, delta).await?;
        }
    }
    Ok(())
}

/// Increment every counter-cached parent of the child row `child_id`.
///
/// The documented escape hatch for applications that insert a child with their
/// own SQL inside their own transaction (rather than through the generated
/// repository) and still want the framework to own the counter arithmetic:
///
/// ```rust,ignore
/// let id = diesel::insert_into(comments::table) /* … */ .get_result(conn).await?;
/// counter_cache_after_insert_by_id(conn, Comment::counter_caches(), id).await?;
/// ```
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_after_insert_by_id<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_id: i64,
) -> AutumnResult<()> {
    for spec in specs {
        counter_cache_apply_delta_by_child_id(conn, spec, child_id, 1, ChildState::Any).await?;
    }
    Ok(())
}

/// Decrement every counter-cached parent of the child row `child_id`.
///
/// Must be called **before** the row is deleted or soft-deleted: it resolves the
/// parent from the still-present child row, and (for a soft-deleting model)
/// requires that row to still be live, so the decrement happens exactly once
/// however many times the delete is retried.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_before_delete_by_id<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_id: i64,
) -> AutumnResult<()> {
    for spec in specs {
        let state = if spec.child_soft_delete {
            ChildState::Live
        } else {
            ChildState::Any
        };
        counter_cache_apply_delta_by_child_id(conn, spec, child_id, -1, state).await?;
    }
    Ok(())
}

/// [`counter_cache_before_delete_by_id`] over a batch of ids (`delete_many`).
///
/// Issues **one** statement per spec regardless of batch size: each affected
/// parent's counter drops by the number of its children in the batch, computed
/// by the database. As with the single-row form, this must run *before* the rows
/// are deleted, and for a soft-deleting model it counts only rows that are still
/// live so a repeated bulk delete moves nothing.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_before_delete_many<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_ids: &[i64],
) -> AutumnResult<()> {
    if specs.is_empty() || child_ids.is_empty() {
        return Ok(());
    }
    let id_list = id_list(child_ids);
    for spec in specs {
        debug_assert_spec_idents(spec);
        let CounterCacheSpec {
            child_table,
            child_pk,
            fk_column,
            parent_table,
            parent_pk,
            counter_column,
            ..
        } = spec;
        let live = if spec.child_soft_delete {
            live_predicate(spec, true)
        } else {
            String::new()
        };
        let sql = format!(
            "UPDATE {parent_table} SET {counter_column} = {counter_column} - \
             (SELECT COUNT(*) FROM {child_table} AS {CHILD_ALIAS} \
              WHERE {CHILD_ALIAS}.{fk_column} = {parent_table}.{parent_pk} \
                AND {CHILD_ALIAS}.{child_pk} IN ({id_list}){live}) \
             WHERE {parent_table}.{parent_pk} IN \
               (SELECT {CHILD_ALIAS}.{fk_column} FROM {child_table} AS {CHILD_ALIAS} \
                WHERE {CHILD_ALIAS}.{child_pk} IN ({id_list}) \
                  AND {CHILD_ALIAS}.{fk_column} IS NOT NULL{live})"
        );
        diesel::sql_query(sql)
            .execute(conn)
            .await
            .map_err(AutumnError::from)?;
    }
    Ok(())
}

/// Render `ids` as a literal SQL list.
///
/// The values are `i64`, so their decimal rendering cannot contain anything a
/// SQL parser would treat as syntax — this is a type-level guarantee, not an
/// escaping convention. Inlining them (rather than binding) keeps the statement
/// backend-portable: `SQLite` has no array type for a bound `= ANY($1)`.
fn id_list(ids: &[i64]) -> String {
    let mut out = String::new();
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&id.to_string());
    }
    out
}

/// Increment every counter-cached parent of a child being **restored** from a
/// soft delete.
///
/// Must be called **before** `deleted_at` is cleared: the delta only applies
/// while the row is still soft-deleted, so restoring an already-live row is a
/// no-op rather than an inflation.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_before_restore_by_id<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_id: i64,
) -> AutumnResult<()> {
    for spec in specs {
        // A model with no `deleted_at` has no restore path; guard anyway so a
        // mis-wired call cannot double-increment.
        if spec.child_soft_delete {
            counter_cache_apply_delta_by_child_id(
                conn,
                spec,
                child_id,
                1,
                ChildState::SoftDeleted,
            )
            .await?;
        }
    }
    Ok(())
}

/// Read the current foreign key of each counter-cached leg for child `child_id`.
///
/// Called **before** an update so the post-update record can be compared against
/// it; the outer `Option` is `None` when the child row does not exist. Issues no
/// statement at all when the model has no counter caches.
///
/// # Errors
///
/// Propagates any database error from the `SELECT`s.
pub async fn counter_cache_capture_fks<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_id: i64,
) -> AutumnResult<Vec<Option<i64>>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        debug_assert_spec_idents(spec);
        let CounterCacheSpec {
            child_table,
            child_pk,
            fk_column,
            ..
        } = spec;
        let sql = format!(
            "SELECT {CHILD_ALIAS}.{fk_column} AS fk_value \
             FROM {child_table} AS {CHILD_ALIAS} \
             WHERE {CHILD_ALIAS}.{child_pk} = {PH1}{FOR_UPDATE}"
        );
        let row: Option<FkRow> = diesel::sql_query(sql)
            .bind::<BigInt, _>(child_id)
            .get_result::<FkRow>(conn)
            .await
            .optional_row()?;
        out.push(row.and_then(|r| r.fk_value));
    }
    Ok(out)
}

/// Move the counters for a child whose foreign keys may have just changed.
///
/// `before` is the slice [`counter_cache_capture_fks`] returned prior to the
/// update; `record` is the row as persisted after it. A leg whose foreign key is
/// unchanged issues **no** statement — an ordinary field edit costs nothing.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_after_update<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    before: &[Option<i64>],
    record: &M,
) -> AutumnResult<()> {
    for (index, spec) in specs.iter().enumerate() {
        let old = before.get(index).copied().flatten();
        let new = (spec.fk_of)(record);
        if old == new {
            continue;
        }
        // Apply in ascending parent id, not old-then-new. Two transactions
        // swapping children between parents A and B would otherwise take the two
        // row locks in opposite orders and deadlock; a consistent global order
        // makes that impossible. The two deltas are independent, so ordering
        // them costs nothing.
        let mut moves: Vec<(i64, i64)> = Vec::with_capacity(2);
        if let Some(old_id) = old {
            moves.push((old_id, -1));
        }
        if let Some(new_id) = new {
            moves.push((new_id, 1));
        }
        moves.sort_unstable();
        for (parent_id, delta) in moves {
            counter_cache_apply_delta(conn, spec, parent_id, delta).await?;
        }
    }
    Ok(())
}

/// [`counter_cache_capture_fks`] over a batch of child ids (`update_many`).
///
/// Issues one `SELECT` per spec (not per id), returning `(child id, foreign keys
/// in spec order)` for every row found. Rows absent from the table are simply
/// absent from the result.
///
/// # Errors
///
/// Propagates any database error from the `SELECT`s.
pub async fn counter_cache_capture_fks_many<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_ids: &[i64],
) -> AutumnResult<Vec<(i64, Vec<Option<i64>>)>> {
    if specs.is_empty() || child_ids.is_empty() {
        return Ok(Vec::new());
    }
    let id_list = id_list(child_ids);
    let mut by_child: HashMap<i64, Vec<Option<i64>>> = HashMap::new();
    for (index, spec) in specs.iter().enumerate() {
        debug_assert_spec_idents(spec);
        let CounterCacheSpec {
            child_table,
            child_pk,
            fk_column,
            ..
        } = spec;
        let sql = format!(
            "SELECT {CHILD_ALIAS}.{child_pk} AS child_id, \
             {CHILD_ALIAS}.{fk_column} AS fk_value \
             FROM {child_table} AS {CHILD_ALIAS} \
             WHERE {CHILD_ALIAS}.{child_pk} IN ({id_list}) \
             ORDER BY {CHILD_ALIAS}.{child_pk}{FOR_UPDATE}"
        );
        let rows: Vec<ChildFkRow> = diesel::sql_query(sql)
            .load::<ChildFkRow>(conn)
            .await
            .map_err(AutumnError::from)?;
        for row in rows {
            let entry = by_child
                .entry(row.child_id)
                .or_insert_with(|| vec![None; specs.len()]);
            entry[index] = row.fk_value;
        }
    }
    let mut out: Vec<(i64, Vec<Option<i64>>)> = by_child.into_iter().collect();
    out.sort_unstable_by_key(|(id, _)| *id);
    Ok(out)
}

/// [`counter_cache_after_update`] over a batch of post-update records
/// (`update_many`), matched back to their pre-update foreign keys by primary key.
///
/// A record with no captured "before" entry (its row appeared between the
/// capture and the update) is skipped rather than guessed at; the recompute
/// repair path exists for exactly that kind of edge.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_after_update_many<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    before: &[(i64, Vec<Option<i64>>)],
    records: &[M],
) -> AutumnResult<()> {
    if specs.is_empty() {
        return Ok(());
    }
    let pk_of = specs[0].pk_of;
    for record in records {
        let child_id = pk_of(record);
        let Ok(index) = before.binary_search_by_key(&child_id, |(id, _)| *id) else {
            continue;
        };
        counter_cache_after_update(conn, specs, &before[index].1, record).await?;
    }
    Ok(())
}

/// Move the counters for an `upsert_many` chunk (#1325).
///
/// `existing` is the pre-upsert snapshot the upsert already loaded (and locked)
/// for this chunk, so no extra query is needed: a record present in it is an
/// **update** and gets the before/after foreign-key diff; a record absent from it
/// is an **insert** and gets a plain increment.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_after_upsert_many<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    existing: &[M],
    upserted: &[M],
) -> AutumnResult<()> {
    if specs.is_empty() {
        return Ok(());
    }
    let pk_of = specs[0].pk_of;
    let before: HashMap<i64, Vec<Option<i64>>> = existing
        .iter()
        .map(|row| {
            (
                pk_of(row),
                specs.iter().map(|spec| (spec.fk_of)(row)).collect(),
            )
        })
        .collect();
    let mut inserted: Vec<&M> = Vec::new();
    for record in upserted {
        if let Some(old) = before.get(&pk_of(record)) {
            counter_cache_after_update(conn, specs, old, record).await?;
        } else {
            inserted.push(record);
        }
    }
    for spec in specs {
        let mut deltas: HashMap<i64, i64> = HashMap::new();
        for record in &inserted {
            if let Some(parent_id) = (spec.fk_of)(record) {
                *deltas.entry(parent_id).or_insert(0) += 1;
            }
        }
        let mut parents: Vec<(i64, i64)> = deltas.into_iter().collect();
        parents.sort_unstable();
        for (parent_id, delta) in parents {
            counter_cache_apply_delta(conn, spec, parent_id, delta).await?;
        }
    }
    Ok(())
}

/// Recompute counters from the source of truth.
///
/// With `parent_id = None` every parent row is rebuilt; with `Some(id)` only
/// that parent is touched. Idempotent by construction — the column is *assigned*
/// a `COUNT(*)`, never adjusted — so it is safe to run repeatedly, and it is the
/// supported way to adopt a counter column on an existing table (AC6).
///
/// Returns the number of parent rows updated, summed across every spec.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_recompute<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    parent_id: Option<i64>,
) -> AutumnResult<usize> {
    let mut touched = 0usize;
    for spec in specs {
        debug_assert_spec_idents(spec);
        let CounterCacheSpec {
            child_table,
            fk_column,
            parent_table,
            parent_pk,
            counter_column,
            ..
        } = spec;
        let live = live_predicate(spec, true);
        // The correlated sub-select aliases the child table so a self-referential
        // counter cache (child table == parent table) still binds the outer
        // `UPDATE` target on the right-hand side of the join predicate.
        let mut sql = format!(
            "UPDATE {parent_table} SET {counter_column} = \
             (SELECT COUNT(*) FROM {child_table} AS {CHILD_ALIAS} \
              WHERE {CHILD_ALIAS}.{fk_column} = {parent_table}.{parent_pk}{live})"
        );
        if parent_id.is_some() {
            sql.push_str(&format!(" WHERE {parent_table}.{parent_pk} = {PH1}"));
        }
        let query = diesel::sql_query(sql);
        let updated = if let Some(id) = parent_id {
            query.bind::<BigInt, _>(id).execute(conn).await
        } else {
            query.execute(conn).await
        }
        .map_err(AutumnError::from)?;
        touched += updated;
    }
    Ok(touched)
}

#[derive(diesel::QueryableByName)]
struct ChildFkRow {
    #[diesel(sql_type = BigInt)]
    child_id: i64,
    #[diesel(sql_type = Nullable<BigInt>)]
    fk_value: Option<i64>,
}

#[derive(diesel::QueryableByName)]
struct FkRow {
    #[diesel(sql_type = Nullable<BigInt>)]
    fk_value: Option<i64>,
}

/// `Result::optional`, spelled locally so this module does not have to pull
/// diesel's `OptionalExtension` into every call site's scope.
trait OptionalRow<T> {
    fn optional_row(self) -> AutumnResult<Option<T>>;
}

impl<T> OptionalRow<T> for Result<T, diesel::result::Error> {
    fn optional_row(self) -> AutumnResult<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(diesel::result::Error::NotFound) => Ok(None),
            Err(e) => Err(AutumnError::from(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy;

    fn spec(soft: bool) -> CounterCacheSpec<Dummy> {
        CounterCacheSpec {
            child_table: "comments",
            child_pk: "id",
            child_soft_delete: soft,
            fk_column: "post_id",
            parent_table: "posts",
            parent_pk: "id",
            counter_column: "comment_count",
            fk_of: |_| Some(1),
            pk_of: |_| 1,
        }
    }

    #[test]
    fn plain_identifiers_are_accepted_and_sql_fragments_are_not() {
        assert!(is_plain_identifier("comment_count"));
        assert!(is_plain_identifier("_x9"));
        assert!(!is_plain_identifier(""));
        assert!(!is_plain_identifier("9lives"));
        assert!(!is_plain_identifier("comment_count; DROP TABLE posts"));
        assert!(!is_plain_identifier("comment count"));
        assert!(!is_plain_identifier("\"comment_count\""));
    }

    #[test]
    fn the_live_predicate_is_emitted_only_for_a_soft_deleting_child() {
        assert_eq!(live_predicate(&spec(false), true), "");
        assert_eq!(
            live_predicate(&spec(true), true),
            format!(" AND {CHILD_ALIAS}.deleted_at IS NULL")
        );
        assert_eq!(
            live_predicate(&spec(true), false),
            format!(" AND {CHILD_ALIAS}.deleted_at IS NOT NULL")
        );
    }

    #[test]
    fn a_model_without_an_inherent_shadow_resolves_to_the_empty_blanket() {
        assert!(!Dummy::HAS_COUNTER_CACHES);
        assert!(Dummy::counter_caches().is_empty());
    }
}
