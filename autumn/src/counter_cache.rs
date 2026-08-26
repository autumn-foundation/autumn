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
//!
//! On top of that, every interpolated identifier is [quoted](quote_ident) — the
//! same convention the `dependent(restrict)` codegen already follows. That is
//! what lets a counter column be named after a SQL keyword
//! (`counter_cache = "order"`) without turning every generated statement into a
//! syntax error, and it is safe precisely because the validation above rules out
//! the `"` that would otherwise escape the quotes.

use std::collections::HashMap;
use std::fmt::Write as _;

use diesel::sql_types::{BigInt, Nullable};
use diesel_async::RunQueryDsl as _;
use scoped_futures::ScopedFutureExt as _;

use crate::db::{RuntimeConnection, scoped_immediate_transaction};
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

/// Its negation, for matching a tenant discriminator that may be NULL. Plain `=`
/// would make two rows that are both untenanted fail to match, which for a
/// counter cache means the maintenance silently does nothing — the one failure
/// mode this module works hardest to avoid.
#[cfg(not(feature = "sqlite"))]
const IS_NOT_DISTINCT_FROM: &str = "IS NOT DISTINCT FROM";
#[cfg(feature = "sqlite")]
const IS_NOT_DISTINCT_FROM: &str = "IS";

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
/// `pub` (inside this `pub(crate)` module, so still crate-private) because
/// [`crate::commentable`] (#1367) builds its polymorphic
/// statements to the same identifier-safety contract and must apply the same
/// backstop — one definition, so the two can never drift apart.
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

/// Wrap `ident` in the SQL identifier quotes both backends accept.
///
/// Quoting is what keeps a counter column named after a SQL keyword
/// (`counter_cache = "order"`) from turning every generated statement into a
/// syntax error, and it matches how the `dependent(restrict)` codegen already
/// interpolates table and column names. Safe to apply unconditionally: the names
/// are validated plain identifiers, so none of them contains the `"` that would
/// otherwise let a hand-built spec escape the quotes.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{ident}\"")
}

/// Every identifier a spec splices into SQL, quoted.
///
/// Field names match [`CounterCacheSpec`]'s so the statement builders below can
/// destructure this in place of the spec and leave their SQL untouched.
struct Quoted {
    child_table: String,
    child_pk: String,
    fk_column: String,
    parent_table: String,
    parent_pk: String,
    counter_column: String,
}

fn quoted<M: 'static>(spec: &CounterCacheSpec<M>) -> Quoted {
    Quoted {
        child_table: quote_ident(spec.child_table),
        child_pk: quote_ident(spec.child_pk),
        fk_column: quote_ident(spec.fk_column),
        parent_table: quote_ident(spec.parent_table),
        parent_pk: quote_ident(spec.parent_pk),
        counter_column: quote_ident(spec.counter_column),
    }
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
/// `AND EXISTS (SELECT 1 FROM <child> x WHERE x.<pk> = N AND <parent>.tenant IS
/// NOT DISTINCT FROM x.tenant)` for the parent-keyed statements, which have no
/// child row to join, empty otherwise. The `EXISTS` (rather than a scalar
/// sub-select) keeps the statement a no-op when the child row is absent, which
/// is what a missing child has always meant here.
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
    let tenant_column = quote_ident(tenant_column);
    let parent_table = quote_ident(spec.parent_table);
    format!(
        " AND {parent_table}.{tenant_column} {IS_NOT_DISTINCT_FROM} \
         {CHILD_ALIAS}.{tenant_column}"
    )
}

fn tenant_predicate<M: 'static>(spec: &CounterCacheSpec<M>, child_id: i64) -> String {
    let Some(tenant_column) = spec.tenant_column else {
        return String::new();
    };
    let tenant_column = quote_ident(tenant_column);
    let Quoted {
        child_table,
        child_pk,
        parent_table,
        ..
    } = quoted(spec);
    format!(
        " AND EXISTS \
         (SELECT 1 FROM {child_table} AS {CHILD_ALIAS}_t \
          WHERE {CHILD_ALIAS}_t.{child_pk} = {child_id} \
            AND {parent_table}.{tenant_column} {IS_NOT_DISTINCT_FROM} \
                {CHILD_ALIAS}_t.{tenant_column})"
    )
}

