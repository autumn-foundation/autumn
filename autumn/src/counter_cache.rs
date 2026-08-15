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
use std::fmt::Write as _;

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

/// NULL-safe inequality. Postgres spells it `IS DISTINCT FROM`; `SQLite` has
/// spelled the same operator `IS NOT` since long before it gained the SQL
/// standard alias, so the short form is the portable one there.
#[cfg(not(feature = "sqlite"))]
const IS_DISTINCT_FROM: &str = "IS DISTINCT FROM";
#[cfg(feature = "sqlite")]
const IS_DISTINCT_FROM: &str = "IS NOT";

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
    /// before the update, and to scope the tenant predicate below.
    pub pk_of: fn(&M) -> i64,
    /// Whether a record is live (not soft-deleted). Always `true` for a child
    /// with no `deleted_at` column.
    ///
    /// The generated `update` does **not** filter soft-deleted rows, so an
    /// already-deleted child can be re-parented. It is counted by nobody, so
    /// neither its old nor its new parent may move — without this the update
    /// would decrement a parent that had already dropped it and increment one
    /// for a dead row.
    pub live_of: fn(&M) -> bool,
    /// The tenant-discriminator column, from
    /// `#[belongs_to(…, counter_cache, counter_cache_tenant = "<column>")]`.
    ///
    /// A counter update is a write to a row the caller only had to name the id
    /// of, so on a shared multi-tenant table it has to be confined to the
    /// caller's own tenant — the same hazard `#[votable]`'s aggregate `UPDATE`
    /// is scoped for. The predicate spells the invariant "the parent sits in the
    /// same tenant as the child whose foreign key named it", so **both** tables
    /// must carry a column of this name. That is why it is explicit: `#[model]`
    /// on the child cannot see the parent's fields, and guessing would turn
    /// every tenant-scoped child hanging off a global parent into a hard
    /// `column does not exist`.
    ///
    /// `None` (the default) emits no predicate anywhere, so a single-tenant
    /// app's SQL is byte-for-byte what it would be without this field.
    pub tenant_column: Option<&'static str>,
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
/// Module-private: the whole module is `pub(crate)`, so a `pub(crate)` here
/// would be redundant.
///
/// Names reaching the `format!`ed SQL below are macro-emitted and already
/// validated at macro time; this is the run-time backstop for that invariant.
#[must_use]
fn is_plain_identifier(s: &str) -> bool {
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

/// `AND <parent>.<tenant> = __autumn_cc_child.<tenant>`, for the statements that
/// already have the child row joined in a sub-select. Empty when the association
/// declares no `counter_cache_tenant`.
///
/// The predicate must be applied to **every** delta path or it makes things
/// worse rather than better: scoping only the increment would leave the matching
/// decrement unscoped, so a cross-tenant foreign key would drive another
/// tenant's counter down without ever driving it up.
///
/// `AND <parent>.tenant IN (SELECT x.tenant FROM <child> x WHERE x.<pk> = N)`
/// for the parent-keyed statements, which have no child row to join, empty
/// otherwise.
///
/// Deliberately a sub-select on the child row rather than a bound ambient
/// tenant: the invariant being enforced is "the parent is in the same tenant as
/// the child that names it", which is exactly what makes a cross-tenant foreign
/// key move nothing. It needs no tenant plumbing through the generated code, and
/// it is correct for `across_tenants()` callers too.
fn tenant_predicate_joined<M: 'static>(spec: &CounterCacheSpec<M>) -> String {
    let Some(tenant_column) = spec.tenant_column else {
        return String::new();
    };
    let parent_table = spec.parent_table;
    format!(" AND {parent_table}.{tenant_column} = {CHILD_ALIAS}.{tenant_column}")
}

