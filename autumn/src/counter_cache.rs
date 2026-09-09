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

use diesel::sql_types::{BigInt, Nullable, Text};
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
#[cfg(not(feature = "sqlite"))]
const PH3: &str = "$3";
#[cfg(feature = "sqlite")]
const PH1: &str = "?";
#[cfg(feature = "sqlite")]
const PH2: &str = "?";
#[cfg(feature = "sqlite")]
const PH3: &str = "?";

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
    /// Reads the child's tenant discriminator off a record, as text, the way
    /// the database spells `CAST(<tenant> AS TEXT)`.
    ///
    /// `Some` only for a leg with a [`Self::tenant_column`] whose child model
    /// carries that field as an integer or string. Then the pre-update capture
    /// reads the old tenant too, and a child moved to another tenant takes its
    /// contribution off the old parent under the *old* tenant: the live
    /// predicate would look for the parent under the new one and miss it.
    /// `None` keeps the leg's SQL exactly as it was.
    pub tenant_of: Option<fn(&M) -> Option<String>>,
    /// This row's weight in the maintained aggregate. `0` excludes the row.
    ///
    /// A plain counter cache returns `1` for every row; a `#[derivation]`
    /// returns `0` when its filter rejects the row, and otherwise `1` (a count)
    /// or the summed field's value (a sum).
    pub contrib_of: fn(&M) -> i64,
    /// The SQL half of [`Self::contrib_of`], for the set-based paths.
    ///
    /// `"1"` for a counter cache, else a child column reference such as
    /// `{c}."score"`. `{c}` is the placeholder for whichever alias the statement
    /// gives the child table.
    pub contrib_sql: &'static str,
    /// The derivation's row filter, lowered to SQL and already prefixed with
    /// ` AND ` so any builder can concatenate it.
    ///
    /// `""` for a counter cache, which is what keeps a counter cache's
    /// generated SQL byte-identical. Else ` AND (<pred>)`, using the same `{c}`
    /// child-alias placeholder as [`Self::contrib_sql`].
    pub filter_sql: &'static str,
    /// The `#[derivation]` this spec maintains, or `None` for a plain counter
    /// cache. It carries the metadata the backfill and status surfaces read.
    pub derivation: Option<&'static crate::derivation::DerivationDef>,
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

/// Guard that every identifier a view splices into SQL is plain.
///
/// A hard assert, not a debug one. The module doc claims a hand-built spec
/// cannot smuggle SQL, so a release build has to honour that claim too.
///
/// `filter_sql` is deliberately not scanned for SQL syntax. It is a lowered
/// fragment of quoted identifiers, literals, operators and the `{c}` alias
/// placeholder, so a substring scan would reject a legal quoted literal:
/// `filter = status == "a -- b"` carries a comment marker inside a string, and
/// the macro already escapes and validates the literal it lowers. What matters
/// at run time is that no placeholder survives substitution, which
/// [`with_alias`] asserts.
///
/// `contrib_sql` is checked structurally instead of by substring: the lowering
/// only ever produces the literal `1` or one quoted child column.
fn assert_spec_idents(view: &SqlView) {
    assert!(
        is_plain_identifier(view.child_table)
            && is_plain_identifier(view.child_pk)
            && is_plain_identifier(view.fk_column)
            && is_plain_identifier(view.parent_table)
            && is_plain_identifier(view.parent_pk)
            && is_plain_identifier(view.counter_column)
            && view.tenant_column.is_none_or(is_plain_identifier),
        "counter-cache spec carries a non-identifier name; it would be spliced \
         verbatim into SQL"
    );
    assert!(
        is_lowered_contribution(view.contrib_sql),
        "counter-cache spec carries a contribution that is neither `1` nor one \
         child column; it would be spliced verbatim into SQL"
    );
}

/// Whether `sql` is a contribution expression the `#[derivation]` lowering can
/// produce: the literal `1` (a row count), or one plain child column behind the
/// `{c}` alias placeholder (a weighted sum).
fn is_lowered_contribution(sql: &str) -> bool {
    if sql == "1" {
        return true;
    }
    sql.strip_prefix("{c}.\"")
        .and_then(|rest| rest.strip_suffix('"'))
        .is_some_and(is_plain_identifier)
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

/// Everything the statement builders need from a spec, with no model type.
///
/// [`CounterCacheSpec`] is generic over the child model, but no SQL in this
/// module depends on that type, and the derivation repair paths
/// ([`crate::derivation`]) have only a [`crate::derivation::DerivationDef`],
/// never an `M`. Both therefore build their statements from this view, so one
/// set of builders serves both and the two can never emit different SQL.
#[derive(Debug, Clone, Copy)]
pub struct SqlView {
    pub child_table: &'static str,
    pub child_pk: &'static str,
    pub child_soft_delete: bool,
    pub fk_column: &'static str,
    pub parent_table: &'static str,
    pub parent_pk: &'static str,
    pub counter_column: &'static str,
    pub contrib_sql: &'static str,
    pub filter_sql: &'static str,
    pub tenant_column: Option<&'static str>,
}

impl SqlView {
    /// Whether this is a plain counter cache: every live row contributes 1 and
    /// nothing is filtered out. The builders below keep such a view's SQL
    /// byte-identical to what it was before derivations existed.
    const fn is_plain(&self) -> bool {
        self.contrib_sql.len() == 1
            && self.contrib_sql.as_bytes()[0] == b'1'
            && self.filter_sql.is_empty()
    }

    /// Whether the aggregate is a row count rather than a weighted sum.
    const fn counts_rows(&self) -> bool {
        self.contrib_sql.len() == 1 && self.contrib_sql.as_bytes()[0] == b'1'
    }
}

const fn view<M: 'static>(spec: &CounterCacheSpec<M>) -> SqlView {
    SqlView {
        child_table: spec.child_table,
        child_pk: spec.child_pk,
        child_soft_delete: spec.child_soft_delete,
        fk_column: spec.fk_column,
        parent_table: spec.parent_table,
        parent_pk: spec.parent_pk,
        counter_column: spec.counter_column,
        contrib_sql: spec.contrib_sql,
        filter_sql: spec.filter_sql,
        tenant_column: spec.tenant_column,
    }
}

/// Every identifier a view splices into SQL, quoted.
///
/// Field names match [`SqlView`]'s so the statement builders below can
/// destructure this in place of the view and leave their SQL untouched.
struct Quoted {
    child_table: String,
    child_pk: String,
    fk_column: String,
    parent_table: String,
    parent_pk: String,
    counter_column: String,
}

fn quoted(view: &SqlView) -> Quoted {
    Quoted {
        child_table: quote_ident(view.child_table),
        child_pk: quote_ident(view.child_pk),
        fk_column: quote_ident(view.fk_column),
        parent_table: quote_ident(view.parent_table),
        parent_pk: quote_ident(view.parent_pk),
        counter_column: quote_ident(view.counter_column),
    }
}

/// What the `{bin}` placeholder in a lowered string comparison resolves to.
///
/// A derivation filter compares a string field with `==` or `!=`, and the Rust
/// lowering compares bytes. SQL compares by the column's collation, which on a
/// `NOCASE` or otherwise case-folding column would call `"PUB"` and `'pub'`
/// equal where Rust does not, so the record paths and the set-based paths of
/// one derivation would disagree. Pinning the comparison to the backend's
/// bytewise collation makes both sides mean the same thing whatever the column
/// was declared with.
#[cfg(not(feature = "sqlite"))]
const BINARY_COLLATION: &str = "COLLATE \"C\"";
#[cfg(feature = "sqlite")]
const BINARY_COLLATION: &str = "COLLATE BINARY";

/// Resolve the `{c}` child-alias and `{bin}` collation placeholders in a
/// lowered SQL fragment.
///
/// One lowered filter has to serve statements that alias the child table
/// differently (`__autumn_cc_child`, `__autumn_cc_child_t`), so `#[model]`
/// emits the alias as a placeholder and each statement substitutes its own.
/// The collation is a placeholder for the same reason in the other direction:
/// the fragment is lowered once, and which backend runs it is a feature flag.
fn with_alias(sql: &str, alias: &str) -> String {
    let out = sql.replace("{c}", alias).replace("{bin}", BINARY_COLLATION);
    assert!(
        !out.contains('{'),
        "a lowered fragment carries an unresolved placeholder: {out}"
    );
    out
}

/// The derivation's row filter for `alias`, or `""` for a counter cache.
fn filter_predicate(view: &SqlView, alias: &str) -> String {
    with_alias(view.filter_sql, alias)
}

/// One row's contribution expression for `alias`.
fn contrib_expr(view: &SqlView, alias: &str) -> String {
    with_alias(view.contrib_sql, alias)
}

/// The aggregate that folds a set of child rows into the maintained value.
///
/// `COUNT(*)` for a count: the filter is in the surrounding `WHERE`, so every
/// counted row already qualifies. `COALESCE(SUM(...), 0)` for a weighted sum,
/// because `SUM` over an empty set is NULL and the maintained column is not.
fn aggregate_expr(view: &SqlView, alias: &str) -> String {
    if view.counts_rows() {
        "COUNT(*)".to_owned()
    } else {
        format!("COALESCE(SUM({}), 0)", contrib_expr(view, alias))
    }
}