/// `AND __autumn_cc_child.deleted_at IS NULL` (or `IS NOT NULL`) for a
/// soft-deleting child, empty for a child with no `deleted_at` column.
fn live_predicate<M: 'static>(spec: &CounterCacheSpec<M>, want_live: bool) -> String {
    if !spec.child_soft_delete {
        return String::new();
    }
    let op = if want_live { "IS NULL" } else { "IS NOT NULL" };
    let deleted_at = quote_ident(DELETED_AT);
    format!(" AND {CHILD_ALIAS}.{deleted_at} {op}")
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
    let Quoted {
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = quoted(spec);
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
    let Quoted {
        child_table,
        child_pk,
        fk_column,
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = quoted(spec);
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
/// A record that arrives already soft-deleted moves nothing: the counter is
/// defined as live rows only, and every other path agrees on that, so counting a
/// born-deleted row would inflate the counter until the next repair. An ordinary
/// insert is live, so this changes nothing for it.
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
    let mut contributions: Vec<Contribution> = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        if !(spec.live_of)(record) {
            continue;
        }
        if let Some(parent_id) = (spec.fk_of)(record) {
            contributions.push((index, parent_id, 1, (spec.pk_of)(record)));
        }
    }
    apply_ordered(conn, specs, contributions).await
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
    let mut contributions: Vec<Contribution> = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        for record in records {
            if !(spec.live_of)(record) {
                continue;
            }
            if let Some(parent_id) = (spec.fk_of)(record) {
                contributions.push((index, parent_id, 1, (spec.pk_of)(record)));
            }
        }
    }
    apply_ordered(conn, specs, contributions).await
}

/// One parent row a mutation will move: `(spec index, parent id, delta,
/// witness child id)`.
type Contribution = (usize, i64, i64, i64);

/// Apply every contribution in the lock order all counter-cached mutations
/// agree on: `(parent_table, parent_id)`.
///
/// Ordering by parent id alone is not enough once a mutation moves more than one
/// leg. Two child models that declare their legs in opposite orders — one
/// `belongs_to(User)` then `belongs_to(Post)`, the other the reverse — would
/// take the same two row locks in opposite orders and deadlock, and generated
/// repository transactions are single-attempt, so one request fails outright.
/// Keying on the parent *table* as well makes the order a property of the schema
/// rather than of any one model's declaration order, so every writer in the
/// process agrees on it — including two legs that point at the same table, which
/// a per-spec ordering cannot reconcile.
///
/// Folding several children of one parent into a single `+ n` is a pure
/// optimization: re-parenting 1000 children becomes two statements rather than
/// 2000. It is only sound when the association has no tenant column, since the
/// tenant predicate is scoped to a single witness child and a mixed-tenant batch
/// folded behind one arbitrary witness would either sweep cross-tenant children
/// into the increment or drop legitimate ones. Tenant-scoped specs therefore
/// keep one statement per contribution — in the same global order.
async fn apply_ordered<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    contributions: Vec<Contribution>,
) -> AutumnResult<()> {
    if contributions.is_empty() {
        return Ok(());
    }
    for (spec_index, parent_id, delta, witness) in fold_and_order(specs, contributions) {
        counter_cache_apply_delta(
            conn,
            &specs[spec_index],
            parent_id,
            delta,
            TenantScope::SameTenantAsChild(witness),
        )
        .await?;
    }
    Ok(())
}

/// The pure half of [`apply_ordered`]: fold, drop the no-ops, and put what is
/// left in the global lock order. Split out so the ordering is unit-testable
/// without a database.
fn fold_and_order<M: 'static>(
    specs: &[CounterCacheSpec<M>],
    contributions: Vec<Contribution>,
) -> Vec<Contribution> {
    let mut folded: Vec<Contribution> = Vec::with_capacity(contributions.len());
    // `(spec, parent)` -> where that parent's running total lives in `folded`.
    let mut seen: HashMap<(usize, i64), usize> = HashMap::new();
    for (spec_index, parent_id, delta, witness) in contributions {
        if specs[spec_index].tenant_column.is_some() {
            folded.push((spec_index, parent_id, delta, witness));
            continue;
        }
        if let Some(&at) = seen.get(&(spec_index, parent_id)) {
            folded[at].2 += delta;
        } else {
            seen.insert((spec_index, parent_id), folded.len());
            folded.push((spec_index, parent_id, delta, witness));
        }
    }
    // Zero deltas are dropped rather than issued as `+ 0`.
    folded.retain(|&(_, _, delta, _)| delta != 0);
    folded
        .sort_by_key(|&(spec_index, parent_id, _, _)| (specs[spec_index].parent_table, parent_id));
    folded
}