fn tenant_predicate<M: 'static>(spec: &CounterCacheSpec<M>, child_id: i64) -> String {
    let Some(tenant_column) = spec.tenant_column else {
        return String::new();
    };
    let CounterCacheSpec {
        child_table,
        child_pk,
        parent_table,
        ..
    } = spec;
    format!(
        " AND {parent_table}.{tenant_column} IN \
         (SELECT {CHILD_ALIAS}_t.{tenant_column} FROM {child_table} AS {CHILD_ALIAS}_t \
          WHERE {CHILD_ALIAS}_t.{child_pk} = {child_id})"
    )
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
    scope: TenantScope,
) -> AutumnResult<()> {
    debug_assert_spec_idents(spec);
    let CounterCacheSpec {
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = spec;
    let tenant = match scope {
        TenantScope::SameTenantAsChild(child_id) => tenant_predicate(spec, child_id),
        TenantScope::Unscoped => String::new(),
    };
    let sql = format!(
        "UPDATE {parent_table} SET {counter_column} = {counter_column} + {PH1} \
         WHERE {parent_table}.{parent_pk} = {PH2}{tenant}"
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
#[doc(hidden)]
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
    // The parent is resolved by a sub-select on the child row, so the tenant
    // check is a correlated comparison in the outer `WHERE` — it names the child
    // alias, which is only in scope for a sub-select the outer statement
    // correlates with, so the whole predicate moves into a second sub-select.
    let tenant = tenant_predicate(spec, child_id);
    let sql = format!(
        "UPDATE {parent_table} SET {counter_column} = {counter_column} + {PH1} \
         WHERE {parent_table}.{parent_pk} IN \
           (SELECT {CHILD_ALIAS}.{fk_column} FROM {child_table} AS {CHILD_ALIAS} \
            WHERE {CHILD_ALIAS}.{child_pk} = {PH2} \
              AND {CHILD_ALIAS}.{fk_column} IS NOT NULL{state_predicate}{FOR_UPDATE}){tenant}"
    );
    diesel::sql_query(sql)
        .bind::<BigInt, _>(delta)
        .bind::<BigInt, _>(child_id)
        .execute(conn)
        .await
        .map_err(AutumnError::from)?;
    Ok(())
}

/// How a parent-keyed delta is confined to a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantScope {
    /// Require the parent to sit in the same tenant as this child row. Emits no
    /// predicate for a child model without a tenant column.
    SameTenantAsChild(i64),
    /// No tenant predicate. Used only by paths that have no child row to scope
    /// against (`recompute`, which sweeps the parent table wholesale).
    Unscoped,
}

/// Which soft-delete state the child row must be in for a by-id delta to apply.
#[doc(hidden)]
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
#[doc(hidden)]
pub async fn counter_cache_after_insert<M: Send + Sync + 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    record: &M,
) -> AutumnResult<()> {
    for spec in specs {
        if let Some(parent_id) = (spec.fk_of)(record) {
            counter_cache_apply_delta(
                conn,
                spec,
                parent_id,
                1,
                TenantScope::SameTenantAsChild((spec.pk_of)(record)),
            )
            .await?;
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
#[doc(hidden)]
pub async fn counter_cache_after_insert_many<M: Send + Sync + 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    records: &[M],
) -> AutumnResult<()> {
    for spec in specs {
        let mut contributions: Vec<(i64, i64, i64)> = Vec::new();
        for record in records {
            if let Some(parent_id) = (spec.fk_of)(record) {
                contributions.push((parent_id, 1, (spec.pk_of)(record)));
            }
        }
        fold_or_apply_each(conn, spec, contributions).await?;
    }
    Ok(())
}

/// Apply per-parent deltas, folding only when it is safe to do so.
///
/// Folding a batch into one `UPDATE` per parent is a pure optimization. It is
/// only sound when the association has no tenant column: the tenant predicate is
/// scoped to a single witness child, so a mixed-tenant batch folded behind one
/// arbitrary witness would either sweep cross-tenant children into the increment
/// or drop legitimate ones. With a tenant column each contribution is therefore
/// applied under its own witness.
async fn fold_or_apply_each<M: 'static>(
    conn: &mut RuntimeConnection,
    spec: &CounterCacheSpec<M>,
    contributions: Vec<(i64, i64, i64)>,
) -> AutumnResult<()> {
    if spec.tenant_column.is_some() {
        // Sorted by parent id for the same lock-ordering reason folding is.
        let mut each = contributions;
        each.sort_unstable();
        for (parent_id, delta, witness) in each {
            if delta == 0 {
                continue;
            }
            counter_cache_apply_delta(
                conn,
                spec,
                parent_id,
                delta,
                TenantScope::SameTenantAsChild(witness),
            )
            .await?;
        }
        return Ok(());
    }
    let mut folded: HashMap<i64, Fold> = HashMap::new();
    for (parent_id, delta, witness) in contributions {
        folded
            .entry(parent_id)
            .or_insert_with(|| Fold::new(witness))
            .delta += delta;
    }
    apply_folded(conn, spec, folded).await
}

/// One parent's accumulated delta plus a witness child id.
///
/// The tenant predicate needs *a* child row to scope against; any child that
/// contributed to this parent's delta is a valid witness, since children of one
/// parent necessarily share its tenant (that is the invariant being enforced).
struct Fold {
    delta: i64,
    witness: i64,
}

impl Fold {
    const fn new(witness: i64) -> Self {
        Self { delta: 0, witness }
    }
}

/// Apply a folded `parent -> delta` map in ascending parent id.
///
/// Ascending order is what stops two concurrent batches touching the same
/// parents from taking their row locks in opposite orders and deadlocking.
/// Zero deltas are dropped rather than issued as `+ 0`.
///
/// **Only used when the association declares no tenant column.** The tenant
/// predicate is scoped to one witness child, so folding a mixed-tenant batch
/// behind a single arbitrary witness would either sweep cross-tenant children
/// into the increment or drop legitimate ones. Where a tenant column exists the
/// callers apply per child instead — see [`fold_or_apply_each`].
async fn apply_folded<M: 'static>(
    conn: &mut RuntimeConnection,
    spec: &CounterCacheSpec<M>,
    deltas: HashMap<i64, Fold>,
) -> AutumnResult<()> {
    let mut parents: Vec<(i64, i64, i64)> = deltas
        .into_iter()
        .filter(|(_, fold)| fold.delta != 0)
        .map(|(parent_id, fold)| (parent_id, fold.delta, fold.witness))
        .collect();
    parents.sort_unstable();
    for (parent_id, delta, witness) in parents {
        counter_cache_apply_delta(
            conn,
            spec,
            parent_id,
            delta,
            TenantScope::SameTenantAsChild(witness),
        )
        .await?;
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
#[doc(hidden)]
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
        // Both sub-selects correlate on the parent, so the tenant check is a
        // plain column comparison inside each — no extra round trip, and a
        // cross-tenant child contributes to neither the count nor the row set.
        let tenant = tenant_predicate_joined(spec);
        let sql = format!(
            "UPDATE {parent_table} SET {counter_column} = {counter_column} - \
             (SELECT COUNT(*) FROM {child_table} AS {CHILD_ALIAS} \
              WHERE {CHILD_ALIAS}.{fk_column} = {parent_table}.{parent_pk} \
                AND {CHILD_ALIAS}.{child_pk} IN ({id_list}){live}{tenant}) \
             WHERE {parent_table}.{parent_pk} IN \
               (SELECT {CHILD_ALIAS}.{fk_column} FROM {child_table} AS {CHILD_ALIAS} \
                WHERE {CHILD_ALIAS}.{child_pk} IN ({id_list}) \
                  AND {CHILD_ALIAS}.{fk_column} IS NOT NULL{live} \
                  AND {CHILD_ALIAS}.{fk_column} = {parent_table}.{parent_pk}{tenant})"
        );
        diesel::sql_query(sql)
            .execute(conn)
            .await
            .map_err(AutumnError::from)?;
    }
    Ok(())
}