/// `CASE WHEN <filter> THEN <contrib> ELSE 0 END`, for the statements that read
/// a row's contribution as a column.
///
/// The `1 = 1` seed absorbs `filter_sql`'s leading ` AND `, which every other
/// caller concatenates onto a predicate it already has.
///
/// The `CAST` is not cosmetic: this value is *decoded* into an `i64`, and a bare
/// integer literal is `integer` on Postgres, so the driver would refuse the
/// four bytes it got where it expected eight. Casting also normalises a legacy
/// 32-bit summed column to the width the maintained value is read at.
fn contrib_case_expr(view: &SqlView, alias: &str) -> String {
    let contrib = contrib_expr(view, alias);
    if view.filter_sql.is_empty() {
        return format!("CAST({contrib} AS BIGINT)");
    }
    let filter = filter_predicate(view, alias);
    format!("CAST(CASE WHEN 1 = 1{filter} THEN {contrib} ELSE 0 END AS BIGINT)")
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
fn tenant_predicate_joined(view: &SqlView) -> String {
    let Some(tenant_column) = view.tenant_column else {
        return String::new();
    };
    let tenant_column = quote_ident(tenant_column);
    let parent_table = quote_ident(view.parent_table);
    format!(
        " AND {parent_table}.{tenant_column} {IS_NOT_DISTINCT_FROM} \
         {CHILD_ALIAS}.{tenant_column}"
    )
}

/// `AND CAST(<parent>.<tenant> AS TEXT) IS NOT DISTINCT FROM $3`, for a delta
/// scoped by a tenant the pre-update capture read (see
/// [`TenantScope::Captured`]). Both sides are text so one bind serves every
/// tenant column type; `IS NOT DISTINCT FROM` matches a NULL tenant to a NULL
/// bind the way the joined predicate does.
fn tenant_predicate_captured(view: &SqlView) -> String {
    let Some(tenant_column) = view.tenant_column else {
        return String::new();
    };
    let tenant_column = quote_ident(tenant_column);
    let parent_table = quote_ident(view.parent_table);
    format!(" AND CAST({parent_table}.{tenant_column} AS TEXT) {IS_NOT_DISTINCT_FROM} {PH3}")
}

fn tenant_predicate(view: &SqlView, child_id: i64) -> String {
    let Some(tenant_column) = view.tenant_column else {
        return String::new();
    };
    let tenant_column = quote_ident(tenant_column);
    let Quoted {
        child_table,
        child_pk,
        parent_table,
        ..
    } = quoted(view);
    format!(
        " AND EXISTS \
         (SELECT 1 FROM {child_table} AS {CHILD_ALIAS}_t \
          WHERE {CHILD_ALIAS}_t.{child_pk} = {child_id} \
            AND {parent_table}.{tenant_column} {IS_NOT_DISTINCT_FROM} \
                {CHILD_ALIAS}_t.{tenant_column})"
    )
}

/// The left-hand side of a `SET c = <acc> +/- ...` adjustment.
///
/// A plain counter cache reads the column bare, so its SQL stays byte-identical
/// to what it was before derivations existed. A derivation wraps it in
/// `COALESCE(c, 0)`: a derivation is adopted on an existing table, where the
/// column can still be NULL, and `NULL +/- expr` is NULL. One such adjustment
/// would poison the column, and a NULL maintained value is the one outcome no
/// repair can tell apart from legitimate drift.
fn accumulator(view: &SqlView, counter_column: &str) -> String {
    if view.is_plain() {
        counter_column.to_owned()
    } else {
        format!("COALESCE({counter_column}, 0)")
    }
}

/// `AND __autumn_cc_child.deleted_at IS NULL` (or `IS NOT NULL`) for a
/// soft-deleting child, empty for a child with no `deleted_at` column.
fn live_predicate(view: &SqlView, want_live: bool) -> String {
    if !view.child_soft_delete {
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
    let view = view(spec);
    assert_spec_idents(&view);
    let Quoted {
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = quoted(&view);
    let tenant = match &scope {
        TenantScope::SameTenantAsChild(child_id) => tenant_predicate(&view, *child_id),
        TenantScope::Captured(_) => tenant_predicate_captured(&view),
        TenantScope::Unscoped => String::new(),
    };
    let accumulator = accumulator(&view, &counter_column);
    let sql = format!(
        "UPDATE {parent_table} SET {counter_column} = {accumulator} + {PH1} \
         WHERE {parent_table}.{parent_pk} = {PH2}{tenant}"
    );
    let query = diesel::sql_query(sql)
        .bind::<BigInt, _>(delta)
        .bind::<BigInt, _>(parent_id);
    // The captured tenant is a third bind, but only when the leg has a tenant
    // column: without one the predicate is empty and so is the bind list.
    if let (TenantScope::Captured(tenant), true) = (scope, view.tenant_column.is_some()) {
        query
            .bind::<Nullable<Text>, _>(tenant)
            .execute(conn)
            .await
            .map_err(AutumnError::from)?;
        return Ok(());
    }
    query.execute(conn).await.map_err(AutumnError::from)?;
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
/// For a `#[derivation]` the magnitude cannot come from the caller: the weight
/// is a property of the child row, which only the database can see here. So
/// `delta` supplies the **sign** and the statement reads the contribution
/// itself. The filter sits in both sub-selects, so a row the filter rejects
/// matches no parent and the statement is a no-op.
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
    let view = view(spec);
    assert_spec_idents(&view);
    let state_predicate = match child_state {
        ChildState::Any => String::new(),
        ChildState::Live => live_predicate(&view, true),
        ChildState::SoftDeleted => live_predicate(&view, false),
    };
    // The parent is resolved by a sub-select on the child row, so the tenant
    // check is a correlated comparison in the outer `WHERE`. It names the child
    // alias, which is only in scope for a sub-select the outer statement
    // correlates with, so the whole predicate moves into a second sub-select.
    let tenant = tenant_predicate(&view, child_id);

    if !view.is_plain() {
        debug_assert!(
            delta == 1 || delta == -1,
            "a derivation delta carries only a sign; the weight comes from the row"
        );
        let sql =
            weighted_delta_by_child_id_sql(&view, child_id, delta < 0, &state_predicate, &tenant);
        diesel::sql_query(sql)
            .execute(conn)
            .await
            .map_err(AutumnError::from)?;
        return Ok(());
    }

    let Quoted {
        child_table,
        child_pk,
        fk_column,
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = quoted(&view);
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

/// The `#[derivation]` form of [`counter_cache_apply_delta_by_child_id`]: the
/// weight is read from the child row instead of bound by the caller.
///
/// `child_id` is inlined rather than bound, so the two occurrences stay one
/// statement on both backends (`SQLite`'s `?` is positional, so a second bind
/// would be required there and not on Postgres). It is an `i64`, so its decimal
/// rendering cannot contain SQL syntax, the same type-level guarantee
/// [`id_list`] rests on.
///
/// `COALESCE` is belt-and-braces: the outer `WHERE` already restricts the
/// statement to parents whose child row qualifies, so the sub-select cannot be
/// empty. But a NULL in the summed column would still poison the column, and a
/// NULL maintained value is the one outcome no repair can distinguish from
/// legitimate drift.
fn weighted_delta_by_child_id_sql(
    view: &SqlView,
    child_id: i64,
    subtract: bool,
    state_predicate: &str,
    tenant: &str,
) -> String {
    let Quoted {
        child_table,
        child_pk,
        fk_column,
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = quoted(view);
    let filter = filter_predicate(view, CHILD_ALIAS);
    let contrib = contrib_expr(view, CHILD_ALIAS);
    let sign = if subtract { "-" } else { "+" };
    let accumulator = accumulator(view, &counter_column);
    format!(
        "UPDATE {parent_table} SET {counter_column} = {accumulator} {sign} \
         COALESCE((SELECT {contrib} FROM {child_table} AS {CHILD_ALIAS} \
           WHERE {CHILD_ALIAS}.{child_pk} = {child_id} \
             AND {CHILD_ALIAS}.{fk_column} IS NOT NULL{state_predicate}{filter}), 0) \
         WHERE {parent_table}.{parent_pk} IN \
           (SELECT {CHILD_ALIAS}.{fk_column} FROM {child_table} AS {CHILD_ALIAS} \
            WHERE {CHILD_ALIAS}.{child_pk} = {child_id} \
              AND {CHILD_ALIAS}.{fk_column} \
                  IS NOT NULL{state_predicate}{filter}{FOR_UPDATE}){tenant}"
    )
}

/// How a parent-keyed delta is confined to a tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantScope {
    /// Require the parent to sit in the same tenant as this child row. Emits no
    /// predicate for a child model without a tenant column.
    SameTenantAsChild(i64),
    /// Require the parent to sit in this tenant, spelled as the text the
    /// pre-update capture read (`None` for a NULL tenant). Used for the old
    /// side of a child that changed tenant: its row now says the new tenant,
    /// so the live predicate would miss the parent it is leaving. Emits no
    /// predicate for a child model without a tenant column.
    Captured(Option<String>),
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
        // A row the derivation's filter rejects contributes 0, and a 0 delta is
        // no statement at all, not even a `+ 0` write to the parent row.
        let contrib = (spec.contrib_of)(record);
        if contrib == 0 {
            continue;
        }
        if let Some(parent_id) = (spec.fk_of)(record) {
            let scope = TenantScope::SameTenantAsChild((spec.pk_of)(record));
            contributions.push((index, parent_id, contrib, scope));
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
            let contrib = (spec.contrib_of)(record);
            if contrib == 0 {
                continue;
            }
            if let Some(parent_id) = (spec.fk_of)(record) {
                let scope = TenantScope::SameTenantAsChild((spec.pk_of)(record));
                contributions.push((index, parent_id, contrib, scope));
            }
        }
    }
    apply_ordered(conn, specs, contributions).await
}

/// One parent row a mutation will move: `(spec index, parent id, delta, how
/// the parent is confined to a tenant)`.
type Contribution = (usize, i64, i64, TenantScope);

/// One leg's pre-mutation view of a child row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// The parent the row contributes to.
    pub parent: i64,
    /// Its weight in the aggregate. `0` for a row the filter rejects: it *has* a
    /// parent, and it weighs nothing, which is what makes a filter flip on an
    /// unchanged parent visible as a delta.
    pub contrib: i64,
    /// The row's tenant as text, for a leg that reads it (see
    /// [`CounterCacheSpec::tenant_of`]); `None` otherwise, or for a NULL tenant.
    pub tenant: Option<String>,
}

/// One leg's pre-mutation [`Captured`] for a child row.
///
/// `None` when the row contributes to no parent at all: it does not exist, its
/// foreign key is NULL, or it is soft-deleted.
pub type CapturedContribution = Option<Captured>;

/// Every leg's [`CapturedContribution`] for a batch of children, keyed by child
/// primary key and sorted by it, as the bulk update paths consume it.
pub type CapturedContributions = Vec<(i64, Vec<CapturedContribution>)>;

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
    for (spec_index, parent_id, delta, scope) in fold_and_order(specs, contributions) {
        counter_cache_apply_delta(conn, &specs[spec_index], parent_id, delta, scope).await?;
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
    // The running total is an `i128`, so weights that cancel do cancel even
    // when no `i64` could hold an intermediate sum (`[MAX, MAX, -MAX, -MAX]`
    // is zero, not two statements the database would refuse).
    let mut totals: Vec<(usize, i64, i128, TenantScope)> = Vec::with_capacity(contributions.len());
    // `(spec, parent)` -> where that parent's running total lives in `totals`.
    let mut seen: HashMap<(usize, i64), usize> = HashMap::new();
    for (spec_index, parent_id, delta, scope) in contributions {
        if specs[spec_index].tenant_column.is_some() {
            totals.push((spec_index, parent_id, i128::from(delta), scope));
            continue;
        }
        if let Some(&at) = seen.get(&(spec_index, parent_id)) {
            totals[at].2 += i128::from(delta);
        } else {
            seen.insert((spec_index, parent_id), totals.len());
            totals.push((spec_index, parent_id, i128::from(delta), scope));
        }
    }
    // A net total that no single `i64` delta can carry goes out as several
    // deltas of the same sign, so the parent moves monotonically from where it
    // is to where it ends: when both are representable, so is every step.
    // Zero totals are dropped rather than issued as `+ 0`.
    let mut folded: Vec<Contribution> = Vec::with_capacity(totals.len());
    for (spec_index, parent_id, mut total, scope) in totals {
        while total != 0 {
            let chunk = total.clamp(i128::from(i64::MIN), i128::from(i64::MAX));
            #[allow(clippy::cast_possible_truncation)]
            folded.push((spec_index, parent_id, chunk as i64, scope.clone()));
            total -= chunk;
        }
    }
    folded
        .sort_by_key(|&(spec_index, parent_id, _, _)| (specs[spec_index].parent_table, parent_id));
    order_tenant_runs(specs, &mut folded);
    folded
}

/// Order each tenant-scoped parent's deltas so their running sum stays between
/// zero and their total.
///
/// A tenant-scoped leg cannot fold: each delta is guarded by its own child's
/// tenant, and two children of one parent need not share one. What it can do
/// is choose the order. Walking toward the total and stepping back past it
/// keeps every prefix within `[min(0, total), max(0, total)]` whenever the
/// deltas allow it at all, so `[MAX, MAX, -MAX]` goes out as `MAX, -MAX, MAX`
/// and a parent whose start and end both fit never passes through a value
/// that does not. The sort is stable, so the global lock order is unchanged.
fn order_tenant_runs<M: 'static>(specs: &[CounterCacheSpec<M>], folded: &mut [Contribution]) {
    let mut start = 0;
    while start < folded.len() {
        let (spec_index, parent_id) = (folded[start].0, folded[start].1);
        let mut end = start + 1;
        while end < folded.len() && folded[end].0 == spec_index && folded[end].1 == parent_id {
            end += 1;
        }
        if specs[spec_index].tenant_column.is_some() && end - start > 1 {
            let run = &mut folded[start..end];
            let total: i128 = run.iter().map(|(_, _, delta, _)| i128::from(*delta)).sum();
            let mut remaining: Vec<Contribution> = run.to_vec();
            let mut prefix: i128 = 0;
            for slot in run.iter_mut() {
                let toward_total = prefix < total || (prefix == total && total < 0);
                // Ties keep their original (child) order, so equal weights go
                // out as they were captured and the mutation stays
                // deterministic: `min_by_key` takes the first minimum, and the
                // reversed scan makes `max_by_key` take the first maximum.
                let pick = if toward_total {
                    (0..remaining.len()).rev().max_by_key(|&i| remaining[i].2)
                } else {
                    (0..remaining.len()).min_by_key(|&i| remaining[i].2)
                }
                .expect("a run has at least one delta left per slot");
                let next = remaining.remove(pick);
                prefix += i128::from(next.2);
                *slot = next;
            }
        }
        start = end;
    }
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
fn child_lock_sql(view: &SqlView, id_list: &str) -> String {
    let Quoted {
        child_table,
        child_pk,
        ..
    } = quoted(view);
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
    let first = view(&specs[0]);
    assert_spec_idents(&first);
    diesel::sql_query(child_lock_sql(&first, &id_list))
        .load::<IdRow>(conn)
        .await
        .map_err(AutumnError::from)?;

    for index in specs_in_lock_order(specs) {
        let view = view(&specs[index]);
        assert_spec_idents(&view);
        let sql = bulk_decrement_sql(&view, &id_list);
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

/// The one `UPDATE` a bulk decrement issues per leg.
///
/// The maintained value drops by the aggregate of the batch's qualifying child
/// rows, computed by the database in one statement. Both sub-selects carry the
/// derivation filter, so a batch of rows the filter rejects matches no parent
/// and writes nothing, which is also what keeps NULL arithmetic out of reach.
fn bulk_decrement_sql(view: &SqlView, id_list: &str) -> String {
    let Quoted {
        child_table,
        child_pk,
        fk_column,
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = quoted(view);
    let live = if view.child_soft_delete {
        live_predicate(view, true)
    } else {
        String::new()
    };
    let filter = filter_predicate(view, CHILD_ALIAS);
    let aggregate = aggregate_expr(view, CHILD_ALIAS);
    // Both sub-selects correlate on the parent, so the tenant check is a
    // plain column comparison inside each: no extra round trip, and a
    // cross-tenant child contributes to neither the count nor the row set.
    let tenant = tenant_predicate_joined(view);
    let accumulator = accumulator(view, &counter_column);
    format!(
        "UPDATE {parent_table} SET {counter_column} = {accumulator} - \
         (SELECT {aggregate} FROM {child_table} AS {CHILD_ALIAS} \
          WHERE {CHILD_ALIAS}.{fk_column} = {parent_table}.{parent_pk} \
            AND {CHILD_ALIAS}.{child_pk} IN ({id_list}){live}{filter}{tenant}) \
         WHERE {parent_table}.{parent_pk} IN \
           (SELECT {CHILD_ALIAS}.{fk_column} FROM {child_table} AS {CHILD_ALIAS} \
            WHERE {CHILD_ALIAS}.{child_pk} IN ({id_list}) \
              AND {CHILD_ALIAS}.{fk_column} IS NOT NULL{live}{filter} \
              AND {CHILD_ALIAS}.{fk_column} = {parent_table}.{parent_pk}{tenant})"
    )
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

/// Read each counter-cached leg's current foreign key **and contribution** for
/// child `child_id`.
///
/// Called **before** an update so the post-update record can be compared against
/// it; the outer `Option` is `None` when the child row does not exist, has no
/// parent, or is soft-deleted. Issues no statement at all when the model has no
/// counter caches.
///
/// The contribution is read here rather than recomputed later because the
/// pre-update row is what it is a function of, and that row is gone once the
/// `UPDATE` lands. A row the filter rejects reports `0`, which is what makes a
/// filter flip on an unchanged parent a `+1` rather than a no-op.
///
/// # Errors
///
/// Propagates any database error from the `SELECT`s.
#[doc(hidden)]
pub async fn counter_cache_capture_fks<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_id: i64,
) -> AutumnResult<Vec<CapturedContribution>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let view = view(spec);
        assert_spec_idents(&view);
        let Quoted {
            child_table,
            child_pk,
            fk_column,
            ..
        } = quoted(&view);
        // A soft-deleted child is counted by nobody, so it has no "old parent"
        // to move away from — reporting one would make a later re-parent
        // decrement a counter that had already dropped this row.
        let live = live_predicate(&view, true);
        // A leg that reads the tenant captures it as well, as text: the old
        // side of a tenant change is guarded by this value, not by the row's
        // (by then new) tenant.
        if let Some(tenant_column) = captured_tenant_column(spec, &view) {
            let contrib = contrib_case_expr(&view, CHILD_ALIAS);
            let sql = format!(
                "SELECT {CHILD_ALIAS}.{fk_column} AS fk_value, \
                 {contrib} AS contrib_value, \
                 CAST({CHILD_ALIAS}.{tenant_column} AS TEXT) AS tenant_text \
                 FROM {child_table} AS {CHILD_ALIAS} \
                 WHERE {CHILD_ALIAS}.{child_pk} = {PH1}{live}{FOR_UPDATE}"
            );
            let row: Option<FkContribTenantRow> = diesel::sql_query(sql)
                .bind::<BigInt, _>(child_id)
                .get_result::<FkContribTenantRow>(conn)
                .await
                .optional_row()?;
            out.push(row.and_then(FkContribTenantRow::captured));
            continue;
        }
        if view.is_plain() {
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
            out.push(row.and_then(|r| r.fk_value).map(|fk| captured(fk, 1)));
            continue;
        }
        let contrib = contrib_case_expr(&view, CHILD_ALIAS);
        let sql = format!(
            "SELECT {CHILD_ALIAS}.{fk_column} AS fk_value, \
             {contrib} AS contrib_value \
             FROM {child_table} AS {CHILD_ALIAS} \
             WHERE {CHILD_ALIAS}.{child_pk} = {PH1}{live}{FOR_UPDATE}"
        );
        let row: Option<FkContribRow> = diesel::sql_query(sql)
            .bind::<BigInt, _>(child_id)
            .get_result::<FkContribRow>(conn)
            .await
            .optional_row()?;
        out.push(row.and_then(|r| r.fk_value.map(|fk| captured(fk, r.contrib_value))));
    }
    Ok(out)
}

/// A [`Captured`] with no tenant, for the legs that do not read one.
const fn captured(parent: i64, contrib: i64) -> Captured {
    Captured {
        parent,
        contrib,
        tenant: None,
    }
}

/// The quoted tenant column the capture reads, when this leg reads one: it
/// has a tenant column *and* an accessor for the new side to compare against.
/// Without the accessor the capture stays as it was, so a leg that cannot see
/// its tenant costs nothing extra.
fn captured_tenant_column<M: 'static>(
    spec: &CounterCacheSpec<M>,
    view: &SqlView,
) -> Option<String> {
    let tenant_column = view.tenant_column?;
    spec.tenant_of?;
    Some(quote_ident(tenant_column))
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
    before: &[CapturedContribution],
    record: &M,
) -> AutumnResult<()> {
    let mut moves: Vec<Contribution> = Vec::with_capacity(specs.len() * 2);
    for (index, spec) in specs.iter().enumerate() {
        let old = before.get(index).cloned().flatten();
        let new = contribution_of(spec, record);
        // Collected rather than applied here, and not old-then-new: every delta
        // this mutation makes goes out in one global lock order (see
        // `apply_ordered`), so two transactions swapping children between the
        // same parents cannot take the two row locks in opposite orders.
        push_diff(index, old, new, (spec.pk_of)(record), &mut moves);
    }
    apply_ordered(conn, specs, moves).await
}

/// A record's `(parent, contribution)` after a mutation, or `None` when it
/// contributes to no parent.
///
/// A soft-deleted child is counted by nobody, so neither its old nor its new
/// parent may move. The generated `update` does not filter soft-deleted rows, so
/// this is reachable.
fn contribution_of<M: 'static>(spec: &CounterCacheSpec<M>, record: &M) -> CapturedContribution {
    if !(spec.live_of)(record) {
        return None;
    }
    (spec.fk_of)(record).map(|parent| Captured {
        parent,
        contrib: (spec.contrib_of)(record),
        tenant: spec.tenant_of.and_then(|tenant_of| tenant_of(record)),
    })
}

/// Turn one leg's before/after [`Captured`] into deltas.
///
/// Unchanged ⇒ nothing. Same parent and tenant, different weight (a filter
/// flip, or an edited summed field) ⇒ one delta for the difference. Different
/// parent, or different tenant ⇒ the old weight off the old parent and the new
/// weight onto the new one, each skipped when it is 0, so a row the filter
/// rejects never touches a parent row.
///
/// The new side is always scoped by the child row as it now is. The old side
/// is too, unless the tenant changed: then the row no longer says which tenant
/// the old parent sits in, so the removal is scoped by the tenant the capture
/// read, and a parent left behind in the old tenant is found rather than
/// missed.
fn push_diff(
    index: usize,
    old: CapturedContribution,
    new: CapturedContribution,
    witness: i64,
    out: &mut Vec<Contribution>,
) {
    if old == new {
        return;
    }
    let live = TenantScope::SameTenantAsChild(witness);
    let new_tenant = new.as_ref().and_then(|new| new.tenant.clone());
    if let (Some(old), Some(new)) = (&old, &new)
        && old.parent == new.parent
        && old.tenant == new.tenant
    {
        // Two representable weights need not have a representable difference
        // (`i64::MIN` edited to `i64::MAX`). Then the move goes out as the old
        // weight off and the new weight on, which reach the same total.
        if let Some(delta) = new.contrib.checked_sub(old.contrib) {
            if delta != 0 {
                out.push((index, old.parent, delta, live));
            }
            return;
        }
        push_removal(index, old.parent, old.contrib, live.clone(), out);
        out.push((index, new.parent, new.contrib, live));
        return;
    }
    if let Some(old) = old
        && old.contrib != 0
    {
        let scope = if old.tenant == new_tenant {
            live.clone()
        } else {
            TenantScope::Captured(old.tenant)
        };
        push_removal(index, old.parent, old.contrib, scope, out);
    }
    if let Some(new) = new
        && new.contrib != 0
    {
        out.push((index, new.parent, new.contrib, live));
    }
}

/// Take `contrib` off `parent`.
///
/// `-i64::MIN` is not an `i64`, so that one weight leaves as two halves that
/// are. Every other weight negates in place.
fn push_removal(
    index: usize,
    parent: i64,
    contrib: i64,
    scope: TenantScope,
    out: &mut Vec<Contribution>,
) {
    if let Some(negated) = contrib.checked_neg() {
        out.push((index, parent, negated, scope));
    } else {
        let half = contrib / 2;
        out.push((index, parent, -half, scope.clone()));
        out.push((index, parent, -(contrib - half), scope));
    }
}

/// [`counter_cache_capture_fks`] over a batch of child ids (`update_many`).
///
/// Issues one `SELECT` per spec (not per id), returning `(child id,
/// (parent, contribution) in spec order)` for every row found. Rows absent from
/// the table are simply absent from the result.
///
/// # Errors
///
/// Propagates any database error from the `SELECT`s.
#[doc(hidden)]
pub async fn counter_cache_capture_fks_many<M: 'static>(
    conn: &mut RuntimeConnection,
    specs: &[CounterCacheSpec<M>],
    child_ids: &[i64],
) -> AutumnResult<CapturedContributions> {
    if specs.is_empty() || child_ids.is_empty() {
        return Ok(Vec::new());
    }
    let id_list = id_list(child_ids);
    let mut by_child: HashMap<i64, Vec<CapturedContribution>> = HashMap::new();
    for (index, spec) in specs.iter().enumerate() {
        let view = view(spec);
        assert_spec_idents(&view);
        let Quoted {
            child_table,
            child_pk,
            fk_column,
            ..
        } = quoted(&view);
        let live = live_predicate(&view, true);
        if let Some(tenant_column) = captured_tenant_column(spec, &view) {
            let contrib = contrib_case_expr(&view, CHILD_ALIAS);
            let sql = format!(
                "SELECT {CHILD_ALIAS}.{child_pk} AS child_id, \
                 {CHILD_ALIAS}.{fk_column} AS fk_value, \
                 {contrib} AS contrib_value, \
                 CAST({CHILD_ALIAS}.{tenant_column} AS TEXT) AS tenant_text \
                 FROM {child_table} AS {CHILD_ALIAS} \
                 WHERE {CHILD_ALIAS}.{child_pk} IN ({id_list}){live} \
                 ORDER BY {CHILD_ALIAS}.{child_pk}{FOR_UPDATE}"
            );
            let rows: Vec<ChildFkContribTenantRow> = diesel::sql_query(sql)
                .load::<ChildFkContribTenantRow>(conn)
                .await
                .map_err(AutumnError::from)?;
            for row in rows {
                let entry = by_child
                    .entry(row.child_id)
                    .or_insert_with(|| vec![None; specs.len()]);
                entry[index] = row.captured();
            }
            continue;
        }
        if view.is_plain() {
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
                entry[index] = row.fk_value.map(|fk| captured(fk, 1));
            }
            continue;
        }
        let contrib = contrib_case_expr(&view, CHILD_ALIAS);
        let sql = format!(
            "SELECT {CHILD_ALIAS}.{child_pk} AS child_id, \
             {CHILD_ALIAS}.{fk_column} AS fk_value, \
             {contrib} AS contrib_value \
             FROM {child_table} AS {CHILD_ALIAS} \
             WHERE {CHILD_ALIAS}.{child_pk} IN ({id_list}){live} \
             ORDER BY {CHILD_ALIAS}.{child_pk}{FOR_UPDATE}"
        );
        let rows: Vec<ChildFkContribRow> = diesel::sql_query(sql)
            .load::<ChildFkContribRow>(conn)
            .await
            .map_err(AutumnError::from)?;
        for row in rows {
            let entry = by_child
                .entry(row.child_id)
                .or_insert_with(|| vec![None; specs.len()]);
            entry[index] = row.fk_value.map(|fk| captured(fk, row.contrib_value));
        }
    }
    let mut out: CapturedContributions = by_child.into_iter().collect();
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
    before: &[(i64, Vec<CapturedContribution>)],
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
            let old = before[found].1.get(index).cloned().flatten();
            let new = contribution_of(spec, record);
            push_diff(index, old, new, child_id, &mut contributions);
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
    let before: HashMap<i64, Vec<CapturedContribution>> = existing
        .iter()
        .map(|row| {
            (
                pk_of(row),
                specs
                    .iter()
                    .map(|spec| contribution_of(spec, row))
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
            let new = contribution_of(spec, record);
            match before.get(&child_id) {
                None => {
                    if let Some(new) = new
                        && new.contrib != 0
                    {
                        let scope = TenantScope::SameTenantAsChild(child_id);
                        contributions.push((index, new.parent, new.contrib, scope));
                    }
                }
                Some(old_fks) => {
                    let old = old_fks.get(index).cloned().flatten();
                    push_diff(index, old, new, child_id, &mut contributions);
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

/// The maintained value as the source of truth defines it, correlated on the
/// parent row.
///
/// The correlated sub-select aliases the child table so a self-referential
/// counter cache (child table == parent table) still binds the outer statement's
/// target on the right-hand side of the join predicate.
///
/// The ground truth has to agree with what the deltas maintain: an ordinary
/// delta skips a cross-tenant child and a filtered-out row, so a repair that
/// counted either would undo the isolation, or the filter, on the very next
/// sweep.
pub fn ground_truth_sql(view: &SqlView) -> String {
    let Quoted {
        child_table,
        fk_column,
        parent_table,
        parent_pk,
        ..
    } = quoted(view);
    let live = live_predicate(view, true);
    let filter = filter_predicate(view, CHILD_ALIAS);
    let tenant = tenant_predicate_joined(view);
    let aggregate = aggregate_expr(view, CHILD_ALIAS);
    format!(
        "(SELECT {aggregate} FROM {child_table} AS {CHILD_ALIAS} \
          WHERE {CHILD_ALIAS}.{fk_column} = {parent_table}.{parent_pk}{live}{filter}{tenant})"
    )
}

/// The `UPDATE` that rebuilds `ids`' maintained values from the source of truth.
pub fn recompute_update_sql(view: &SqlView, ids: &str) -> String {
    let Quoted {
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = quoted(view);
    let ground_truth = ground_truth_sql(view);
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

/// How many of the newest `limit` parent rows disagree with the source of
/// truth.
///
/// One aggregate statement per derivation, so it is a single round trip. The
/// `LIMIT` bounds the **input**: the `limit` highest-id parents are the window,
/// materialised under the parent table's own name so the correlated ground
/// truth needs no rewriting, and only those rows pay for a correlated
/// aggregate. A `LIMIT` on the matching output alone would not do it: on a
/// healthy table nothing matches, so the engine would evaluate the aggregate
/// for every parent before proving there is nothing left to find. The question
/// an operator asks is "is this derivation drifting", not "by exactly how much
/// on a billion-row table", and newest-first is where live writes, and so live
/// drift, concentrate.
pub fn drift_sql(view: &SqlView, limit: i64) -> String {
    let Quoted {
        parent_table,
        parent_pk,
        counter_column,
        ..
    } = quoted(view);
    let ground_truth = ground_truth_sql(view);
    format!(
        "SELECT COUNT(*) AS count FROM \
         (SELECT 1 AS drifted FROM \
           (SELECT * FROM {parent_table} ORDER BY {parent_table}.{parent_pk} DESC LIMIT {limit}) \
           AS {parent_table} \
          WHERE {parent_table}.{counter_column} {IS_DISTINCT_FROM} {ground_truth}) \
         AS __autumn_cc_drift"
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
async fn recompute_batch(
    conn: &mut RuntimeConnection,
    view: &SqlView,
    ids: &[i64],
) -> AutumnResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let view = *view;
    let ids = ids.to_vec();
    scoped_immediate_transaction::<usize, AutumnError, _>(conn, move |conn| {
        async move { recompute_batch_statements(conn, &view, &ids).await }.scope_boxed()
    })
    .await
}

/// The two statements [`recompute_batch`] runs, without the transaction.
///
/// Split out because the derivation backfill has to commit the repaired batch
/// and its checkpoint **together**: it opens one transaction and runs this plus
/// the checkpoint write inside it. A checkpoint committed separately from the
/// batch it describes would double-apply or skip a batch after a crash.
pub async fn recompute_batch_statements(
    conn: &mut RuntimeConnection,
    view: &SqlView,
    ids: &[i64],
) -> AutumnResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    assert_spec_idents(view);
    let id_list = id_list(ids);
    let parent_table = quote_ident(view.parent_table);
    let parent_pk = quote_ident(view.parent_pk);
    let lock_sql = format!(
        "SELECT {parent_pk} AS id FROM {parent_table} \
         WHERE {parent_pk} IN ({id_list}) ORDER BY {parent_pk}{FOR_UPDATE}"
    );
    diesel::sql_query(lock_sql)
        .load::<IdRow>(&mut *conn)
        .await
        .map_err(AutumnError::from)?;
    diesel::sql_query(recompute_update_sql(view, &id_list))
        .execute(&mut *conn)
        .await
        .map_err(AutumnError::from)
}

/// One page of parent primary keys after `cursor`, in ascending order.
///
/// A parent inserted after its page was read is simply not in this sweep, which
/// is harmless: it starts at the column default and its children are counted by
/// the delta paths.
///
/// [`recompute_view`] reads pages outside its repair transactions, so no lock is
/// held while enumerating. The derivation backfill
/// ([`crate::derivation::run_backfill`]) reads its page **inside** the batch
/// transaction instead, because the cursor it pages from is the checkpoint on
/// the state row that transaction holds the lock on. Reading it outside would
/// mean paging from a cursor another replica had already moved.
pub async fn parent_id_page(
    conn: &mut RuntimeConnection,
    view: &SqlView,
    cursor: Option<i64>,
    limit: i64,
) -> AutumnResult<Vec<i64>> {
    assert_spec_idents(view);
    let parent_table = quote_ident(view.parent_table);
    let parent_pk = quote_ident(view.parent_pk);
    let mut page_sql = format!("SELECT {parent_pk} AS id FROM {parent_table}");
    if cursor.is_some() {
        let _ = write!(page_sql, " WHERE {parent_pk} > {PH1}");
    }
    let _ = write!(page_sql, " ORDER BY {parent_pk} LIMIT {limit}");
    let query = diesel::sql_query(page_sql);
    let page = if let Some(after) = cursor {
        query.bind::<BigInt, _>(after).load::<IdRow>(conn).await
    } else {
        query.load::<IdRow>(conn).await
    }
    .map_err(AutumnError::from)?;
    Ok(page.iter().map(|row| row.id).collect())
}

/// Rebuild one maintained value from the source of truth, one batch at a time.
///
/// The non-generic core of [`counter_cache_recompute`], so the derivation
/// repair path ([`crate::derivation::recompute`]) runs exactly the same sweep.
pub async fn recompute_view(
    conn: &mut RuntimeConnection,
    view: &SqlView,
    parent_id: Option<i64>,
) -> AutumnResult<usize> {
    assert_spec_idents(view);
    if let Some(id) = parent_id {
        return recompute_batch(conn, view, &[id]).await;
    }
    // A self-referential table (a comment's `reply_count`) has rows that are
    // children and parents at once: a batch holding several parents in id
    // order could form a lock cycle with a mutation that holds a child row and
    // wants its parent. One parent per transaction takes one lock and never a
    // second, so the repair cannot be one side of that cycle.
    let batch = if view.child_table == view.parent_table {
        1
    } else {
        RECOMPUTE_BATCH
    };
    let mut touched = 0usize;
    let mut cursor: Option<i64> = None;
    loop {
        let ids = parent_id_page(conn, view, cursor, batch).await?;
        let Some(&last) = ids.last() else { break };
        cursor = Some(last);
        touched += recompute_batch_retrying(conn, view, &ids).await?;
    }
    Ok(touched)
}

/// How many times a repair batch that lost a lock race is retried before the
/// error surfaces. Each batch is its own transaction, so a retry repeats no
/// committed work.
const LOCK_CONTENTION_RETRIES: u32 = 5;

/// [`recompute_batch`], retried when the database aborted the batch to break a
/// lock cycle or a serialisation conflict with a concurrent writer.
async fn recompute_batch_retrying(
    conn: &mut RuntimeConnection,
    view: &SqlView,
    ids: &[i64],
) -> AutumnResult<usize> {
    let mut attempt = 0;
    loop {
        match recompute_batch(conn, view, ids).await {
            Err(error) if attempt < LOCK_CONTENTION_RETRIES && is_lock_contention(&error) => {
                attempt += 1;
                tracing::warn!(
                    parent_table = view.parent_table,
                    column = view.counter_column,
                    attempt,
                    error = %error,
                    "repair batch lost a lock race; retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(25 * u64::from(attempt))).await;
            }
            outcome => return outcome,
        }
    }
}

/// Whether `error` is the database aborting one transaction so that another
/// can proceed: Postgres's deadlock detector (`40P01`) or a serialisation
/// failure (`40001`), and `SQLite`'s busy timeout.
pub fn is_lock_contention(error: &AutumnError) -> bool {
    let message = error.to_string();
    message.contains("deadlock detected")
        || message.contains("could not serialize access")
        || message.contains("database is locked")
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
        touched += recompute_view(conn, &view(&specs[index]), parent_id).await?;
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

/// [`ChildFkRow`] plus the row's contribution, for a derivation's bulk capture.
#[derive(diesel::QueryableByName)]
struct ChildFkContribRow {
    #[diesel(sql_type = BigInt)]
    child_id: i64,
    #[diesel(sql_type = Nullable<BigInt>)]
    fk_value: Option<i64>,
    #[diesel(sql_type = BigInt)]
    contrib_value: i64,
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

/// [`FkRow`] plus the row's contribution, for a derivation's single capture.
#[derive(diesel::QueryableByName)]
struct FkContribRow {
    #[diesel(sql_type = Nullable<BigInt>)]
    fk_value: Option<i64>,
    #[diesel(sql_type = BigInt)]
    contrib_value: i64,
}

/// [`FkContribRow`] plus the row's tenant as text, for a leg that reads it.
#[derive(diesel::QueryableByName)]
struct FkContribTenantRow {
    #[diesel(sql_type = Nullable<BigInt>)]
    fk_value: Option<i64>,
    #[diesel(sql_type = BigInt)]
    contrib_value: i64,
    #[diesel(sql_type = Nullable<Text>)]
    tenant_text: Option<String>,
}

impl FkContribTenantRow {
    fn captured(self) -> CapturedContribution {
        self.fk_value.map(|parent| Captured {
            parent,
            contrib: self.contrib_value,
            tenant: self.tenant_text,
        })
    }
}

/// [`FkContribTenantRow`] keyed by child, for the bulk capture.
#[derive(diesel::QueryableByName)]
struct ChildFkContribTenantRow {
    #[diesel(sql_type = BigInt)]
    child_id: i64,
    #[diesel(sql_type = Nullable<BigInt>)]
    fk_value: Option<i64>,
    #[diesel(sql_type = BigInt)]
    contrib_value: i64,
    #[diesel(sql_type = Nullable<Text>)]
    tenant_text: Option<String>,
}

impl ChildFkContribTenantRow {
    fn captured(self) -> CapturedContribution {
        self.fk_value.map(|parent| Captured {
            parent,
            contrib: self.contrib_value,
            tenant: self.tenant_text,
        })
    }
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
            tenant_of: None,
            contrib_of: |_| 1,
            contrib_sql: "1",
            filter_sql: "",
            derivation: None,
        }
    }

    /// A filtered `sum(score)` derivation over the same tables.
    fn sum_spec() -> CounterCacheSpec<Dummy> {
        CounterCacheSpec {
            counter_column: "visible_score",
            contrib_sql: "{c}.\"score\"",
            filter_sql: " AND ({c}.\"published\" = TRUE)",
            ..spec(false)
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
        assert_eq!(live_predicate(&view(&spec(false)), true), "");
        assert_eq!(
            live_predicate(&view(&spec(true)), true),
            format!(" AND {CHILD_ALIAS}.\"deleted_at\" IS NULL")
        );
        assert_eq!(
            live_predicate(&view(&spec(true)), false),
            format!(" AND {CHILD_ALIAS}.\"deleted_at\" IS NOT NULL")
        );
    }

    #[test]
    fn a_bulk_decrement_locks_its_children_in_ascending_id_order() {
        // Deterministic order, and children before parents — the same direction
        // the single-row and update paths take. Inverting it here would let a
        // bulk delete deadlock against an `update` that re-parents one of the
        // same children.
        let sql = child_lock_sql(&view(&spec(false)), "3,1,2");
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
        let sql = recompute_update_sql(&view(&spec(false)), "1,2,3");
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
        let ordered = fold_and_order(
            &specs,
            vec![
                (0, 5, 1, TenantScope::SameTenantAsChild(100)),
                (1, 9, 1, TenantScope::SameTenantAsChild(100)),
                (1, 2, 1, TenantScope::SameTenantAsChild(100)),
            ],
        );
        let keys: Vec<(&str, i64)> = ordered
            .iter()
            .map(|(i, parent_id, _, _)| (specs[*i].parent_table, *parent_id))
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
                (1, 2, -1, TenantScope::SameTenantAsChild(10)),
                (1, 2, 1, TenantScope::SameTenantAsChild(11)),
                (1, 7, 1, TenantScope::SameTenantAsChild(12)),
                (1, 7, 1, TenantScope::SameTenantAsChild(13)),
                (0, 4, -1, TenantScope::SameTenantAsChild(14)),
            ],
        );
        assert_eq!(
            ordered,
            vec![
                (1, 7, 2, TenantScope::SameTenantAsChild(12)),
                (0, 4, -1, TenantScope::SameTenantAsChild(14))
            ]
        );
    }

    #[test]
    fn a_net_total_no_single_delta_can_carry_goes_out_in_same_sign_pieces() {
        // Two children each weighing `i64::MAX` onto one parent: the sum is
        // not an `i64`, so a wrapping fold would apply `-2`. The net total is
        // carried by two deltas of the same sign instead.
        let specs = two_legs();
        let ordered = fold_and_order(
            &specs,
            vec![
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(10)),
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(11)),
            ],
        );
        assert_eq!(
            ordered,
            vec![
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(10)),
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(10))
            ]
        );
        // …and a total one past `i64::MAX` is `MAX` then `1`, never a wrapped
        // negative: the sum of every delta issued is the sum requested.
        let ordered = fold_and_order(
            &specs,
            vec![
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(10)),
                (1, 2, -5, TenantScope::SameTenantAsChild(11)),
                (1, 2, 5, TenantScope::SameTenantAsChild(12)),
                (1, 2, 1, TenantScope::SameTenantAsChild(13)),
            ],
        );
        assert_eq!(
            ordered,
            vec![
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(10)),
                (1, 2, 1, TenantScope::SameTenantAsChild(10))
            ]
        );
    }

    #[test]
    fn weights_that_cancel_across_an_overflowing_intermediate_issue_nothing() {
        // `[MAX, MAX, -MAX, -MAX]` nets to zero. Segmenting at the overflow
        // point would have issued `+MAX` then `-MAX`, and a parent sitting at
        // `1` would have aborted the whole mutation on the first statement
        // even though its final value is exactly where it started.
        let specs = two_legs();
        let ordered = fold_and_order(
            &specs,
            vec![
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(10)),
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(11)),
                (1, 2, -i64::MAX, TenantScope::SameTenantAsChild(12)),
                (1, 2, -i64::MAX, TenantScope::SameTenantAsChild(13)),
            ],
        );
        assert!(ordered.is_empty(), "{ordered:?}");
        // The same shape netting to a representable value is one delta.
        let ordered = fold_and_order(
            &specs,
            vec![
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(10)),
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(11)),
                (1, 2, -i64::MAX, TenantScope::SameTenantAsChild(12)),
                (1, 2, -7, TenantScope::SameTenantAsChild(13)),
            ],
        );
        assert_eq!(
            ordered,
            vec![(1, 2, i64::MAX - 7, TenantScope::SameTenantAsChild(10))]
        );
    }

    #[test]
    fn a_weight_edit_whose_difference_does_not_fit_goes_out_as_two_deltas() {
        // `i64::MIN` edited to `i64::MAX` on the same parent: the mathematical
        // difference is 2^64 - 1, which is no `i64`, so the old weight comes
        // off and the new one goes on as separate statements. Folding keeps
        // them apart for the same reason.
        let mut out = Vec::new();
        push_diff(
            0,
            Some(captured(7, i64::MIN)),
            Some(captured(7, i64::MAX)),
            1,
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                (0, 7, i64::MAX / 2 + 1, TenantScope::SameTenantAsChild(1)),
                (0, 7, i64::MAX / 2 + 1, TenantScope::SameTenantAsChild(1)),
                (0, 7, i64::MAX, TenantScope::SameTenantAsChild(1))
            ]
        );
        // Folded, the three pieces net to 2^64 - 1 and go out as same-sign
        // deltas: the parent climbs monotonically to its final value.
        let specs = two_legs();
        assert_eq!(
            fold_and_order(&specs, out.clone()),
            vec![
                (0, 7, i64::MAX, TenantScope::SameTenantAsChild(1)),
                (0, 7, i64::MAX, TenantScope::SameTenantAsChild(1)),
                (0, 7, 1, TenantScope::SameTenantAsChild(1))
            ]
        );

        // An ordinary edit is still one delta for the difference.
        let mut out = Vec::new();
        push_diff(0, Some(captured(7, 3)), Some(captured(7, 10)), 1, &mut out);
        assert_eq!(out, vec![(0, 7, 7, TenantScope::SameTenantAsChild(1))]);
    }

    #[test]
    fn removing_the_one_unnegatable_weight_splits_it_in_two() {
        // `-i64::MIN` does not exist. Deleting (or re-parenting) a child that
        // weighs exactly that much takes it off the old parent as two halves
        // whose sum is the whole, rather than wrapping to `i64::MIN` again and
        // moving the parent the wrong way by 2^63.
        let mut out = Vec::new();
        push_diff(0, Some(captured(7, i64::MIN)), None, 1, &mut out);
        assert_eq!(
            out,
            vec![
                (0, 7, i64::MAX / 2 + 1, TenantScope::SameTenantAsChild(1)),
                (0, 7, i64::MAX / 2 + 1, TenantScope::SameTenantAsChild(1))
            ]
        );
        assert_eq!(
            out.iter()
                .map(|(_, _, delta, _)| i128::from(*delta))
                .sum::<i128>(),
            -i128::from(i64::MIN)
        );

        let mut out = Vec::new();
        push_diff(
            0,
            Some(captured(7, i64::MIN)),
            Some(captured(8, 4)),
            1,
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                (0, 7, i64::MAX / 2 + 1, TenantScope::SameTenantAsChild(1)),
                (0, 7, i64::MAX / 2 + 1, TenantScope::SameTenantAsChild(1)),
                (0, 8, 4, TenantScope::SameTenantAsChild(1))
            ]
        );
    }

    #[test]
    fn a_tenant_scoped_run_is_ordered_so_no_prefix_leaves_the_span_of_zero_and_total() {
        // The leg cannot fold (each delta is guarded by its own child's
        // tenant), so the order is what keeps `[MAX, MAX, -MAX]` from passing
        // through `2 * MAX` on the way to its representable total.
        let mut specs = two_legs();
        specs[1].tenant_column = Some("tenant_id");
        let ordered = fold_and_order(
            &specs,
            vec![
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(10)),
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(11)),
                (1, 2, -i64::MAX, TenantScope::SameTenantAsChild(12)),
            ],
        );
        let deltas: Vec<i64> = ordered.iter().map(|(_, _, d, _)| *d).collect();
        assert_eq!(deltas, vec![i64::MAX, -i64::MAX, i64::MAX]);
        // …and symmetrically for a negative total.
        let ordered = fold_and_order(
            &specs,
            vec![
                (1, 2, -i64::MAX, TenantScope::SameTenantAsChild(10)),
                (1, 2, -i64::MAX, TenantScope::SameTenantAsChild(11)),
                (1, 2, i64::MAX, TenantScope::SameTenantAsChild(12)),
            ],
        );
        let deltas: Vec<i64> = ordered.iter().map(|(_, _, d, _)| *d).collect();
        assert_eq!(deltas, vec![-i64::MAX, i64::MAX, -i64::MAX]);
        // Every child keeps its own witness, and other parents are untouched.
        let witnesses: Vec<TenantScope> = ordered.iter().map(|(_, _, _, w)| w.clone()).collect();
        assert_eq!(
            witnesses,
            vec![
                TenantScope::SameTenantAsChild(10),
                TenantScope::SameTenantAsChild(12),
                TenantScope::SameTenantAsChild(11)
            ]
        );
    }

    fn captured_in(parent: i64, contrib: i64, tenant: Option<&str>) -> Captured {
        Captured {
            parent,
            contrib,
            tenant: tenant.map(str::to_owned),
        }
    }

    #[test]
    fn a_tenant_change_takes_the_old_weight_off_under_the_old_tenant() {
        // The row now says the new tenant, so a live predicate would look for
        // the old parent in the wrong tenant and miss it; the removal is
        // scoped by the tenant the capture read. The addition is live.
        let live = TenantScope::SameTenantAsChild(1);
        let was_a = TenantScope::Captured(Some("a".to_owned()));
        let mut out = Vec::new();
        push_diff(
            0,
            Some(captured_in(7, 3, Some("a"))),
            Some(captured_in(7, 3, Some("b"))),
            1,
            &mut out,
        );
        assert_eq!(
            out,
            vec![(0, 7, -3, was_a.clone()), (0, 7, 3, live.clone())]
        );

        // Re-parented and moved at once: the same two statements.
        let mut out = Vec::new();
        push_diff(
            0,
            Some(captured_in(7, 3, Some("a"))),
            Some(captured_in(8, 3, Some("b"))),
            1,
            &mut out,
        );
        assert_eq!(
            out,
            vec![(0, 7, -3, was_a.clone()), (0, 8, 3, live.clone())]
        );

        // A NULL old tenant is a captured NULL, not "unknown".
        let mut out = Vec::new();
        push_diff(
            0,
            Some(captured_in(7, 1, None)),
            Some(captured_in(7, 1, Some("b"))),
            1,
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                (0, 7, -1, TenantScope::Captured(None)),
                (0, 7, 1, live.clone())
            ]
        );

        // Same tenant: the diff is what it always was, one live delta.
        let mut out = Vec::new();
        push_diff(
            0,
            Some(captured_in(7, 3, Some("a"))),
            Some(captured_in(7, 10, Some("a"))),
            1,
            &mut out,
        );
        assert_eq!(out, vec![(0, 7, 7, live)]);

        // Unparented after a tenant change: the removal is still under the old
        // tenant, and nothing is added.
        let mut out = Vec::new();
        push_diff(0, Some(captured_in(7, 3, Some("a"))), None, 1, &mut out);
        assert_eq!(out, vec![(0, 7, -3, was_a)]);
    }

    #[test]
    fn a_captured_tenant_scopes_the_parent_by_its_text() {
        let mut tenanted = spec(false);
        tenanted.tenant_column = Some("tenant_id");
        assert_eq!(
            tenant_predicate_captured(&view(&tenanted)),
            format!(" AND CAST(\"posts\".\"tenant_id\" AS TEXT) {IS_NOT_DISTINCT_FROM} {PH3}")
        );
        // Without a tenant column there is no predicate and no bind.
        assert_eq!(tenant_predicate_captured(&view(&spec(false))), "");
    }

    #[test]
    fn a_tenant_scoped_leg_keeps_one_statement_per_child() {
        // Folding behind a single arbitrary witness would either sweep
        // cross-tenant children into the delta or drop legitimate ones, so a
        // tenant-scoped spec stays unfolded — still in the global order.
        let mut specs = two_legs();
        specs[1].tenant_column = Some("tenant_id");
        let ordered = fold_and_order(
            &specs,
            vec![
                (1, 3, 1, TenantScope::SameTenantAsChild(20)),
                (1, 3, 1, TenantScope::SameTenantAsChild(21)),
            ],
        );
        assert_eq!(
            ordered,
            vec![
                (1, 3, 1, TenantScope::SameTenantAsChild(20)),
                (1, 3, 1, TenantScope::SameTenantAsChild(21))
            ]
        );
    }

    #[test]
    fn a_tenant_discriminator_is_matched_null_safely() {
        // A nullable discriminator with NULL on both sides is the same tenant
        // (namely none), but `=` yields NULL there, so the maintenance would
        // silently do nothing — a counter that quietly stops moving is worse
        // than one that errors.
        let mut tenanted = spec(false);
        tenanted.tenant_column = Some("tenant_id");

        let joined = tenant_predicate_joined(&view(&tenanted));
        assert!(joined.contains(IS_NOT_DISTINCT_FROM), "{joined}");
        assert!(
            !joined.contains("\"tenant_id\" = "),
            "plain equality is not NULL-safe: {joined}"
        );

        // The parent-keyed form still requires the child row to exist, so a
        // missing child remains a no-op rather than matching every untenanted
        // parent the way a scalar sub-select would.
        let keyed = tenant_predicate(&view(&tenanted), 7);
        assert!(keyed.contains("EXISTS"), "{keyed}");
        assert!(keyed.contains(IS_NOT_DISTINCT_FROM), "{keyed}");
        assert!(keyed.contains("\"id\" = 7"), "{keyed}");

        // Still nothing at all for an association that declares no tenant.
        assert_eq!(tenant_predicate_joined(&view(&spec(false))), "");
        assert_eq!(tenant_predicate(&view(&spec(false)), 7), "");
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
        let sql = recompute_update_sql(&view(&keyword), "1");
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
    fn a_plain_counter_cache_keeps_its_pre_derivation_sql() {
        // The whole design rests on this: derivations reuse the counter-cache
        // paths, so a counter cache's own SQL must be untouched by them.
        let plain = view(&spec(false));
        assert!(plain.is_plain());
        assert!(plain.counts_rows());
        assert_eq!(filter_predicate(&plain, CHILD_ALIAS), "");
        assert_eq!(aggregate_expr(&plain, CHILD_ALIAS), "COUNT(*)");
        assert!(
            !recompute_update_sql(&plain, "1").contains("COALESCE"),
            "a plain counter cache must not grow a COALESCE"
        );
        assert_eq!(
            accumulator(&plain, "\"comment_count\""),
            "\"comment_count\"",
            "a plain counter cache reads its column bare"
        );
        assert!(
            !bulk_decrement_sql(&plain, "7").contains("COALESCE"),
            "a plain bulk decrement must not grow a COALESCE"
        );
    }

    #[test]
    fn a_derivation_delta_never_writes_null_into_the_maintained_column() {
        // A derivation is adopted on an existing table, so the column can still
        // be NULL. `NULL +/- expr` is NULL, and a NULL maintained value cannot
        // be told apart from drift, so every adjusting statement coalesces.
        let mut filtered = spec(false);
        filtered.filter_sql = " AND ({c}.\"published\" = TRUE)";
        let filtered = view(&filtered);
        assert_eq!(
            accumulator(&filtered, "\"comment_count\""),
            "COALESCE(\"comment_count\", 0)"
        );
        assert!(
            bulk_decrement_sql(&filtered, "7")
                .contains("SET \"comment_count\" = COALESCE(\"comment_count\", 0) -"),
            "{}",
            bulk_decrement_sql(&filtered, "7")
        );
        let sum = view(&sum_spec());
        assert!(
            weighted_delta_by_child_id_sql(&sum, 1, false, "", "")
                .contains("SET \"visible_score\" = COALESCE(\"visible_score\", 0) +"),
            "the by-id weighted delta coalesces too"
        );
    }

    #[test]
    fn a_lowered_contribution_is_either_one_or_a_single_child_column() {
        // The run-time backstop for `contrib_sql`: a structural check, so a
        // legal quoted literal is never mistaken for smuggled SQL.
        assert!(is_lowered_contribution("1"));
        assert!(is_lowered_contribution("{c}.\"score\""));
        assert!(!is_lowered_contribution("{c}.\"score\" + 1"));
        assert!(!is_lowered_contribution("(SELECT 1)"));
        assert!(!is_lowered_contribution(""));
        assert!(!is_lowered_contribution("{c}.\"a\"; DROP TABLE posts --\""));
    }

    #[test]
    fn a_filter_may_carry_a_comment_marker_inside_a_quoted_literal() {
        // The previous substring scan panicked on this in debug builds: `--`
        // and `;` are inert inside a quoted literal, and the macro escapes the
        // literal it lowers, so only the identifiers and the contribution are
        // checked at run time.
        let mut filtered = spec(false);
        filtered.filter_sql = " AND ({c}.\"status\" = 'a -- b; c')";
        let filtered = view(&filtered);
        assert_spec_idents(&filtered);
        assert!(
            bulk_decrement_sql(&filtered, "7").contains("'a -- b; c'"),
            "the literal reaches the statement unaltered"
        );
    }

    #[test]
    fn a_filtered_count_carries_its_filter_into_every_statement() {
        let mut filtered = spec(false);
        filtered.filter_sql = " AND ({c}.\"published\" = TRUE)";
        let filtered = view(&filtered);
        assert!(
            !filtered.is_plain(),
            "a filter is not a plain counter cache"
        );
        assert!(filtered.counts_rows(), "a filtered count still counts rows");

        // The deltas: the filter sits in both sub-selects, so a rejected row
        // matches no parent and the statement writes nothing.
        let delete = bulk_decrement_sql(&filtered, "7,8");
        assert_eq!(
            delete,
            "UPDATE \"posts\" SET \"comment_count\" = \
             COALESCE(\"comment_count\", 0) - \
             (SELECT COUNT(*) FROM \"comments\" AS __autumn_cc_child \
              WHERE __autumn_cc_child.\"post_id\" = \"posts\".\"id\" \
                AND __autumn_cc_child.\"id\" IN (7,8) \
                AND (__autumn_cc_child.\"published\" = TRUE)) \
             WHERE \"posts\".\"id\" IN \
               (SELECT __autumn_cc_child.\"post_id\" FROM \"comments\" AS __autumn_cc_child \
                WHERE __autumn_cc_child.\"id\" IN (7,8) \
                  AND __autumn_cc_child.\"post_id\" IS NOT NULL \
                  AND (__autumn_cc_child.\"published\" = TRUE) \
                  AND __autumn_cc_child.\"post_id\" = \"posts\".\"id\")"
        );

        // The capture reads the contribution as a column, so a filter flip on an
        // unchanged parent is visible as `0 -> 1`.
        assert_eq!(
            contrib_case_expr(&filtered, CHILD_ALIAS),
            "CAST(CASE WHEN 1 = 1 AND (__autumn_cc_child.\"published\" = TRUE) \
             THEN 1 ELSE 0 END AS BIGINT)"
        );
    }

    #[test]
    fn a_sum_recompute_assigns_the_summed_contribution() {
        let sum = view(&sum_spec());
        assert!(!sum.counts_rows());
        assert_eq!(
            recompute_update_sql(&sum, "1,2"),
            "UPDATE \"posts\" SET \"visible_score\" = \
             (SELECT COALESCE(SUM(__autumn_cc_child.\"score\"), 0) \
              FROM \"comments\" AS __autumn_cc_child \
              WHERE __autumn_cc_child.\"post_id\" = \"posts\".\"id\" \
                AND (__autumn_cc_child.\"published\" = TRUE)) \
             WHERE \"posts\".\"id\" IN (1,2) \
               AND \"posts\".\"visible_score\" "
                .to_owned()
                + IS_DISTINCT_FROM
                + " (SELECT COALESCE(SUM(__autumn_cc_child.\"score\"), 0) \
              FROM \"comments\" AS __autumn_cc_child \
              WHERE __autumn_cc_child.\"post_id\" = \"posts\".\"id\" \
                AND (__autumn_cc_child.\"published\" = TRUE))"
        );
    }

    #[test]
    fn a_weighted_by_id_delta_reads_the_weight_from_the_row() {
        // The magnitude cannot be bound by the caller: only the database can see
        // the row the weight comes from. `COALESCE` keeps a NULL out of the
        // maintained column even so.
        let sql = weighted_delta_by_child_id_sql(&view(&sum_spec()), 9, false, "", "");
        assert!(
            sql.starts_with(
                "UPDATE \"posts\" SET \"visible_score\" = \
                 COALESCE(\"visible_score\", 0) + \
                 COALESCE((SELECT __autumn_cc_child.\"score\""
            ),
            "{sql}"
        );
        assert_eq!(
            sql.matches("__autumn_cc_child.\"id\" = 9").count(),
            2,
            "{sql}"
        );
        assert!(
            sql.contains(&format!(
                "IS NOT NULL AND (__autumn_cc_child.\"published\" = TRUE){FOR_UPDATE})"
            )),
            "{sql}"
        );

        let down = weighted_delta_by_child_id_sql(&view(&sum_spec()), 9, true, "", "");
        assert!(
            down.contains("\"visible_score\" = COALESCE(\"visible_score\", 0) - COALESCE("),
            "{down}"
        );
    }

    #[test]
    fn a_filter_flip_on_the_same_parent_is_one_delta() {
        let mut out = Vec::new();
        // Unpublished (0) -> published (1) with the parent unchanged.
        push_diff(0, Some(captured(4, 0)), Some(captured(4, 1)), 11, &mut out);
        assert_eq!(out, vec![(0, 4, 1, TenantScope::SameTenantAsChild(11))]);

        // A rejected row that stays rejected moves nothing, even across a
        // reparent: neither side has a weight to move.
        out.clear();
        push_diff(0, Some(captured(4, 0)), Some(captured(5, 0)), 11, &mut out);
        assert!(out.is_empty(), "{out:?}");

        // A reparent of a qualifying row moves both ends.
        out.clear();
        push_diff(0, Some(captured(4, 3)), Some(captured(5, 3)), 11, &mut out);
        assert_eq!(
            out,
            vec![
                (0, 4, -3, TenantScope::SameTenantAsChild(11)),
                (0, 5, 3, TenantScope::SameTenantAsChild(11))
            ]
        );

        // A weight edit on an unchanged parent is the difference only.
        out.clear();
        push_diff(0, Some(captured(4, 3)), Some(captured(4, 10)), 11, &mut out);
        assert_eq!(out, vec![(0, 4, 7, TenantScope::SameTenantAsChild(11))]);
    }

    #[test]
    fn drift_is_one_aggregate_over_the_parent_table() {
        let sql = drift_sql(&view(&sum_spec()), 10_000);
        assert!(
            sql.starts_with("SELECT COUNT(*) AS count FROM (SELECT 1 AS drifted FROM "),
            "{sql}"
        );
        assert!(sql.contains(IS_DISTINCT_FROM), "{sql}");
        // The cap is on the parents examined, not on the matches: a healthy
        // table has no match, so an output-side LIMIT would still evaluate the
        // correlated aggregate for every parent before giving up.
        assert!(
            sql.contains(
                "FROM (SELECT * FROM \"posts\" ORDER BY \"posts\".\"id\" DESC LIMIT 10000) AS \"posts\" WHERE"
            ),
            "the window is materialised under the table's own name: {sql}"
        );
        assert!(sql.ends_with(") AS __autumn_cc_drift"), "{sql}");
    }

    #[test]
    fn a_model_without_an_inherent_shadow_resolves_to_the_empty_blanket() {
        // A `const` block, because the whole point of the flag is that it folds
        // at compile time — `assert!` on a constant is a clippy error.
        const { assert!(!Dummy::HAS_COUNTER_CACHES) };
        assert!(Dummy::counter_caches().is_empty());
    }
}