/// Spec indices in the same global order, for the paths that resolve the parent
/// inside the statement and so cannot key on its id.
///
/// This removes every cross-table cycle, which is the half these paths can
/// reach: a by-id call touches one parent per leg, so two legs onto *different*
/// tables are ordered here, and two legs onto the *same* table are the residual
/// (`recompute` is the repair, and a deadlock there surfaces as an error rather
/// than as drift). `counter_column` breaks ties so the order is total.
fn specs_in_lock_order<M: 'static>(specs: &[CounterCacheSpec<M>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..specs.len()).collect();
    order.sort_by_key(|&i| (specs[i].parent_table, specs[i].counter_column));
    order
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
/// For a soft-deleting child the increment is conditional on the row being live,
/// mirroring the delete side. Raw SQL can insert a row with `deleted_at` already
/// set, and every other path — delete, update, recompute — defines the counter as
/// live rows only, so incrementing for a born-deleted row would inflate the
/// counter until the next repair.
///
/// # Errors
///
/// Propagates any database error from the `UPDATE`s.
pub async fn counter_cache_after_insert_by_id<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_id: i64,
) -> AutumnResult<()> {
    for index in specs_in_lock_order(specs) {
        let spec = &specs[index];
        let state = if spec.child_soft_delete {
            ChildState::Live
        } else {
            ChildState::Any
        };
        counter_cache_apply_delta_by_child_id(conn, spec, child_id, 1, state).await?;
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
    for index in specs_in_lock_order(specs) {
        let spec = &specs[index];
        let state = if spec.child_soft_delete {
            ChildState::Live
        } else {
            ChildState::Any
        };
        counter_cache_apply_delta_by_child_id(conn, spec, child_id, -1, state).await?;
    }
    Ok(())
}

/// Take the child row locks a bulk decrement needs, in ascending id order.
///
/// The returned rows are discarded — the statement exists for its locks (see
/// [`counter_cache_before_delete_many`]'s lock-order note).
fn child_lock_sql<M: 'static>(spec: &CounterCacheSpec<M>, id_list: &str) -> String {
    let Quoted {
        child_table,
        child_pk,
        ..
    } = quoted(spec);
    format!(
        "SELECT {child_pk} AS id FROM {child_table} \
         WHERE {child_pk} IN ({id_list}) ORDER BY {child_pk}{FOR_UPDATE}"
    )
}

/// [`counter_cache_before_delete_by_id`] over a batch of ids (`delete_many`).
///
/// Issues **one** statement per spec regardless of batch size: each affected
/// parent's counter drops by the number of its children in the batch, computed
/// by the database. As with the single-row form, this must run *before* the rows
/// are deleted, and for a soft-deleting model it counts only rows that are still
/// live so a repeated bulk delete moves nothing.
///
/// # Lock order
///
/// The children are locked in ascending id order **before** any parent counter
/// is touched. Every other path in this module reaches the parent through a
/// child row it has already locked (`counter_cache_apply_delta_by_child_id`
/// carries `FOR UPDATE` on its sub-select; the update paths lock the child while
/// capturing its foreign keys), so the module's lock order is uniformly
/// child-then-parent. Without this statement the bulk path would invert it —
/// locking the parent for the decrement here and the child only later, when the
/// `DELETE` itself runs — and a bulk delete racing an `update` that re-parents
/// one of the same children would deadlock, which generated repository
/// transactions do not retry.
///
/// # Errors
///
/// Propagates any database error from the `SELECT` or the `UPDATE`s.
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

    // One statement, not one per spec: every spec here is a leg of the *same*
    // child model, so they all name the same table and primary key.
    debug_assert_spec_idents(&specs[0]);
    diesel::sql_query(child_lock_sql(&specs[0], &id_list))
        .load::<IdRow>(conn)
        .await
        .map_err(AutumnError::from)?;

    for index in specs_in_lock_order(specs) {
        let spec = &specs[index];
        debug_assert_spec_idents(spec);
        let Quoted {
            child_table,
            child_pk,
            fk_column,
            parent_table,
            parent_pk,
            counter_column,
            ..
        } = quoted(spec);
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
    for index in specs_in_lock_order(specs) {
        let spec = &specs[index];
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
        let Quoted {
            child_table,
            child_pk,
            fk_column,
            ..
        } = quoted(spec);
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
    let mut moves: Vec<Contribution> = Vec::with_capacity(specs.len() * 2);
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
        // Collected rather than applied here, and not old-then-new: every delta
        // this mutation makes goes out in one global lock order (see
        // `apply_ordered`), so two transactions swapping children between the
        // same parents cannot take the two row locks in opposite orders.
        let witness = (spec.pk_of)(record);
        if let Some(old_id) = old {
            moves.push((index, old_id, -1, witness));
        }
        if let Some(new_id) = new {
            moves.push((index, new_id, 1, witness));
        }
    }
    apply_ordered(conn, specs, moves).await
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
        let Quoted {
            child_table,
            child_pk,
            fk_column,
            ..
        } = quoted(spec);
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
    let mut contributions: Vec<Contribution> = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        // Every leg's deltas are collected before any statement runs, then
        // folded and applied in one global lock order (see `apply_ordered`).
        // Re-parenting 1000 children from one post to another is then two
        // `UPDATE`s (`-1000`, `+1000`) rather than 2000, and two concurrent
        // batches swapping children between the same parents take their row
        // locks in the same order and cannot deadlock.
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
                contributions.push((index, old_id, -1, child_id));
            }
            if let Some(new_id) = new {
                contributions.push((index, new_id, 1, child_id));
            }
        }
    }
    apply_ordered(conn, specs, contributions).await
}