/// [`counter_cache_before_delete_many`] restricted to the ONE leg whose foreign
/// key is about to be cleared (`dependent = nullify`).
///
/// A nullify detaches the children from a single parent association; their other
/// counter-cached legs are untouched, so decrementing every spec would
/// permanently undercount them. For example, nullifying `comments.author_id`
/// when a user is deleted must not drop `posts.comment_count` for comments that
/// remain attached to their post.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
#[doc(hidden)]
pub async fn counter_cache_before_detach_many<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    fk_column: &str,
    child_ids: &[i64],
) -> AutumnResult<()> {
    let detached: Vec<CounterCacheSpec<M>> = specs
        .iter()
        .filter(|spec| spec.fk_column == fk_column)
        .copied()
        .collect();
    counter_cache_before_delete_many(conn, &detached, child_ids).await
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
#[doc(hidden)]
pub async fn counter_cache_before_restore_by_id<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_id: i64,
) -> AutumnResult<()> {
    for spec in specs {
        // A model with no `deleted_at` has no restore path; guard anyway so a
        // mis-wired call cannot double-increment.
        if spec.child_soft_delete {
            counter_cache_apply_delta_by_child_id(conn, spec, child_id, 1, ChildState::SoftDeleted)
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
#[doc(hidden)]
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
        // A soft-deleted child is counted by nobody, so it has no "old parent"
        // to move away from — reporting one would make a later re-parent
        // decrement a counter that had already dropped this row.
        let live = live_predicate(spec, true);
        let sql = format!(
            "SELECT {CHILD_ALIAS}.{fk_column} AS fk_value \
             FROM {child_table} AS {CHILD_ALIAS} \
             WHERE {CHILD_ALIAS}.{child_pk} = {PH1}{live}{FOR_UPDATE}"
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
#[doc(hidden)]
pub async fn counter_cache_after_update<M: Send + Sync + 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    before: &[Option<i64>],
    record: &M,
) -> AutumnResult<()> {
    for (index, spec) in specs.iter().enumerate() {
        let old = before.get(index).copied().flatten();
        // A soft-deleted child is counted by nobody, so neither its old nor its
        // new parent may move. The generated `update` does not filter
        // soft-deleted rows, so this is reachable.
        let new = if (spec.live_of)(record) {
            (spec.fk_of)(record)
        } else {
            None
        };
        if old == new {
            continue;
        }
        // Applied in ascending parent id, not old-then-new: two transactions
        // swapping children between parents A and B would otherwise take the two
        // row locks in opposite orders and deadlock.
        let witness = (spec.pk_of)(record);
        let mut moves: Vec<(i64, i64, i64)> = Vec::with_capacity(2);
        if let Some(old_id) = old {
            moves.push((old_id, -1, witness));
        }
        if let Some(new_id) = new {
            moves.push((new_id, 1, witness));
        }
        fold_or_apply_each(conn, spec, moves).await?;
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
#[doc(hidden)]
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
        let live = live_predicate(spec, true);
        let sql = format!(
            "SELECT {CHILD_ALIAS}.{child_pk} AS child_id, \
             {CHILD_ALIAS}.{fk_column} AS fk_value \
             FROM {child_table} AS {CHILD_ALIAS} \
             WHERE {CHILD_ALIAS}.{child_pk} IN ({id_list}){live} \
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
#[doc(hidden)]
pub async fn counter_cache_after_update_many<M: Send + Sync + 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    before: &[(i64, Vec<Option<i64>>)],
    records: &[M],
) -> AutumnResult<()> {
    if specs.is_empty() {
        return Ok(());
    }
    let pk_of = specs[0].pk_of;
    for (index, spec) in specs.iter().enumerate() {
        // Fold the WHOLE batch into one delta per parent before issuing any
        // statement. Re-parenting 1000 children from one post to another is then
        // two `UPDATE`s (`-1000`, `+1000`) rather than 2000, and — because the
        // folded deltas are applied in ascending parent id — two concurrent
        // batches swapping children between the same parents take their row
        // locks in the same order and cannot deadlock.
        let mut contributions: Vec<(i64, i64, i64)> = Vec::new();
        for record in records {
            let child_id = pk_of(record);
            let Ok(found) = before.binary_search_by_key(&child_id, |(id, _)| *id) else {
                // No captured "before" for this row: it appeared between the
                // capture and the update. Guessing would be worse than leaving
                // it to `recompute`.
                continue;
            };
            let old = before[found].1.get(index).copied().flatten();
            // A soft-deleted child is counted by nobody, so neither its old nor
            // its new parent may move.
            let new = if (spec.live_of)(record) {
                (spec.fk_of)(record)
            } else {
                None
            };
            if old == new {
                continue;
            }
            if let Some(old_id) = old {
                contributions.push((old_id, -1, child_id));
            }
            if let Some(new_id) = new {
                contributions.push((new_id, 1, child_id));
            }
        }
        fold_or_apply_each(conn, spec, contributions).await?;
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
#[doc(hidden)]
pub async fn counter_cache_after_upsert_many<M: Send + Sync + 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    existing: &[M],
    upserted: &[M],
) -> AutumnResult<()> {
    if specs.is_empty() {
        return Ok(());
    }
    let pk_of = specs[0].pk_of;
    // A soft-deleted existing row is counted by nobody, so it has no "old
    // parent". Recording one would make the post-upsert side — which now checks
    // `live_of` and yields `None` for a row that stays deleted — decrement a
    // parent that had already dropped it.
    let before: HashMap<i64, Vec<Option<i64>>> = existing
        .iter()
        .map(|row| {
            let live = specs.first().is_none_or(|spec| (spec.live_of)(row));
            (
                pk_of(row),
                specs
                    .iter()
                    .map(|spec| if live { (spec.fk_of)(row) } else { None })
                    .collect(),
            )
        })
        .collect();
    // One folded pass per leg over the whole chunk, inserts and updates together:
    // a row absent from `before` is an insert (`+1`), a row present in it is an
    // update (the before/after diff). Same folding and same ascending-parent-id
    // ordering as the other bulk paths, for the same two reasons.
    for (index, spec) in specs.iter().enumerate() {
        let mut contributions: Vec<(i64, i64, i64)> = Vec::new();
        for record in upserted {
            let child_id = pk_of(record);
            let new = if (spec.live_of)(record) {
                (spec.fk_of)(record)
            } else {
                None
            };
            match before.get(&child_id) {
                None => {
                    if let Some(parent_id) = new {
                        contributions.push((parent_id, 1, child_id));
                    }
                }
                Some(old_fks) => {
                    let old = old_fks.get(index).copied().flatten();
                    if old == new {
                        continue;
                    }
                    if let Some(old_id) = old {
                        contributions.push((old_id, -1, child_id));
                    }
                    if let Some(new_id) = new {
                        contributions.push((new_id, 1, child_id));
                    }
                }
            }
        }
        fold_or_apply_each(conn, spec, contributions).await?;
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
/// Returns the number of parent rows **actually repaired**, summed across every
/// spec — a sweep over a table with no drift returns 0 and writes nothing.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
#[doc(hidden)]
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
        // The ground truth has to agree with what the deltas maintain: an
        // ordinary delta skips a cross-tenant child, so a recompute that counted
        // it would undo the isolation on the very next sweep.
        let tenant = tenant_predicate_joined(spec);
        // The correlated sub-select aliases the child table so a self-referential
        // counter cache (child table == parent table) still binds the outer
        // `UPDATE` target on the right-hand side of the join predicate.
        let ground_truth = format!(
            "(SELECT COUNT(*) FROM {child_table} AS {CHILD_ALIAS} \
              WHERE {CHILD_ALIAS}.{fk_column} = {parent_table}.{parent_pk}{live}{tenant})"
        );
        // `IS DISTINCT FROM` so a sweep over a healthy table writes nothing:
        // under MVCC an unconditional assignment would rewrite every parent row
        // (bloat proportional to the whole table, for no change), and it would
        // make the returned count the row count rather than the repair count.
        let mut sql = format!(
            "UPDATE {parent_table} SET {counter_column} = {ground_truth} \
             WHERE {parent_table}.{counter_column} {IS_DISTINCT_FROM} {ground_truth}"
        );
        if parent_id.is_some() {
            let _ = write!(sql, " AND {parent_table}.{parent_pk} = {PH1}");
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
            live_of: |_| true,
            tenant_column: None,
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