/// Move the counters for an `upsert_many` chunk (#1325).
///
/// `existing` is the pre-upsert snapshot the upsert already loaded (and locked)
/// for this chunk, so no extra query is needed: a record present in it is an
/// **update** and gets the before/after foreign-key diff; a record absent from it
/// is an **insert** and gets a plain increment.
///
/// # The one row this classification can get wrong
///
/// The snapshot is loaded `FOR UPDATE`, so every row that *existed* when it ran
/// is locked: it cannot be re-parented underneath this chunk, and its diff is
/// exact. The gap is the row that did **not** exist then — there is nothing to
/// lock — and that another transaction inserts before this chunk's `INSERT …
/// ON CONFLICT` runs. Postgres then updates that row rather than inserting it,
/// while this classifies it as an insert: the `+1` duplicates the increment the
/// other transaction already made, and if the upsert also moved the foreign key,
/// the old parent never gets its decrement.
///
/// Closing it needs the discrimination to come from the statement that did the
/// work (Postgres exposes it as `xmax = 0` in `RETURNING`), because no
/// pre-upsert read can see a row that does not exist yet — row locks do not
/// block inserts, and the advisory lock the *versioned* upsert takes only
/// serializes upserts against each other, not against a plain `save` or a raw
/// insert. It is drift, not corruption, and `recompute` repairs it.
///
/// `SQLite` is unaffected: `BEGIN IMMEDIATE` excludes every other writer for the
/// duration of the transaction, so the window does not exist there.
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
    // One pass per leg over the whole chunk, inserts and updates together: a row
    // absent from `before` is an insert (`+1`), a row present in it is an update
    // (the before/after diff). Folded and ordered by `apply_ordered` like every
    // other bulk path, for the same two reasons.
    let mut contributions: Vec<Contribution> = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
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
                        contributions.push((index, parent_id, 1, child_id));
                    }
                }
                Some(old_fks) => {
                    let old = old_fks.get(index).copied().flatten();
                    if old == new {
                        continue;
                    }
                    if let Some(old_id) = old {
                        contributions.push((index, old_id, -1, child_id));
                    }
                    if let Some(new_id) = new {
                        contributions.push((index, new_id, 1, child_id));
                    }
                }
            }
        }
    }
    apply_ordered(conn, specs, contributions).await
}

/// How many parent rows one recompute transaction locks and repairs at a time.
///
/// A full sweep has to take a row lock on every parent it rebuilds (see
/// [`recompute_batch`]), and locks are held until commit. Rebuilding the whole
/// table in one transaction would therefore block every concurrent write to that
/// table for the duration of the sweep, so the sweep is cut into batches: each
/// one is its own short transaction, and per-parent correctness does not depend
/// on the batches being atomic with each other.
const RECOMPUTE_BATCH: i64 = 1_000;

/// The `UPDATE` that rebuilds `ids`' counters from the source of truth.
fn recompute_update_sql<M: 'static>(spec: &CounterCacheSpec<M>, ids: &str) -> String {
    let Quoted {
        child_table,
        fk_column,
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = quoted(spec);
    let live = live_predicate(spec, true);
    // The ground truth has to agree with what the deltas maintain: an ordinary
    // delta skips a cross-tenant child, so a recompute that counted it would
    // undo the isolation on the very next sweep.
    let tenant = tenant_predicate_joined(spec);
    // The correlated sub-select aliases the child table so a self-referential
    // counter cache (child table == parent table) still binds the outer `UPDATE`
    // target on the right-hand side of the join predicate.
    let ground_truth = format!(
        "(SELECT COUNT(*) FROM {child_table} AS {CHILD_ALIAS} \
          WHERE {CHILD_ALIAS}.{fk_column} = {parent_table}.{parent_pk}{live}{tenant})"
    );
    // `IS DISTINCT FROM` so a sweep over a healthy table writes nothing: under
    // MVCC an unconditional assignment would rewrite every parent row (bloat
    // proportional to the whole table, for no change), and it would make the
    // returned count the row count rather than the repair count.
    format!(
        "UPDATE {parent_table} SET {counter_column} = {ground_truth} \
         WHERE {parent_table}.{parent_pk} IN ({ids}) \
           AND {parent_table}.{counter_column} {IS_DISTINCT_FROM} {ground_truth}"
    )
}

/// Repair one batch of parents: lock them, then rebuild them, in one transaction.
///
/// The lock is what makes a repair safe to run against live traffic. Without it,
/// on Postgres, a repair racing an in-flight child insert **introduces** the
/// drift it exists to remove: the child's transaction has already taken the
/// parent's row lock for its `SET c = c + 1` but has not committed, so the
/// repair's `UPDATE` blocks on that lock, and when it resumes it writes a
/// `COUNT(*)` taken from a snapshot that predates — and therefore cannot see —
/// the child that just committed. The committed `+1` is silently overwritten.
///
/// Taking the lock in a *separate, earlier statement* removes both halves of
/// that race. Either the repair wins the lock, in which case the child's
/// increment is applied after the repair commits and is relative, so it lands on
/// top of the rebuilt value; or the child wins, in which case the locking
/// `SELECT` waits for it to commit and the `UPDATE` that follows takes a fresh
/// snapshot that does see it. Ids are locked in ascending order, matching the
/// order the delta paths apply in, so the two cannot deadlock against each
/// other.
///
/// On `SQLite` the enclosing `BEGIN IMMEDIATE` already excludes every other
/// writer, and `FOR UPDATE` degrades to the empty string; the locking `SELECT`
/// is then a cheap indexed read that keeps this one code path.
async fn recompute_batch<M: 'static>(
    conn: &mut RuntimeConnection,
    spec: &CounterCacheSpec<M>,
    ids: &[i64],
) -> AutumnResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let id_list = id_list(ids);
    let parent_table = quote_ident(spec.parent_table);
    let parent_pk = quote_ident(spec.parent_pk);
    let lock_sql = format!(
        "SELECT {parent_pk} AS id FROM {parent_table} \
         WHERE {parent_pk} IN ({id_list}) ORDER BY {parent_pk}{FOR_UPDATE}"
    );
    let update_sql = recompute_update_sql(spec, &id_list);

    scoped_immediate_transaction::<usize, AutumnError, _>(conn, move |conn| {
        async move {
            diesel::sql_query(lock_sql)
                .load::<IdRow>(&mut *conn)
                .await
                .map_err(AutumnError::from)?;
            diesel::sql_query(update_sql)
                .execute(&mut *conn)
                .await
                .map_err(AutumnError::from)
        }
        .scope_boxed()
    })
    .await
}

/// Recompute counters from the source of truth.
///
/// With `parent_id = None` every parent row is rebuilt; with `Some(id)` only
/// that parent is touched. Idempotent by construction — the column is *assigned*
/// a `COUNT(*)`, never adjusted — so it is safe to run repeatedly, and it is the
/// supported way to adopt a counter column on an existing table (AC6).
///
/// Safe to run against live traffic: each batch locks the parents it is about to
/// rebuild before counting, so it can neither read a half-applied write nor
/// clobber one (see [`recompute_batch`]).
///
/// Returns the number of parent rows **actually repaired**, summed across every
/// spec — a sweep over a table with no drift returns 0 and writes nothing.
///
/// # Errors
///
/// Propagates any database error from the `SELECT`s or `UPDATE`s.
#[doc(hidden)]
pub async fn counter_cache_recompute<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    parent_id: Option<i64>,
) -> AutumnResult<usize> {
    let mut touched = 0usize;
    for index in specs_in_lock_order(specs) {
        let spec = &specs[index];
        debug_assert_spec_idents(spec);
        if let Some(id) = parent_id {
            touched += recompute_batch(conn, spec, &[id]).await?;
            continue;
        }

        // Page over the parent ids *outside* the repair transactions, so no lock
        // is held while enumerating. A parent inserted after its page was read
        // is simply not in this sweep, which is harmless: it starts at the
        // column default and its children are counted by the delta paths.
        let parent_table = quote_ident(spec.parent_table);
        let parent_pk = quote_ident(spec.parent_pk);
        let mut cursor: Option<i64> = None;
        loop {
            let mut page_sql = format!("SELECT {parent_pk} AS id FROM {parent_table}");
            if cursor.is_some() {
                let _ = write!(page_sql, " WHERE {parent_pk} > {PH1}");
            }
            let _ = write!(page_sql, " ORDER BY {parent_pk} LIMIT {RECOMPUTE_BATCH}");
            let query = diesel::sql_query(page_sql);
            let page = if let Some(after) = cursor {
                query.bind::<BigInt, _>(after).load::<IdRow>(conn).await
            } else {
                query.load::<IdRow>(conn).await
            }
            .map_err(AutumnError::from)?;

            let Some(last) = page.last() else { break };
            cursor = Some(last.id);
            let ids: Vec<i64> = page.iter().map(|row| row.id).collect();
            touched += recompute_batch(conn, spec, &ids).await?;
        }
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

/// A single `i64` primary key, aliased `id`.
///
/// Shared by the two statements whose result is incidental: the recompute
/// sweep's page of parent ids, and the bulk-delete child lock (whose rows are
/// discarded — the point is the lock it takes).
#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
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
            format!(" AND {CHILD_ALIAS}.\"deleted_at\" IS NULL")
        );
        assert_eq!(
            live_predicate(&spec(true), false),
            format!(" AND {CHILD_ALIAS}.\"deleted_at\" IS NOT NULL")
        );
    }

    #[test]
    fn a_bulk_decrement_locks_its_children_in_ascending_id_order() {
        // Deterministic order, and children before parents — the same direction
        // the single-row and update paths take. Inverting it here would let a
        // bulk delete deadlock against an `update` that re-parents one of the
        // same children.
        let sql = child_lock_sql(&spec(false), "3,1,2");
        assert!(
            sql.starts_with("SELECT \"id\" AS id FROM \"comments\""),
            "{sql}"
        );
        assert!(sql.contains("WHERE \"id\" IN (3,1,2)"), "{sql}");
        assert!(
            sql.contains(&format!("ORDER BY \"id\"{FOR_UPDATE}")),
            "the lock must be taken in a deterministic order: {sql}"
        );
    }

    #[test]
    fn a_recompute_batch_is_scoped_to_the_ids_it_locked() {
        // Restricting the `UPDATE` to the batch is what makes the locking
        // `SELECT` that precedes it meaningful: a sweep-wide `UPDATE` would
        // touch parents this transaction never locked.
        let sql = recompute_update_sql(&spec(false), "1,2,3");
        assert!(sql.contains("\"posts\".\"id\" IN (1,2,3)"), "{sql}");
        assert!(
            sql.contains(&format!("\"posts\".\"comment_count\" {IS_DISTINCT_FROM}")),
            "a healthy parent must still be left unwritten: {sql}"
        );
    }

    /// Two legs whose parents sit in tables named in the *opposite* order to
    /// their declaration — the shape a second child model would produce if it
    /// declared the same two parents the other way round.
    fn two_legs() -> Vec<CounterCacheSpec<Dummy>> {
        let mut users = spec(false);
        users.parent_table = "users";
        users.counter_column = "sent_count";
        let mut posts = spec(false);
        posts.parent_table = "posts";
        posts.counter_column = "comment_count";
        // Declared users-first; `posts` sorts first.
        vec![users, posts]
    }

    #[test]
    fn deltas_go_out_in_a_globally_stable_lock_order() {
        // The order has to come from the schema, not from either model's
        // declaration order: two child models declaring the same two parents in
        // opposite orders would otherwise take the two row locks in opposite
        // orders and deadlock, and the generated transactions do not retry.
        let specs = two_legs();
        let ordered = fold_and_order(&specs, vec![(0, 5, 1, 100), (1, 9, 1, 100), (1, 2, 1, 100)]);
        let keys: Vec<(&str, i64)> = ordered
            .iter()
            .map(|&(i, parent_id, _, _)| (specs[i].parent_table, parent_id))
            .collect();
        assert_eq!(keys, vec![("posts", 2), ("posts", 9), ("users", 5)]);

        // …and the same for the paths that resolve the parent inside the
        // statement, which can only order by table.
        assert_eq!(specs_in_lock_order(&specs), vec![1, 0]);
    }

    #[test]
    fn contributions_to_one_parent_fold_into_a_single_statement() {
        let specs = two_legs();
        // Re-parenting away from and back to the same parent nets out; the two
        // remaining moves stay one statement each.
        let ordered = fold_and_order(
            &specs,
            vec![
                (1, 2, -1, 10),
                (1, 2, 1, 11),
                (1, 7, 1, 12),
                (1, 7, 1, 13),
                (0, 4, -1, 14),
            ],
        );
        assert_eq!(ordered, vec![(1, 7, 2, 12), (0, 4, -1, 14)]);
    }

    #[test]
    fn a_tenant_scoped_leg_keeps_one_statement_per_child() {
        // Folding behind a single arbitrary witness would either sweep
        // cross-tenant children into the delta or drop legitimate ones, so a
        // tenant-scoped spec stays unfolded — still in the global order.
        let mut specs = two_legs();
        specs[1].tenant_column = Some("tenant_id");
        let ordered = fold_and_order(&specs, vec![(1, 3, 1, 20), (1, 3, 1, 21)]);
        assert_eq!(ordered, vec![(1, 3, 1, 20), (1, 3, 1, 21)]);
    }

    #[test]
    fn a_tenant_discriminator_is_matched_null_safely() {
        // A nullable discriminator with NULL on both sides is the same tenant
        // (namely none), but `=` yields NULL there, so the maintenance would
        // silently do nothing — a counter that quietly stops moving is worse
        // than one that errors.
        let mut tenanted = spec(false);
        tenanted.tenant_column = Some("tenant_id");

        let joined = tenant_predicate_joined(&tenanted);
        assert!(joined.contains(IS_NOT_DISTINCT_FROM), "{joined}");
        assert!(
            !joined.contains("\"tenant_id\" = "),
            "plain equality is not NULL-safe: {joined}"
        );

        // The parent-keyed form still requires the child row to exist, so a
        // missing child remains a no-op rather than matching every untenanted
        // parent the way a scalar sub-select would.
        let keyed = tenant_predicate(&tenanted, 7);
        assert!(keyed.contains("EXISTS"), "{keyed}");
        assert!(keyed.contains(IS_NOT_DISTINCT_FROM), "{keyed}");
        assert!(keyed.contains("\"id\" = 7"), "{keyed}");

        // Still nothing at all for an association that declares no tenant.
        assert_eq!(tenant_predicate_joined(&spec(false)), "");
        assert_eq!(tenant_predicate(&spec(false), 7), "");
    }

    #[test]
    fn a_counter_column_named_after_a_sql_keyword_still_produces_valid_sql() {
        // `counter_cache = "order"` is a legal identifier and a legal column
        // name, but `SET order = order + $1` is a syntax error on both backends.
        // Quoting every interpolated identifier is what keeps the choice of
        // column name from being able to break the generated statements.
        let mut keyword = spec(false);
        keyword.counter_column = "order";
        keyword.parent_table = "group";
        let sql = recompute_update_sql(&keyword, "1");
        assert!(
            sql.starts_with("UPDATE \"group\" SET \"order\" = "),
            "{sql}"
        );
        assert!(sql.contains("\"group\".\"order\" "), "{sql}");
        assert!(
            !sql.contains(" order "),
            "no bare keyword may survive: {sql}"
        );
    }

    #[test]
    fn a_model_without_an_inherent_shadow_resolves_to_the_empty_blanket() {
        // A `const` block, because the whole point of the flag is that it folds
        // at compile time — `assert!` on a constant is a clippy error.
        const { assert!(!Dummy::HAS_COUNTER_CACHES) };
        assert!(Dummy::counter_caches().is_empty());
    }
}
