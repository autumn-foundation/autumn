//! Threaded, **polymorphic** comments (issue #1367).
//!
//! `belongs_to` / `has_many` / `has_one` / `through` all name exactly one
//! parent table in the child's foreign key. Comments do not work that way: one
//! `comments` table wants to attach to posts *and* photos *and* tickets. This
//! module is autumn's fifth association kind — the polymorphic one — keyed on a
//! `(commentable_type, commentable_id)` discriminator pair, with a `parent_id`
//! self-reference for threading.
//!
//! Declaring it is one attribute:
//!
//! ```rust,ignore
//! #[autumn_web::model]
//! #[commentable(by = User, author_name = username)]
//! pub struct Post { /* … `comment_count: i64` … */ }
//! ```
//!
//! which emits `Post::COMMENTABLE_TYPE`, `Post::commentable_spec()`, a
//! `PostComments` trait (`add_comment` / `comment_thread` / `delete_comment`)
//! blanket-implemented over the generated repository, and an [`inventory`]
//! registration so [`router()`] can serve the thread for *any* commentable model
//! without the app writing a route per model.
//!
//! # Why there is no foreign key on `commentable_id`
//!
//! That is the known trade-off of the polymorphic pattern: a single column
//! cannot reference two tables. The framework write path is therefore the
//! referential check — [`add_comment`] probes (and row-locks) the parent row
//! before it inserts, so an unknown parent is a `404` rather than a dangling
//! comment. Nothing else in this module trusts `commentable_id`.
//!
//! # Counter maintenance
//!
//! `comment_count` is maintained by the counter-cache mechanism (#1325) — the
//! same atomic `UPDATE parent SET c = c + $1`, applied
//! **inside** the comment's own transaction, so a reader never sees a comment
//! whose count has not moved (or a count that moved without a comment). The
//! delete path decrements by the number of rows the cascade actually removed,
//! never by an assumed 1.
//!
//! # Identifier safety
//!
//! Every table/column name below arrives as a `&'static str` emitted by
//! `#[commentable]` and validated at macro time to be a plain identifier.
//! [`CommentableSpec::validate`] re-checks that on **every** call — in release
//! builds too, not just under `debug_assertions` — because the fields are
//! public and a hand-built spec is otherwise the one way past the macro's
//! guarantee. Every interpolated identifier is additionally quoted, the same
//! convention the counter-cache and `dependent(restrict)` codegen follow.
//! Values (bodies, ids, the discriminator) are always **bound**, never
//! formatted.

use diesel::sql_types::{BigInt, Nullable, Text, Timestamp};
use diesel_async::RunQueryDsl as _;
use scoped_futures::ScopedFutureExt as _;

use crate::counter_cache::{
    CounterCacheSpec, TenantScope, counter_cache_apply_delta, is_plain_identifier, quote_ident,
};
use crate::db::{RuntimeConnection, scoped_immediate_transaction};
use crate::{AutumnError, AutumnResult};

/// Bind placeholder `n` (1-based). Postgres numbers its binds; `SQLite` does
/// not. Binds are pushed in the same order on both, so one statement template
/// with swapped placeholder text serves both backends — the same fork
/// [`crate::counter_cache`] uses.
#[cfg(not(feature = "sqlite"))]
fn ph(n: usize) -> String {
    format!("${n}")
}
#[cfg(feature = "sqlite")]
fn ph(_n: usize) -> String {
    "?".to_owned()
}

/// Row lock taken on the parent before anything is read or written.
///
/// It is what lets the counter `UPDATE` be keyed on the parent id alone: the
/// tenant-scoped probe proves the parent belongs to this caller's tenant, and
/// holding `FOR NO KEY UPDATE` until commit means no concurrent writer can
/// re-tenant the row underneath us. `FOR NO KEY UPDATE` rather than `FOR
/// UPDATE` because this transaction only ever writes the parent's counter
/// column, never its key — so it does not queue behind (or in front of) the
/// `FOR KEY SHARE` locks Postgres takes for foreign-key checks.
///
/// `SQLite` has no `SELECT … FOR UPDATE` and needs none: every write path here
/// runs under `BEGIN IMMEDIATE`, which excludes every other writer outright.
#[cfg(not(feature = "sqlite"))]
const FOR_NO_KEY_UPDATE: &str = " FOR NO KEY UPDATE";
#[cfg(feature = "sqlite")]
const FOR_NO_KEY_UPDATE: &str = "";

/// The author key types the comments API can carry.
///
/// `author_id` is `i64` across the whole public surface — [`CommentCreated`],
/// `add_comment`, the shared table's `BIGINT` column — so an author model keyed
/// by anything else cannot work. Nothing said so before: `#[commentable(by =
/// User)]` checked only that the type EXISTS, so a UUID-keyed `User` compiled
/// happily and then failed at run time. `session_author` parses the session
/// value with `str::parse::<i64>`, so every authenticated POST looked
/// signed-out and returned 401, and an `author_name` lookup compared a UUID key
/// against a bound `BIGINT`.
///
/// `i32` is admitted alongside `i64`: those ids widen losslessly, they parse,
/// and `PostgreSQL` compares `INTEGER` to `BIGINT` without complaint, so an
/// `i32`-keyed author model works today and must keep working.
///
/// Sealed, because implementing it for a wider type would re-open exactly the
/// runtime failure it exists to prevent.
pub trait CommentAuthorKey: sealed::Sealed {}

impl CommentAuthorKey for i64 {}
impl CommentAuthorKey for i32 {}

mod sealed {
    pub trait Sealed {}
    impl Sealed for i64 {}
    impl Sealed for i32 {}
}

/// The soft-delete marker column. Autumn's `soft_delete` convention is fixed,
/// so the predicate is a constant rather than another spec field.
const DELETED_AT: &str = "deleted_at";

/// Hard ceiling on the recursive CTEs' own recursion.
///
/// Threading is an adjacency list over insert-only rows whose parent must
/// already exist, so a cycle is unrepresentable — but a hand-edited row could
/// still create one, and an unguarded `WITH RECURSIVE` would then spin until
/// the connection died. The guard turns that into a bounded (wrong, but
/// terminating) answer.
const RECURSION_GUARD: i64 = 1_000;

/// The default maximum nesting depth: a top-level comment (`depth == 0`) plus
/// five levels of replies.
pub const DEFAULT_MAX_DEPTH: u32 = 5;

/// The default cap on a single comment body, in bytes.
pub const DEFAULT_MAX_BODY_BYTES: usize = 10_000;

// ── Spec ────────────────────────────────────────────────────────────────────

/// The shape of one commentable model's polymorphic comment binding, produced
/// at compile time by `#[commentable]`.
///
/// Framework plumbing; not constructed by hand. Every field is either a plain
/// SQL identifier (spliced, quoted, into the statements below) or a bound
/// value.
///
/// Deliberately **not** `#[non_exhaustive]`: `#[commentable]` expands to a
/// struct literal in the *user's* crate, which a non-exhaustive type forbids.
/// The protection that matters is [`validate`](Self::validate), which runs on
/// every entry point in release builds too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentableSpec {
    /// The shared comments table, e.g. `comments`.
    pub comments_table: &'static str,
    /// The comments table's primary key, e.g. `id`.
    pub comment_pk: &'static str,
    /// The discriminator column naming the parent *model*, e.g.
    /// `commentable_type`.
    pub type_column: &'static str,
    /// The discriminator column holding the parent row's id, e.g.
    /// `commentable_id`.
    pub id_column: &'static str,
    /// The self-referencing threading column, e.g. `parent_id`.
    pub parent_column: &'static str,
    /// The comment author's foreign key, e.g. `author_id`.
    pub author_column: &'static str,
    /// The comment body column, e.g. `body`.
    pub body_column: &'static str,
    /// The comment creation timestamp column, e.g. `created_at`.
    pub created_at_column: &'static str,
    /// Whether the comments table carries `deleted_at`. When true, deletes are
    /// soft and every read filters soft-deleted rows out.
    pub soft_delete: bool,
    /// The parent model's table, e.g. `posts`.
    pub parent_table: &'static str,
    /// The parent model's primary key, e.g. `id`.
    pub parent_pk: &'static str,
    /// Whether the *parent* model soft-deletes. When true a soft-deleted parent
    /// accepts no new comments and reports no thread.
    pub parent_soft_delete: bool,
    /// The maintained counter column on the parent, e.g. `comment_count`, or
    /// `None` for a parent that keeps no count.
    pub counter_column: Option<&'static str>,
    /// The parent's tenant discriminator column, from a `tenant_scoped`
    /// repository's model. `None` emits no tenant predicate anywhere, so a
    /// single-tenant app's SQL is byte-for-byte what it would be without this
    /// field.
    pub parent_tenant_column: Option<&'static str>,
    /// Whether the parent model is sharded (`#[shard_key = "…"]`).
    ///
    /// The generic router refuses to serve a sharded model: it extracts a
    /// plain `Db`, which always checks out the control pool, while the
    /// generated repository helpers route through the shard the tenant
    /// selects. Serving one from the other would probe and mutate the wrong
    /// database — silently, wherever the same tables exist in both.
    pub parent_sharded: bool,
    /// The author model's table, e.g. `users`, for resolving display names in
    /// [`comment_thread`]. `None` leaves [`Comment::author_name`] unresolved.
    pub author_table: Option<&'static str>,
    /// The author table's primary key, e.g. `id`.
    pub author_pk: &'static str,
    /// The author display-name column, e.g. `username`. `None` (the default)
    /// resolves no name — the framework refuses to guess a column.
    pub author_name_column: Option<&'static str>,
    /// Maximum nesting depth. A top-level comment is depth `0`, so `max_depth =
    /// 1` permits exactly one level of replies.
    pub max_depth: u32,
    /// Cap on a single comment body, in bytes.
    pub max_body_bytes: usize,
}

impl CommentableSpec {
    /// Every identifier this spec splices into SQL, with the field it came
    /// from — the input to [`validate`](Self::validate).
    const fn idents(&self) -> [(&'static str, Option<&'static str>); 15] {
        [
            ("comments_table", Some(self.comments_table)),
            ("comment_pk", Some(self.comment_pk)),
            ("type_column", Some(self.type_column)),
            ("id_column", Some(self.id_column)),
            ("parent_column", Some(self.parent_column)),
            ("author_column", Some(self.author_column)),
            ("body_column", Some(self.body_column)),
            ("created_at_column", Some(self.created_at_column)),
            ("parent_table", Some(self.parent_table)),
            ("parent_pk", Some(self.parent_pk)),
            ("author_pk", Some(self.author_pk)),
            ("counter_column", self.counter_column),
            ("parent_tenant_column", self.parent_tenant_column),
            ("author_table", self.author_table),
            ("author_name_column", self.author_name_column),
        ]
    }

    /// Reject a spec carrying a name that is not a plain SQL identifier,
    /// **before** any of it reaches a `format!`ed statement.
    ///
    /// `#[commentable]` already validates every name at macro time, so this
    /// never fires for a macro-built spec. It is not a `debug_assert` anyway:
    /// the struct's fields are `pub`, so a downstream crate can construct one
    /// from a configuration string, and a debug-only guard would be erased in
    /// exactly the build where that matters. Returning an error rather than
    /// panicking keeps a mistake in app wiring from taking the process down.
    ///
    /// # Errors
    ///
    /// [`AutumnError::internal_server_error_msg`] naming the offending field.
    pub fn validate(&self) -> AutumnResult<()> {
        for (field, value) in self.idents() {
            let Some(value) = value else { continue };
            if !is_plain_identifier(value) {
                return Err(AutumnError::internal_server_error_msg(format!(
                    "commentable spec field `{field}` is {value:?}, which is not a plain SQL \
                     identifier; it would be spliced verbatim into generated SQL"
                )));
            }
        }
        Ok(())
    }

    /// `AND "deleted_at" IS NULL` for the comments table, or nothing.
    fn live_comments(&self, alias: &str) -> String {
        if self.soft_delete {
            format!(" AND {alias}.{} IS NULL", quote_ident(DELETED_AT))
        } else {
            String::new()
        }
    }

    /// The counter-cache view of this spec, for
    /// [`counter_cache_apply_delta`].
    ///
    /// Only the parent-keyed delta is ever applied through it, so the record
    /// accessors are never called — hence `fk_of` reporting `None` rather than
    /// a fabricated foreign key. `tenant_column` is deliberately `None`: the
    /// counter `UPDATE` is confined to the caller's tenant by the *parent row
    /// lock* the probe already holds (see [`FOR_NO_KEY_UPDATE`]), not by a
    /// second correlated predicate against a comments table that has no tenant
    /// column to correlate on.
    fn counter_spec(&self, counter_column: &'static str) -> CounterCacheSpec<Comment> {
        CounterCacheSpec {
            child_table: self.comments_table,
            child_pk: self.comment_pk,
            child_soft_delete: self.soft_delete,
            fk_column: self.id_column,
            parent_table: self.parent_table,
            parent_pk: self.parent_pk,
            counter_column,
            fk_of: |_| None,
            pk_of: |comment| comment.id,
            live_of: |_| true,
            tenant_column: None,
            // Neutral derivation fields: a comment counter is a plain counter
            // cache, so every live row contributes 1 and nothing is filtered
            // out — which keeps the generated SQL byte-identical.
            contrib_of: |_| 1,
            contrib_sql: "1",
            filter_sql: "",
            derivation: None,
        }
    }
}

// ── Rows ────────────────────────────────────────────────────────────────────

/// One comment row, as returned by [`add_comment`] and [`comment_thread`].
///
/// Column aliases in the generated SQL are fixed (`id`, `parent_id`, …) so this
/// one struct reads back a comments table whose real column names are
/// configurable.
#[derive(Debug, Clone, PartialEq, Eq, diesel::QueryableByName)]
pub struct Comment {
    /// The comment's primary key.
    #[diesel(sql_type = BigInt)]
    pub id: i64,
    /// The comment this one replies to, or `None` for a top-level comment.
    #[diesel(sql_type = Nullable<BigInt>)]
    pub parent_id: Option<i64>,
    /// The author's id, as supplied by the caller.
    #[diesel(sql_type = BigInt)]
    pub author_id: i64,
    /// The comment body, verbatim (trimmed of surrounding whitespace on write).
    /// Plain text: rendering escapes it. Rich text composes with #1255.
    #[diesel(sql_type = Text)]
    pub body: String,
    /// When the comment was created.
    #[diesel(sql_type = Timestamp)]
    pub created_at: chrono::NaiveDateTime,
    /// The author's display name, when the model declared
    /// `#[commentable(author_name = <column>)]`. `None` otherwise — the
    /// framework does not guess a column.
    #[diesel(sql_type = Nullable<Text>)]
    pub author_name: Option<String>,
}

/// One node of a rendered thread: a comment plus the replies nested under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentNode {
    /// This node's comment.
    pub comment: Comment,
    /// Nesting depth, `0` for a top-level comment.
    pub depth: usize,
    /// Replies to this comment, in the same stable `(created_at, id)` order.
    pub replies: Vec<Self>,
}

#[derive(diesel::QueryableByName)]
struct DepthRow {
    #[diesel(sql_type = Nullable<BigInt>)]
    depth: Option<i64>,
}

/// The parent probe's result row. Only its *presence* matters — the id is
/// already known — but `QueryableByName` needs a field to bind the selected
/// column to.
#[derive(diesel::QueryableByName)]
struct ParentRow {
    #[allow(dead_code)]
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

/// A subtree row plus how deep the walk was when it was reached.
///
/// The depth is carried so [`delete_subtree`] can tell whether the recursive
/// walk was CUT OFF by [`RECURSION_GUARD`] rather than finishing.
#[derive(diesel::QueryableByName)]
struct SubtreeRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = BigInt)]
    depth: i64,
}

#[derive(diesel::QueryableByName)]
struct TargetRow {
    #[diesel(sql_type = Text)]
    commentable_type: String,
    #[diesel(sql_type = BigInt)]
    commentable_id: i64,
}

// ── Registry ────────────────────────────────────────────────────────────────

/// One `#[commentable]` model's registration, submitted by the macro so
/// [`router()`] can dispatch on `commentable_type` alone.
///
/// This is what makes AC5 true: adding comments to a second model is the
/// attribute and nothing else — no route, no query, no registration call.
#[doc(hidden)]
pub struct CommentableDescriptor {
    /// The discriminator this model stores in `commentable_type`.
    pub type_name: &'static str,
    /// The model type's fully-qualified name, as [`core::any::type_name`]
    /// reports it. Distinct from `type_name`, which a
    /// `#[commentable(type_name = "…")]` override can rename; this is what the
    /// sharded-repository registry is keyed on, and it has to be qualified so
    /// two modules' same-named models stay distinguishable.
    pub model: fn() -> &'static str,
    /// The model's binding.
    pub spec: &'static CommentableSpec,
}

inventory::collect!(CommentableDescriptor);

/// A model whose `#[repository(...)]` opted into `sharded`.
///
/// Registered by the repository macro rather than the model macro, because
/// **that** is where the fact lives: `#[repository(..., sharded)]` routes every
/// query through the tenant's shard whether or not the model carries a
/// `#[shard_key]` (see `sharding_across_tenants.rs`, where a sharded repository
/// has no shard key at all). Inferring shardedness from the model alone misses
/// exactly that shape — and the miss is silent, because the router would then
/// happily serve the model from the control pool.
#[cfg(feature = "db")]
pub struct RepositoryFacts {
    /// The model type's fully-qualified name.
    ///
    /// A function pointer to [`core::any::type_name`] rather than a
    /// `stringify!`d ident: two modules may each define a `Post`, and keying on
    /// the bare name would make a sharded `admin::Post` repository
    /// indistinguishable from an unsharded `blog::Post` — refusing to mount the
    /// router for a model that is perfectly safe to serve. `module_path!` is no
    /// good either, because a `#[repository]` trait and its `#[model]` struct
    /// routinely live in different modules; the *type* is the one identity both
    /// macros agree on.
    pub model: fn() -> &'static str,
    /// `#[repository(..., sharded)]` — queries route through the tenant's shard.
    pub sharded: bool,
    /// `#[repository(..., tenant_scoped)]` — queries carry a tenant predicate.
    pub tenant_scoped: bool,
    /// `#[repository(..., soft_delete)]` — reads filter `deleted_at IS NULL`.
    pub soft_delete: bool,
}

#[cfg(feature = "db")]
inventory::collect!(RepositoryFacts);

/// Whether `model` has a sharded repository registered in this binary.
#[cfg(feature = "db")]
#[must_use]
pub fn model_has_sharded_repository(model: &str) -> bool {
    // ANY sharded registration counts. The conservative answer here is the
    // opposite of tenancy's — refusing to mount a router that would read the
    // wrong database — but it is the same rule underneath: when registrations
    // disagree, take the one whose failure mode is a visible refusal.
    repository_facts_for(model).any(|facts| facts.sharded)
}

/// Every repository registration for `model`.
///
/// A model may have MORE than one repository trait — `repository_sharded.rs`
/// declares two for `Post`, and an app with a scoped application repository
/// beside an unscoped admin one is an ordinary shape. Returning only the first
/// would make link order decide, which is not a decision anyone made.
#[cfg(feature = "db")]
fn repository_facts_for(model: &str) -> impl Iterator<Item = &'static RepositoryFacts> {
    inventory::iter::<RepositoryFacts>().filter(move |facts| (facts.model)() == model)
}

/// Whether `model`'s comment routes must resolve a tenant.
///
/// The model's own `tenant_id` column is necessary but not sufficient: routing
/// is enabled by `#[repository(..., tenant_scoped)]`, and a model can carry the
/// column while its repository deliberately does not scope on it.
///
/// Aggregated across EVERY registration, and deliberately asymmetric in two
/// ways. A model with a tenant column stays scoped when nothing is registered,
/// and stays scoped when *any* registration is scoped — an unscoped admin
/// repository sitting beside a scoped application one must not be able to
/// unscope the routes. Both defaults point the same way on purpose: the cost of
/// being too strict is a 500 telling the operator to mount the middleware,
/// while the cost of being too lax is serving one tenant's comments to another.
#[cfg(feature = "db")]
#[must_use]
pub fn model_requires_tenant(model: &str, has_tenant_column: bool) -> bool {
    requires_tenant_from(repository_facts_for(model), has_tenant_column)
}

/// The aggregation behind [`model_requires_tenant`], over an arbitrary set of
/// registrations so the order-independence can be tested directly.
#[cfg(feature = "db")]
fn requires_tenant_from<'a>(
    facts: impl Iterator<Item = &'a RepositoryFacts>,
    has_tenant_column: bool,
) -> bool {
    if !has_tenant_column {
        return false;
    }
    let mut registered = false;
    for entry in facts {
        if entry.tenant_scoped {
            return true;
        }
        registered = true;
    }
    // No registration at all: keep scoping rather than guess it away.
    !registered
}

/// Whether `model` has a soft-deleting repository.
///
/// Like tenancy, taken from the repository rather than the presence of a
/// `deleted_at` column: an audit-style timestamp on a model whose repository
/// does not opt into `soft_delete` is ordinary data, and filtering on it would
/// 404 rows the app deliberately still serves.
///
/// `None` when no repository is registered, so the caller can fall back to what
/// the column implies rather than this function inventing an answer.
#[cfg(feature = "db")]
#[must_use]
pub fn model_soft_deletes(model: &str) -> Option<bool> {
    soft_deletes_from(repository_facts_for(model))
}

/// The aggregation behind [`model_soft_deletes`], split out so the choice it
/// makes across MULTIPLE repositories can be tested directly.
///
/// ANY repository soft-deleting makes the model soft-deleting here. That is a
/// deliberate conservative default, and it is not free: a model carrying both a
/// `soft_delete` repository and an ordinary one gets `deleted_at IS NULL` on
/// every helper, so a caller working through the ordinary repository sees a 404
/// for rows that repository's own finders would return.
///
/// It is still the better error of the two available. The helpers take a spec
/// and a connection, never a repository handle, so there is no "the repository
/// being used" to consult — and the opposite rule (soft-deleting only when
/// EVERY repository opts in) would let one admin repository that sees deleted
/// rows switch the filter off for the application repository beside it,
/// attaching comments to rows the app treats as gone. Erring toward 404 is
/// recoverable; erring toward writing is not.
///
/// Threading the caller's own repository fact through the helper API would beat
/// both, the way `tenant: Option<&str>` already does for tenancy — that is a
/// signature change to generated helpers, filed as #2284.
#[cfg(feature = "db")]
fn soft_deletes_from<'a>(facts: impl Iterator<Item = &'a RepositoryFacts>) -> Option<bool> {
    let mut any = false;
    let mut registered = false;
    for entry in facts {
        any |= entry.soft_delete;
        registered = true;
    }
    registered.then_some(any)
}

/// The spec registered for `type_name`, or `None` when no `#[commentable]`
/// model in this binary claims it.
#[must_use]
pub fn commentable_spec_for(type_name: &str) -> Option<&'static CommentableSpec> {
    inventory::iter::<CommentableDescriptor>()
        .find(|descriptor| descriptor.type_name == type_name)
        .map(|descriptor| descriptor.spec)
}

/// The model type name registered for `spec`, matched by IDENTITY.
///
/// Not by discriminator: two models sharing a `commentable_type` while pointing
/// at different comment tables is a supported helper-only shape (the router
/// still refuses it), so a name lookup could return the other model's identity
/// — and with it the other model's repository facts, applying one model's
/// soft-delete rule to the other's table.
///
/// The registered specs are `&'static`, so address equality is exactly the
/// question being asked. A hand-built spec matches nothing and the caller falls
/// back to what the spec itself declares.
#[cfg(feature = "db")]
#[must_use]
pub fn commentable_model_for_spec(spec: &CommentableSpec) -> Option<&'static str> {
    inventory::iter::<CommentableDescriptor>()
        .find(|descriptor| std::ptr::eq(descriptor.spec, spec))
        .map(|descriptor| (descriptor.model)())
}

/// The model type name registered for `type_name`, or `None` when no
/// `#[commentable]` model claims it.
///
/// Distinct from the discriminator: `#[commentable(type_name = "…")]` can
/// rename the latter, while the repository registry is keyed on the former.
#[cfg(feature = "db")]
#[must_use]
pub fn commentable_model_for(type_name: &str) -> Option<&'static str> {
    inventory::iter::<CommentableDescriptor>()
        .find(|descriptor| descriptor.type_name == type_name)
        .map(|descriptor| (descriptor.model)())
}

/// Every `commentable_type` registered in this binary, in unspecified order.
///
/// Useful for a boot-time assertion or an admin page; [`router()`] uses
/// [`commentable_spec_for`] instead.
#[must_use]
pub fn registered_commentable_types() -> Vec<&'static str> {
    inventory::iter::<CommentableDescriptor>()
        .map(|descriptor| descriptor.type_name)
        .collect()
}

/// The first `commentable_type` claimed by two different models, if any.
///
/// `type_name` defaults to the model's bare Rust type name, so two
/// `#[commentable]` models called `Post` in different modules — or a
/// hand-written `type_name` collision — would share one discriminator.
/// [`commentable_spec_for`] takes the first match, so the parent probe would
/// run against one model's table while the rows were shared with the other's:
/// `blog::Post` 5 and `shop::Post` 5 would render each other's threads.
/// [`router()`] calls this at construction and panics, because there is no
/// request-time answer that is not silently wrong.
#[must_use]
pub fn duplicate_commentable_type() -> Option<&'static str> {
    let mut seen = std::collections::HashSet::new();
    inventory::iter::<CommentableDescriptor>()
        .find(|descriptor| !seen.insert(descriptor.type_name))
        .map(|descriptor| descriptor.type_name)
}

/// The `commentable_type` of a registered model that is sharded, if any.
///
/// Used by [`router`] to refuse at wiring time: see [`CommentableSpec::parent_sharded`].
#[must_use]
pub fn sharded_commentable_type() -> Option<&'static str> {
    inventory::iter::<CommentableDescriptor>()
        .find(|descriptor| {
            // Two independent ways to be sharded, and BOTH have to be checked.
            // `parent_sharded` is the model's own `#[shard_key]`; the registry
            // is the repository's `#[repository(..., sharded)]`, which routes
            // through the tenant's shard even when the model carries no shard
            // key. Checking only the first leaves that shape served from the
            // control pool — silently the wrong database.
            descriptor.spec.parent_sharded || model_has_sharded_repository((descriptor.model)())
        })
        .map(|descriptor| descriptor.type_name)
}

/// Panic if two `#[commentable]` models share a discriminator.
///
/// Called from every public entry point rather than only from [`router`],
/// because the discriminator collision is a *data* hazard, not a routing one.
/// The default discriminator is the model's bare type name, so `blog::Post` and
/// `shop::Post` both store `"Post"` — and then `blog::Post` id 5 and
/// `shop::Post` id 5 address the same `(commentable_type, commentable_id)`
/// rows. Each model's parent probe still passes, against its own table, so
/// nothing looks wrong: one model simply renders and deletes the other's
/// comments.
///
/// An app that never mounts the router (using only the generated
/// `{Model}Comments` helpers) would otherwise never reach the check at all,
/// which is exactly the app most likely to have models in separate modules.
///
/// **Scoped to storage, not to the name.** A collision only misfiles rows when
/// the two models share the same comments table *and* the same discriminator
/// columns. `#[commentable(table = …)]` is a supported override, and two
/// same-named models pointed at different tables are as isolated as two
/// unrelated apps — panicking on those would make a legitimate configuration
/// unusable. [`router`] applies [`duplicate_commentable_type`] on top of this
/// one, because a mounted router dispatches on the string alone and a duplicate
/// is ambiguous there whatever the storage.
///
/// Checked once per process — the registry is built at load time and cannot
/// change afterwards — so this costs one atomic load per call.
#[cfg(feature = "db")]
fn assert_unique_discriminators() {
    static CHECKED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    CHECKED.get_or_init(|| {
        assert!(
            duplicate_commentable_storage().is_none(),
            "two #[commentable] models share the commentable_type {:?} **and** the same \
             comments storage, so the table cannot tell their rows apart: each would read \
             and delete the other's comments. Give one of them \
             `#[commentable(type_name = \"…\")]`, or point it at its own \
             `#[commentable(table = …)]`.",
            duplicate_commentable_storage().unwrap_or_default(),
        );
    });
}

/// The `commentable_type` shared by two models that also share their comments
/// storage — the combination that actually misfiles rows.
#[cfg(feature = "db")]
#[must_use]
pub fn duplicate_commentable_storage() -> Option<&'static str> {
    let mut seen = std::collections::HashSet::new();
    inventory::iter::<CommentableDescriptor>()
        .find(|descriptor| {
            let spec = descriptor.spec;
            !seen.insert((
                descriptor.type_name,
                spec.comments_table,
                spec.type_column,
                spec.id_column,
            ))
        })
        .map(|descriptor| descriptor.type_name)
}

// ── Write path ──────────────────────────────────────────────────────────────

/// Post a comment on `(parent_type, parent_id)`, optionally as a reply to
/// `reply_to`, and move the parent's counter in the **same** transaction.
///
/// The steps, all under one [`scoped_immediate_transaction`]:
///
/// 1. probe and row-lock the parent (existence, soft-delete, tenant) — the
///    referential check the polymorphic column cannot delegate to a foreign
///    key, and the lock that lets step 4 key on the parent id alone;
/// 2. when replying, walk the target comment's ancestors to prove it belongs to
///    *this* parent and to measure the new comment's depth against
///    [`CommentableSpec::max_depth`];
/// 3. insert;
/// 4. `UPDATE parent SET comment_count = comment_count + 1` via
///    [`counter_cache_apply_delta`].
///
/// # Errors
///
/// - `422` when `body` is blank after trimming, exceeds
///   [`CommentableSpec::max_body_bytes`], when `reply_to` names a comment that
///   is not a live comment on this same parent, or when the reply would exceed
///   `max_depth`.
/// - `404` when `(parent_type, parent_id)` names no live, visible parent row.
/// - Any database error.
#[allow(clippy::too_many_arguments)] // The polymorphic key is two values, and
// the tenant scope is a third: collapsing them into a struct would hide the
// association's actual shape at every call site.
pub async fn add_comment(
    conn: &mut RuntimeConnection,
    spec: &CommentableSpec,
    parent_type: &str,
    parent_id: i64,
    author_id: i64,
    body: &str,
    reply_to: Option<i64>,
    tenant: Option<&str>,
) -> AutumnResult<Comment> {
    // Every entry point checks: a helper-only app never mounts the router.
    assert_unique_discriminators();
    spec.validate()?;
    let body = body.trim();
    if body.is_empty() {
        return Err(AutumnError::unprocessable_msg("Comment cannot be empty"));
    }
    if body.len() > spec.max_body_bytes {
        return Err(AutumnError::unprocessable_msg(format!(
            "Comment is too long (limit {} bytes)",
            spec.max_body_bytes
        )));
    }

    // Owned copies so the transaction closure — which must be `'static`-ish
    // across the `scope_boxed` boundary — can move them.
    let spec = *spec;
    let parent_type = parent_type.to_owned();
    let body = body.to_owned();
    let tenant = tenant.map(str::to_owned);

    scoped_immediate_transaction::<Comment, AutumnError, _>(conn, |conn| {
        async move {
            lock_parent(conn, &spec, parent_id, tenant.as_deref()).await?;

            if let Some(reply_to) = reply_to {
                let parent_depth =
                    comment_depth(conn, &spec, &parent_type, parent_id, reply_to).await?;
                let depth = parent_depth.saturating_add(1);
                if depth > i64::from(spec.max_depth) {
                    return Err(AutumnError::unprocessable_msg(format!(
                        "Replies are nested at most {} deep here",
                        spec.max_depth
                    )));
                }
            }

            let inserted = insert_comment(
                conn,
                &spec,
                &parent_type,
                parent_id,
                author_id,
                &body,
                reply_to,
            )
            .await?;

            if let Some(counter_column) = spec.counter_column {
                counter_cache_apply_delta(
                    conn,
                    &spec.counter_spec(counter_column),
                    parent_id,
                    1,
                    TenantScope::Unscoped,
                )
                .await?;
            }

            Ok(inserted)
        }
        .scope_boxed()
    })
    .await
}

/// Delete `comment_id` **and every reply beneath it**, decrementing the
/// parent's counter by the number of rows actually removed.
///
/// Soft when the comments table carries `deleted_at` (the default), hard
/// otherwise. Idempotent: deleting an already-deleted comment removes nothing
/// and moves no counter, which is what keeps a double-submit from driving the
/// count negative.
///
/// # Errors
///
/// - `404` when `comment_id` names no comment on `(parent_type, parent_id)`, or
///   when that record is not visible to this caller. The record is part of the
///   check on purpose: without it, any comment id would be deletable from any
///   record of the same model.
/// - Any database error.
pub async fn delete_comment(
    conn: &mut RuntimeConnection,
    spec: &CommentableSpec,
    parent_type: &str,
    parent_id: i64,
    comment_id: i64,
    tenant: Option<&str>,
) -> AutumnResult<usize> {
    // Every entry point checks: a helper-only app never mounts the router.
    assert_unique_discriminators();
    spec.validate()?;
    let spec = *spec;
    let parent_type = parent_type.to_owned();
    let tenant = tenant.map(str::to_owned);

    scoped_immediate_transaction::<usize, AutumnError, _>(conn, |conn| {
        async move {
            let comments = quote_ident(spec.comments_table);
            let pk = quote_ident(spec.comment_pk);
            let type_column = quote_ident(spec.type_column);
            let id_column = quote_ident(spec.id_column);

            // Which parent does this comment hang off? Resolved first so the
            // parent row can be locked before the subtree is touched, keeping
            // the lock order (parent, then comments) identical to `add_comment`
            // — two writers on one thread can therefore never deadlock.
            let target: Option<TargetRow> = diesel::sql_query(format!(
                "SELECT {type_column} AS commentable_type, {id_column} AS commentable_id \
                 FROM {comments} WHERE {pk} = {}",
                ph(1)
            ))
            .bind::<BigInt, _>(comment_id)
            .get_result::<TargetRow>(conn)
            .await
            .optional_row()?;

            // Scoped to the RECORD, not merely to its model. Checking the
            // discriminator alone would let anyone holding any comment id
            // delete a comment on someone else's row — the mirror image of the
            // cross-record `reply_to` graft `comment_depth` refuses.
            let Some(target) = target
                .filter(|t| t.commentable_type == parent_type && t.commentable_id == parent_id)
            else {
                return Err(AutumnError::not_found_msg("Comment not found"));
            };

            lock_parent(conn, &spec, target.commentable_id, tenant.as_deref()).await?;

            let removed = delete_subtree(conn, &spec, &parent_type, parent_id, comment_id).await?;

            if removed > 0
                && let Some(counter_column) = spec.counter_column
            {
                let delta = i64::try_from(removed).unwrap_or(i64::MAX);
                counter_cache_apply_delta(
                    conn,
                    &spec.counter_spec(counter_column),
                    target.commentable_id,
                    -delta,
                    TenantScope::Unscoped,
                )
                .await?;
            }

            Ok(removed)
        }
        .scope_boxed()
    })
    .await
}

/// Rebuild `(parent_type, parent_id)`'s counter from the comments table and
/// return the value written.
///
/// The repair half of the counter, mirroring
/// [`counter_cache_recompute`](crate::counter_cache::counter_cache_recompute):
/// counters drift when rows arrive by import, seed, or hand-written SQL, and
/// they are deliberately not clamped, so a drifted one can go negative. This is
/// idempotent — running it twice writes the same number.
///
/// Reaching for `counter_cache_recompute` with this feature's spec would be
/// **wrong**: that helper keys on the foreign-key column alone, and
/// `commentable_id` is shared across models, so it would count another model's
/// comments that happen to share the id. This counts the discriminator pair.
///
/// Returns `0` for a parent that keeps no counter (`counter_cache = false`),
/// having written nothing.
///
/// # Errors
///
/// - `404` when `(parent_type, parent_id)` names no live, visible parent row.
/// - Any database error.
pub async fn recompute_comment_count(
    conn: &mut RuntimeConnection,
    spec: &CommentableSpec,
    parent_type: &str,
    parent_id: i64,
    tenant: Option<&str>,
) -> AutumnResult<i64> {
    // Every entry point checks: a helper-only app never mounts the router.
    assert_unique_discriminators();
    spec.validate()?;
    let Some(counter_column) = spec.counter_column else {
        probe_parent(conn, spec, parent_id, tenant, false).await?;
        return Ok(0);
    };

    let spec = *spec;
    let parent_type = parent_type.to_owned();
    let tenant = tenant.map(str::to_owned);

    scoped_immediate_transaction::<i64, AutumnError, _>(conn, |conn| {
        async move {
            lock_parent(conn, &spec, parent_id, tenant.as_deref()).await?;

            let comments = quote_ident(spec.comments_table);
            let type_column = quote_ident(spec.type_column);
            let id_column = quote_ident(spec.id_column);
            let parent_table = quote_ident(spec.parent_table);
            let parent_pk = quote_ident(spec.parent_pk);
            let counter = quote_ident(counter_column);
            let live = spec.live_comments("c");

            let truth: CountRow = diesel::sql_query(format!(
                "SELECT COUNT(*) AS count FROM {comments} AS c \
                 WHERE c.{type_column} = {} AND c.{id_column} = {}{live}",
                ph(1),
                ph(2),
            ))
            .bind::<Text, _>(&parent_type)
            .bind::<BigInt, _>(parent_id)
            .get_result::<CountRow>(conn)
            .await
            .map_err(AutumnError::from)?;

            diesel::sql_query(format!(
                "UPDATE {parent_table} SET {counter} = {} WHERE {parent_pk} = {}",
                ph(1),
                ph(2),
            ))
            .bind::<BigInt, _>(truth.count)
            .bind::<BigInt, _>(parent_id)
            .execute(conn)
            .await
            .map_err(AutumnError::from)?;

            Ok(truth.count)
        }
        .scope_boxed()
    })
    .await
}

// ── Read path ───────────────────────────────────────────────────────────────

/// The whole live thread for `(parent_type, parent_id)`, with replies nested
/// under their parent in stable `(created_at, id)` order.
///
/// **One** query for the comments (plus the parent visibility probe), whatever
/// the nesting depth — the tree is assembled in Rust, never with an N+1 walk.
/// Soft-deleted comments are filtered out; a live reply whose parent is missing
/// is promoted to the top level rather than silently dropped.
///
/// # Errors
///
/// - `404` when `(parent_type, parent_id)` names no live, visible parent row.
/// - Any database error.
pub async fn comment_thread(
    conn: &mut RuntimeConnection,
    spec: &CommentableSpec,
    parent_type: &str,
    parent_id: i64,
    tenant: Option<&str>,
) -> AutumnResult<Vec<CommentNode>> {
    // Every entry point checks: a helper-only app never mounts the router.
    assert_unique_discriminators();
    spec.validate()?;
    probe_parent(conn, spec, parent_id, tenant, false).await?;

    let comments = quote_ident(spec.comments_table);
    let pk = quote_ident(spec.comment_pk);
    let parent_column = quote_ident(spec.parent_column);
    let author_column = quote_ident(spec.author_column);
    let body_column = quote_ident(spec.body_column);
    let created_at = quote_ident(spec.created_at_column);
    let type_column = quote_ident(spec.type_column);
    let id_column = quote_ident(spec.id_column);
    let live = spec.live_comments("c");
    let (author_join, author_name) = author_name_fragments(spec);

    let sql = format!(
        "SELECT c.{pk} AS id, c.{parent_column} AS parent_id, \
                c.{author_column} AS author_id, c.{body_column} AS body, \
                c.{created_at} AS created_at, {author_name} AS author_name \
         FROM {comments} AS c{author_join} \
         WHERE c.{type_column} = {} AND c.{id_column} = {}{live} \
         ORDER BY c.{created_at} ASC, c.{pk} ASC",
        ph(1),
        ph(2)
    );
    let rows: Vec<Comment> = diesel::sql_query(sql)
        .bind::<Text, _>(parent_type)
        .bind::<BigInt, _>(parent_id)
        .load::<Comment>(conn)
        .await
        .map_err(AutumnError::from)?;

    Ok(nest(rows))
}

/// `(join clause, author-name expression)` for the thread query.
///
/// `CAST(NULL AS TEXT)` rather than a bare `NULL` so the column has a type on
/// both backends and `QueryableByName` can read it as `Nullable<Text>`.
fn author_name_fragments(spec: &CommentableSpec) -> (String, String) {
    match (spec.author_table, spec.author_name_column) {
        (Some(table), Some(column)) => (
            format!(
                " LEFT JOIN {} AS __autumn_cmt_author ON __autumn_cmt_author.{} = c.{}",
                quote_ident(table),
                quote_ident(spec.author_pk),
                quote_ident(spec.author_column),
            ),
            format!("__autumn_cmt_author.{}", quote_ident(column)),
        ),
        _ => (String::new(), "CAST(NULL AS TEXT)".to_owned()),
    }
}

/// Assemble a flat, ordered comment list into a forest.
///
/// Linear in the number of comments: one pass to index every row's position,
/// one pass to attach each child to its parent's `replies`. Order is preserved
/// at every level because the input is already ordered and children are pushed
/// in input order.
///
/// A reply whose parent is not in this thread (hard-deleted out from under it,
/// or soft-deleted while the reply stayed live) is promoted to the top level —
/// visibly wrong beats invisibly gone.
fn nest(rows: Vec<Comment>) -> Vec<CommentNode> {
    use std::collections::HashMap;

    // `index[id]` is the row's position in `rows`; children are collected by
    // parent position so the tree can be built bottom-up without cloning.
    let index: HashMap<i64, usize> = rows
        .iter()
        .enumerate()
        .map(|(position, comment)| (comment.id, position))
        .collect();

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (position, comment) in rows.iter().enumerate() {
        match comment.parent_id.and_then(|parent| index.get(&parent)) {
            // `parent < position` always holds for a well-formed thread (a
            // parent is inserted before its reply, and the query is ordered by
            // creation), but a clock skew or a hand-edited row could invert it.
            //
            // Keeping only strictly-backward edges is what makes the build
            // acyclic, and it has to be the real test: rejecting merely
            // `parent != position` catches a self-parenting row but not a
            // longer cycle, and a cyclic component has no root — every comment
            // in it would vanish from the thread rather than show up
            // misordered. Promoting the forward edge to a root keeps them
            // visible, which is the whole point of this defensive pass.
            Some(&parent) if parent < position => children[parent].push(position),
            _ => roots.push(position),
        }
    }

    let mut comments: Vec<Option<Comment>> = rows.into_iter().map(Some).collect();
    build_nodes(&roots, 0, &children, &mut comments)
}

/// The deepest level [`build_nodes`] will nest before flattening.
///
/// `#[commentable]` refuses a `max_depth` at or above [`RECURSION_GUARD`], so a
/// framework-written thread never comes close. This bounds the *rendering*
/// recursion against rows written some other way: `build_nodes`,
/// `CommentView::from_thread` and the widget all walk the tree depth-first, and
/// an unbounded chain would overflow the stack rather than raise an error.
/// Beyond it, the rest of the subtree is emitted flat at that level, by an
/// iterative walk — visibly flattened beats a dropped comment, and beats a
/// crashed worker.
/// Spelled as a literal rather than a cast of [`RECURSION_GUARD`]: the two have
/// different types (one indexes SQL, one indexes a `Vec`) and every `as`
/// between them is a truncation clippy is right to flag. They are pinned
/// together by `the_macro_depth_ceiling_matches_the_recursion_guard`.
const MAX_NESTING: usize = 1_000;

/// Emit an entire subtree as one flat list at `depth`, iteratively.
///
/// The escape hatch [`build_nodes`] takes at [`MAX_NESTING`]. Iterative on
/// purpose: this is the path a malformed, arbitrarily deep chain reaches, and
/// recursing here would reintroduce exactly the stack overflow the cap exists
/// to prevent.
fn flatten_subtree(
    positions: &[usize],
    depth: usize,
    children: &[Vec<usize>],
    comments: &mut [Option<Comment>],
) -> Vec<CommentNode> {
    let mut pending: Vec<usize> = positions.iter().rev().copied().collect();
    let mut out = Vec::new();
    while let Some(position) = pending.pop() {
        let Some(comment) = comments[position].take() else {
            continue;
        };
        out.push(CommentNode {
            comment,
            depth,
            replies: Vec::new(),
        });
        pending.extend(children[position].iter().rev().copied());
    }
    out
}

/// Recursive half of [`nest`], depth-first over an already-acyclic index.
fn build_nodes(
    positions: &[usize],
    depth: usize,
    children: &[Vec<usize>],
    comments: &mut [Option<Comment>],
) -> Vec<CommentNode> {
    positions
        .iter()
        .filter_map(|&position| {
            // `take` guarantees each row is emitted at most once even if the
            // index were somehow cyclic, which is what keeps this terminating.
            let comment = comments[position].take()?;
            let replies = if depth < MAX_NESTING {
                build_nodes(&children[position], depth + 1, children, comments)
            } else {
                // At the cap, the rest of the subtree is emitted flat rather
                // than dropped: a comment nobody can see is worse than one
                // rendered at the wrong indent, and the caller asked for the
                // whole thread.
                flatten_subtree(&children[position], depth, children, comments)
            };
            Some(CommentNode {
                comment,
                depth,
                replies,
            })
        })
        .collect()
}

// ── Statement helpers ───────────────────────────────────────────────────────

/// Probe the parent row, optionally taking the row lock.
///
/// The single point that enforces "this parent exists, is live, and belongs to
/// this caller's tenant". Everything else in this module keys on `parent_id`
/// having passed through here.
async fn probe_parent(
    conn: &mut RuntimeConnection,
    spec: &CommentableSpec,
    parent_id: i64,
    tenant: Option<&str>,
    lock: bool,
) -> AutumnResult<()> {
    let parent_table = quote_ident(spec.parent_table);
    let parent_pk = quote_ident(spec.parent_pk);
    // The column's presence is the fallback, not the answer. A `deleted_at`
    // timestamp on a model whose repository does not opt into `soft_delete` is
    // ordinary audit data, and filtering on it would 404 rows the app still
    // serves deliberately. Only when no repository is registered does the
    // column get to decide.
    let soft_deletes = commentable_model_for_spec(spec)
        .and_then(model_soft_deletes)
        .unwrap_or(spec.parent_soft_delete);
    let live = if soft_deletes {
        format!(" AND {parent_table}.{} IS NULL", quote_ident(DELETED_AT))
    } else {
        String::new()
    };
    let lock_clause = if lock { FOR_NO_KEY_UPDATE } else { "" };

    // Three-valued, exactly like `#[votable]`'s `__autumn_m2m_tenant_scope()`
    // contract: the predicate is emitted only when the model HAS a tenant
    // column AND this caller resolved a tenant. A repository that is not
    // `tenant_scoped` — or one used through `across_tenants()` — passes `None`
    // and must get the unscoped query. Branching on the spec alone would bind
    // `NULL` into `IS NOT DISTINCT FROM`, which matches only untenanted rows,
    // turning every call on a tenant-columned model into a 404.
    let found: Option<ParentRow> =
        if let (Some(column), Some(tenant)) = (spec.parent_tenant_column, tenant) {
            {
                let sql = format!(
                    "SELECT {parent_pk} AS id FROM {parent_table} \
                 WHERE {parent_table}.{parent_pk} = {} \
                   AND {parent_table}.{} = {}{live}{lock_clause}",
                    ph(1),
                    quote_ident(column),
                    ph(2),
                );
                diesel::sql_query(sql)
                    .bind::<BigInt, _>(parent_id)
                    .bind::<Text, _>(tenant)
                    .get_result::<ParentRow>(conn)
                    .await
                    .optional_row()?
            }
        } else {
            {
                let sql = format!(
                    "SELECT {parent_pk} AS id FROM {parent_table} \
                 WHERE {parent_table}.{parent_pk} = {}{live}{lock_clause}",
                    ph(1),
                );
                diesel::sql_query(sql)
                    .bind::<BigInt, _>(parent_id)
                    .get_result::<ParentRow>(conn)
                    .await
                    .optional_row()?
            }
        };

    if found.is_none() {
        return Err(AutumnError::not_found_msg("Comment target not found"));
    }
    Ok(())
}

/// [`probe_parent`] with the row lock — the write paths' first statement.
async fn lock_parent(
    conn: &mut RuntimeConnection,
    spec: &CommentableSpec,
    parent_id: i64,
    tenant: Option<&str>,
) -> AutumnResult<()> {
    probe_parent(conn, spec, parent_id, tenant, true).await
}

/// The depth of `comment_id` within `(parent_type, parent_id)`'s thread, where
/// a top-level comment is `0`.
///
/// Doubles as the reply-target validation: the recursive term's anchor requires
/// the target to be a **live** comment on **this** parent, so a `reply_to` from
/// another record (or a deleted one) yields no rows and is rejected — without
/// it, anyone holding any comment id could graft a subtree onto someone else's
/// record.
async fn comment_depth(
    conn: &mut RuntimeConnection,
    spec: &CommentableSpec,
    parent_type: &str,
    parent_id: i64,
    comment_id: i64,
) -> AutumnResult<i64> {
    let comments = quote_ident(spec.comments_table);
    let pk = quote_ident(spec.comment_pk);
    let parent_column = quote_ident(spec.parent_column);
    let type_column = quote_ident(spec.type_column);
    let id_column = quote_ident(spec.id_column);
    let live = spec.live_comments("c");

    let sql = format!(
        "WITH RECURSIVE __autumn_cmt_anc(id, parent_id, depth) AS (\
           SELECT c.{pk}, c.{parent_column}, CAST(0 AS BIGINT) FROM {comments} AS c \
            WHERE c.{pk} = {} AND c.{type_column} = {} AND c.{id_column} = {}{live} \
           UNION ALL \
           SELECT p.{pk}, p.{parent_column}, a.depth + 1 \
             FROM {comments} AS p JOIN __autumn_cmt_anc AS a ON p.{pk} = a.parent_id \
            WHERE a.depth < {RECURSION_GUARD}\
         ) SELECT CAST(MAX(depth) AS BIGINT) AS depth FROM __autumn_cmt_anc",
        ph(1),
        ph(2),
        ph(3),
    );
    let row: DepthRow = diesel::sql_query(sql)
        .bind::<BigInt, _>(comment_id)
        .bind::<Text, _>(parent_type)
        .bind::<BigInt, _>(parent_id)
        .get_result::<DepthRow>(conn)
        .await
        .map_err(AutumnError::from)?;

    let depth = row.depth.ok_or_else(|| {
        AutumnError::unprocessable_msg("Cannot reply to that comment: it is not on this record")
    })?;
    // The CTE stops recursing at `RECURSION_GUARD`, so a chain longer than that
    // reports exactly the guard rather than its real depth. Returning the
    // truncated number would make `max_depth` unenforceable for any thread that
    // got that deep; refusing is the only honest answer. `#[commentable]`
    // rejects a `max_depth` at or above the guard, so this is reachable only
    // through rows written outside the framework.
    if depth >= RECURSION_GUARD {
        return Err(AutumnError::unprocessable_msg(
            "This thread is nested too deeply to reply to",
        ));
    }
    Ok(depth)
}

/// Insert one comment and read it back.
#[allow(clippy::too_many_arguments)] // Same shape as `add_comment`'s own.
async fn insert_comment(
    conn: &mut RuntimeConnection,
    spec: &CommentableSpec,
    parent_type: &str,
    parent_id: i64,
    author_id: i64,
    body: &str,
    reply_to: Option<i64>,
) -> AutumnResult<Comment> {
    let comments = quote_ident(spec.comments_table);
    let pk = quote_ident(spec.comment_pk);
    let parent_column = quote_ident(spec.parent_column);
    let author_column = quote_ident(spec.author_column);
    let body_column = quote_ident(spec.body_column);
    let created_at = quote_ident(spec.created_at_column);
    let type_column = quote_ident(spec.type_column);
    let id_column = quote_ident(spec.id_column);
    // Resolved in the `RETURNING` list so the caller can render the new comment
    // without a second round trip; `NULL` when the model declared no
    // `author_name` column.
    // The author id is bound a SECOND time here rather than reusing `$4`.
    // Postgres would happily take the repeat, but `SQLite` numbers its
    // placeholders by *position of occurrence*, so a reused `$4` would render
    // as a sixth bare `?` with only five values pushed. Binding it twice is the
    // one spelling that is correct on both backends. (Naming `author_id`
    // unqualified inside `RETURNING` is not an option either: it would be
    // ambiguous against the author table's own columns.)
    let resolves_author_name = spec.author_table.is_some() && spec.author_name_column.is_some();
    let author_name = match (spec.author_table, spec.author_name_column) {
        (Some(table), Some(column)) => format!(
            "(SELECT {} FROM {} WHERE {} = {})",
            quote_ident(column),
            quote_ident(table),
            quote_ident(spec.author_pk),
            ph(6),
        ),
        _ => "CAST(NULL AS TEXT)".to_owned(),
    };

    let sql = format!(
        "INSERT INTO {comments} \
           ({type_column}, {id_column}, {parent_column}, {author_column}, {body_column}) \
         VALUES ({}, {}, {}, {}, {}) \
         RETURNING {pk} AS id, {parent_column} AS parent_id, {author_column} AS author_id, \
                   {body_column} AS body, {created_at} AS created_at, \
                   {author_name} AS author_name",
        ph(1),
        ph(2),
        ph(3),
        ph(4),
        ph(5),
    );
    // Bind order mirrors placeholder ORDER OF OCCURRENCE in the statement
    // text, which is what `SQLite`'s bare `?` counts: the five `VALUES` binds,
    // then the author id again for the `RETURNING` sub-select. When there is no
    // `author_name` to resolve the sub-select is absent, so the sixth bind
    // would have no placeholder — hence the fork.
    let query = diesel::sql_query(sql)
        .bind::<Text, _>(parent_type)
        .bind::<BigInt, _>(parent_id)
        .bind::<Nullable<BigInt>, _>(reply_to)
        .bind::<BigInt, _>(author_id)
        .bind::<Text, _>(body);
    if resolves_author_name {
        query
            .bind::<BigInt, _>(author_id)
            .get_result::<Comment>(conn)
            .await
            .map_err(AutumnError::from)
    } else {
        query
            .get_result::<Comment>(conn)
            .await
            .map_err(AutumnError::from)
    }
}

/// Remove `comment_id` and its whole descendant subtree, returning how many
/// rows actually moved — which is exactly the counter delta.
///
/// The subtree ids are **materialised first**, then deleted by id, and the
/// delta is `ids.len()` rather than the statement's affected-row count. That is
/// not belt-and-braces: `parent_id` carries `ON DELETE CASCADE`, and `SQLite`'s
/// `changes()` excludes rows removed by a foreign-key action — so on the
/// hard-delete path a three-row subtree would report `1`, leaving the counter
/// permanently two too high with no recompute in sight. Counting the ids is the
/// one measure both engines agree on.
///
/// The walk is confined to `(parent_type, parent_id)`. Nothing the framework
/// writes can produce a cross-record `parent_id` chain (`comment_depth` refuses
/// it), but no foreign key or `CHECK` enforces that, and an app that inserts
/// comments with raw Diesel can. Without the predicate one parent's counter
/// would absorb the whole span.
async fn delete_subtree(
    conn: &mut RuntimeConnection,
    spec: &CommentableSpec,
    parent_type: &str,
    parent_id: i64,
    comment_id: i64,
) -> AutumnResult<usize> {
    let comments = quote_ident(spec.comments_table);
    let pk = quote_ident(spec.comment_pk);
    let parent_column = quote_ident(spec.parent_column);
    let type_column = quote_ident(spec.type_column);
    let id_column = quote_ident(spec.id_column);
    let deleted_at = quote_ident(DELETED_AT);
    let anchor_live = spec.live_comments("c");
    let descendant_live = spec.live_comments("d");

    let ids: Vec<SubtreeRow> = diesel::sql_query(format!(
        "WITH RECURSIVE __autumn_cmt_sub(id, depth) AS (\
           SELECT c.{pk}, CAST(0 AS BIGINT) FROM {comments} AS c \
            WHERE c.{pk} = {} AND c.{type_column} = {} AND c.{id_column} = {}{anchor_live} \
           UNION ALL \
           SELECT d.{pk}, s.depth + 1 \
             FROM {comments} AS d JOIN __autumn_cmt_sub AS s ON d.{parent_column} = s.id \
            WHERE s.depth < {RECURSION_GUARD} \
              AND d.{type_column} = {} AND d.{id_column} = {}{descendant_live}\
         ) SELECT id, depth FROM __autumn_cmt_sub",
        ph(1),
        ph(2),
        ph(3),
        ph(4),
        ph(5),
    ))
    .bind::<BigInt, _>(comment_id)
    .bind::<Text, _>(parent_type)
    .bind::<BigInt, _>(parent_id)
    .bind::<Text, _>(parent_type)
    .bind::<BigInt, _>(parent_id)
    .load::<SubtreeRow>(conn)
    .await
    .map_err(AutumnError::from)?;

    // Deduplicate before ANY of it is used as a count. `UNION ALL` does not
    // deduplicate, and a `parent_id` cycle among imported or hand-edited rows
    // has no terminating edge, so the walk re-emits the same handful of ids at
    // every depth until the guard stops it — roughly a thousand rows for a
    // two-comment cycle. The `UPDATE`/`DELETE` is keyed on `id IN (…)` and so
    // touches each physical row once, but `ids.len()` is the counter delta, and
    // an inflated one drives `comment_count` sharply negative with no error
    // anywhere. Counters are deliberately unclamped, so it stays wrong until
    // someone runs `recompute_comment_count`.
    //
    // Sorted by depth first so the retained copy of each id is its shallowest,
    // keeping the truncation check below meaningful.
    let mut ids = ids;
    ids.sort_by_key(|row| (row.id, row.depth));
    ids.dedup_by_key(|row| row.id);

    // The walk stops at `RECURSION_GUARD`. On the SOFT-delete path that is
    // survivable: the rows past it stay live, stay counted, and surface as
    // promoted roots. On the HARD-delete path it is not — the `parent_id`
    // foreign key cascades, so the database removes every deeper descendant
    // while `ids.len()` counts only what the walk reached, and the parent's
    // counter is left permanently too high with no error anywhere.
    //
    // Refusing is the honest answer. A chain this deep cannot be produced by
    // the framework's own write path (`max_depth` is capped below the guard),
    // so reaching here means imported or hand-edited rows, and quietly
    // corrupting a counter is a worse service than saying so.
    if !spec.soft_delete && ids.iter().any(|row| row.depth >= RECURSION_GUARD) {
        return Err(AutumnError::unprocessable_msg(format!(
            "comment {comment_id} has a reply chain deeper than {RECURSION_GUARD}, which this \
             hard-delete path cannot remove without leaving the parent's comment counter wrong: \
             the database would cascade past what the traversal can see. Shorten or repair the \
             chain, or run the delete in batches from the leaves."
        )));
    }

    if ids.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i64> = ids.into_iter().map(|row| row.id).collect();
    // Bound the `IN (…)` list: the ids are framework-produced `i64`s, never
    // caller text, so they are formatted rather than bound — Postgres caps
    // bind parameters at 65535 and a deep thread could exceed it.
    let id_list = ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");

    if spec.soft_delete {
        diesel::sql_query(format!(
            "UPDATE {comments} SET {deleted_at} = {} \
             WHERE {pk} IN ({id_list}) AND {deleted_at} IS NULL",
            ph(1),
        ))
        .bind::<Timestamp, _>(chrono::Utc::now().naive_utc())
        .execute(conn)
        .await
        .map_err(AutumnError::from)?;
    } else {
        diesel::sql_query(format!("DELETE FROM {comments} WHERE {pk} IN ({id_list})"))
            .execute(conn)
            .await
            .map_err(AutumnError::from)?;
    }

    Ok(ids.len())
}

/// `Result<T, diesel::result::Error>` → `AutumnResult<Option<T>>`, mapping
/// `NotFound` to `None`.
///
/// A local trait rather than diesel's `OptionalExtension` so the `?` in the
/// callers converts to [`AutumnError`] in the same expression.
trait OptionalRow<T> {
    fn optional_row(self) -> AutumnResult<Option<T>>;
}

impl<T> OptionalRow<T> for Result<T, diesel::result::Error> {
    fn optional_row(self) -> AutumnResult<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(diesel::result::Error::NotFound) => Ok(None),
            Err(err) => Err(AutumnError::from(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: i64, parent_id: Option<i64>, body: &str) -> Comment {
        Comment {
            id,
            parent_id,
            author_id: 1,
            body: body.to_owned(),
            created_at: chrono::NaiveDateTime::default(),
            author_name: None,
        }
    }

    fn walk(nodes: &[CommentNode], out: &mut Vec<(usize, String)>) {
        for node in nodes {
            out.push((node.depth, node.comment.body.clone()));
            walk(&node.replies, out);
        }
    }

    fn flatten(nodes: &[CommentNode]) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        walk(nodes, &mut out);
        out
    }

    #[test]
    fn nest_preserves_input_order_at_every_level() {
        let rows = vec![
            comment(1, None, "a"),
            comment(2, Some(1), "a1"),
            comment(3, Some(2), "a1x"),
            comment(4, Some(1), "a2"),
            comment(5, None, "b"),
        ];
        assert_eq!(
            flatten(&nest(rows)),
            vec![
                (0, "a".to_owned()),
                (1, "a1".to_owned()),
                (2, "a1x".to_owned()),
                (1, "a2".to_owned()),
                (0, "b".to_owned()),
            ]
        );
    }

    #[test]
    fn nest_promotes_an_orphan_rather_than_dropping_it() {
        // Parent 99 is not in this thread (soft-deleted, or hard-deleted out
        // from under the reply). The reply must still render.
        let rows = vec![comment(1, None, "a"), comment(2, Some(99), "orphan")];
        assert_eq!(
            flatten(&nest(rows)),
            vec![(0, "a".to_owned()), (0, "orphan".to_owned())]
        );
    }

    #[test]
    fn nest_survives_a_self_referential_row() {
        let rows = vec![comment(1, Some(1), "self")];
        assert_eq!(flatten(&nest(rows)), vec![(0, "self".to_owned())]);
    }

    /// A cycle longer than one node has no root, so accepting its edges would
    /// drop every comment in the component — the opposite of what this pass is
    /// for. Only strictly-backward edges are kept, which promotes the forward
    /// one to a root and leaves both rows visible.
    #[test]
    fn nest_keeps_both_rows_of_a_two_node_cycle() {
        // 1 -> 2 and 2 -> 1: neither is a root by parent_id alone.
        let rows = vec![comment(1, Some(2), "a"), comment(2, Some(1), "b")];
        let flattened = flatten(&nest(rows));
        assert_eq!(flattened.len(), 2, "no comment may vanish: {flattened:?}");
        assert!(
            flattened.iter().any(|(_, body)| body == "a"),
            "{flattened:?}"
        );
        assert!(
            flattened.iter().any(|(_, body)| body == "b"),
            "{flattened:?}"
        );
    }

    /// Same for a longer ring, which a hand-edited or imported thread could
    /// produce just as easily.
    #[test]
    fn nest_keeps_every_row_of_a_longer_cycle() {
        let rows = vec![
            comment(1, Some(3), "a"),
            comment(2, Some(1), "b"),
            comment(3, Some(2), "c"),
        ];
        assert_eq!(flatten(&nest(rows)).len(), 3);
    }

    #[test]
    fn nest_of_an_empty_thread_is_empty() {
        assert!(nest(Vec::new()).is_empty());
    }

    /// The open-redirect guard on `return_to`.
    ///
    /// The tab case is the one that matters: the WHATWG URL parser strips
    /// ASCII tab, LF and CR from a URL *before* parsing, and `HeaderValue`
    /// carries a tab happily — so a CR/LF-only check would let
    /// `/\t/evil.example` through to the browser, which then reads it as
    /// `//evil.example`.
    #[cfg(all(feature = "db", feature = "maud"))]
    #[test]
    fn only_a_relative_single_slash_path_is_a_safe_return_target() {
        for safe in [
            "/",
            "/r/rust/posts/hello",
            "/posts?page=2",
            "/posts#comment-7",
        ] {
            assert!(is_safe_return_path(safe), "{safe:?} should be safe");
        }
        for unsafe_path in [
            "",
            "//evil.example",
            "https://evil.example",
            "/\\evil.example",
            "/a\\b",
            "/\tevil",
            "/\t/evil.example",
            "/ok\r\nSet-Cookie: x=1",
            "/ok\n",
            "/with space",
            "/\u{1}",
            "/\u{7f}",
            "evil.example",
        ] {
            assert!(
                !is_safe_return_path(unsafe_path),
                "{unsafe_path:?} must be refused"
            );
        }
    }

    /// The identifier guard is what keeps a hand-constructed spec from
    /// smuggling SQL through `format!`.
    #[test]
    fn quoted_identifiers_are_rejected_before_they_reach_sql() {
        assert!(is_plain_identifier("comment_count"));
        assert!(!is_plain_identifier("comment_count\"; DROP TABLE posts --"));
        assert!(!is_plain_identifier(""));
        assert!(!is_plain_identifier("1bad"));
    }

    fn sample_spec() -> CommentableSpec {
        CommentableSpec {
            comments_table: "comments",
            comment_pk: "id",
            type_column: "commentable_type",
            id_column: "commentable_id",
            parent_column: "parent_id",
            author_column: "author_id",
            body_column: "body",
            created_at_column: "created_at",
            soft_delete: true,
            parent_table: "posts",
            parent_pk: "id",
            parent_soft_delete: false,
            counter_column: Some("comment_count"),
            parent_tenant_column: None,
            parent_sharded: false,
            author_table: Some("users"),
            author_pk: "id",
            author_name_column: Some("username"),
            max_depth: DEFAULT_MAX_DEPTH,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }

    /// A spec built by hand — the one way past the macro's guarantee — is
    /// refused by `validate`, in release builds too. A `debug_assert` here
    /// would be erased in exactly the build where it matters.
    /// A draft may only be pinned to a form that will actually be rendered.
    #[cfg(all(feature = "db", feature = "maud"))]
    #[test]
    fn a_reply_form_exists_only_while_the_target_can_still_be_replied_to() {
        fn node(id: i64, depth: usize, replies: Vec<CommentNode>) -> CommentNode {
            CommentNode {
                comment: Comment {
                    id,
                    parent_id: None,
                    author_id: 1,
                    body: String::new(),
                    created_at: chrono::NaiveDateTime::default(),
                    author_name: None,
                },
                depth,
                replies,
            }
        }

        let thread = vec![node(1, 0, vec![node(2, 1, vec![node(3, 2, Vec::new())])])];

        // Present and shallow enough: the form is there to fill.
        assert!(reply_form_exists(&thread, 1, 3));
        // Nested targets are found too — the thread is a tree, not a list.
        assert!(reply_form_exists(&thread, 3, 3));
        // At the cap the widget offers no form, so a draft must not be pinned
        // to one: `can_reply` is `depth < max_depth`.
        assert!(!reply_form_exists(&thread, 3, 3 - 1));
        // A target that is simply gone — deleted between render and submit, or
        // never real — is the case that erased the visitor's body.
        assert!(!reply_form_exists(&thread, 99, 3));
        assert!(!reply_form_exists(&[], 1, 3));
    }

    /// The soft-delete counterpart of the tenancy rule below, with the same
    /// link-order independence — and one case that is a documented cost rather
    /// than a win: see [`soft_deletes_from`] and #2284.
    #[test]
    fn any_soft_deleting_repository_filters_deleted_parents() {
        let soft = RepositoryFacts {
            model: || "app::Post",
            sharded: false,
            tenant_scoped: false,
            soft_delete: true,
        };
        let plain = RepositoryFacts {
            model: || "app::Post",
            sharded: false,
            tenant_scoped: false,
            soft_delete: false,
        };

        // Both visitation orders, because inventory order is link order.
        assert_eq!(
            soft_deletes_from([&soft, &plain].into_iter()),
            Some(true),
            "soft first"
        );
        assert_eq!(
            soft_deletes_from([&plain, &soft].into_iter()),
            Some(true),
            "plain first — the answer must not change"
        );

        // Every registration opting out IS positive evidence, and must not be
        // confused with nothing being registered: the caller falls back to what
        // the column implies only in the latter case.
        assert_eq!(soft_deletes_from([&plain].into_iter()), Some(false));
        assert_eq!(soft_deletes_from(std::iter::empty()), None);
    }

    /// A model may have more than one repository. Taking the FIRST registration
    /// would let link order decide whether the tenant predicate is applied —
    /// and an unscoped admin repository beside a scoped application one would
    /// then be able to unscope the routes, reading another tenant's thread.
    #[test]
    fn any_scoped_repository_keeps_the_routes_scoped() {
        let scoped = RepositoryFacts {
            model: || "app::Post",
            sharded: false,
            tenant_scoped: true,
            soft_delete: false,
        };
        let unscoped = RepositoryFacts {
            model: || "app::Post",
            sharded: false,
            tenant_scoped: false,
            soft_delete: false,
        };

        // Both visitation orders, because inventory order is link order.
        assert!(
            requires_tenant_from([&scoped, &unscoped].into_iter(), true),
            "scoped first"
        );
        assert!(
            requires_tenant_from([&unscoped, &scoped].into_iter(), true),
            "unscoped first — the answer must not change"
        );

        // Every registration unscoped IS positive evidence of opting out.
        assert!(!requires_tenant_from([&unscoped].into_iter(), true));
        // No registration at all keeps scoping.
        assert!(requires_tenant_from(std::iter::empty(), true));
        // …and no tenant column is never scoped, whatever is registered.
        assert!(!requires_tenant_from([&scoped].into_iter(), false));
    }

    /// Tenancy is the one place where "no information" must not mean "no
    /// scoping". A model with a tenant column stays scoped unless a repository
    /// positively says it opted out — guessing the other way would serve one
    /// tenant's comments to another.
    #[test]
    fn an_unregistered_model_stays_tenant_scoped() {
        // No repository facts are registered for these names.
        assert!(
            model_requires_tenant("nobody::Unregistered", true),
            "absent positive evidence of opting out, a tenant column scopes"
        );
        assert!(
            !model_requires_tenant("nobody::Unregistered", false),
            "…but a model with no tenant column is never scoped"
        );
        // The lookup failing entirely is the same conservative answer.
        assert!(model_requires_tenant("", true));
    }

    #[test]
    fn validate_refuses_a_hand_built_spec_carrying_sql() {
        let spec = sample_spec();
        assert!(spec.validate().is_ok());

        let mut smuggled = sample_spec();
        smuggled.comments_table = "comments\"; DROP TABLE users --";
        let err = smuggled
            .validate()
            .expect_err("a quoted name must be refused");
        assert!(err.to_string().contains("comments_table"), "{err}");

        let mut smuggled = sample_spec();
        smuggled.counter_column = Some("count; DROP TABLE users");
        let err = smuggled.validate().expect_err("an optional name too");
        assert!(err.to_string().contains("counter_column"), "{err}");

        // A `None` optional is not a name and must not be rejected.
        let mut sparse = sample_spec();
        sparse.counter_column = None;
        sparse.author_table = None;
        sparse.author_name_column = None;
        assert!(sparse.validate().is_ok());
    }

    /// The macro's own `max_depth` ceiling must match the runtime's recursion
    /// guard. The proc-macro crate cannot reference this constant, so the two
    /// are kept in step here.
    #[test]
    fn the_macro_depth_ceiling_matches_the_recursion_guard() {
        assert_eq!(RECURSION_GUARD, 1_000);
        assert_eq!(MAX_NESTING, usize::try_from(RECURSION_GUARD).expect("fits"));
        assert!(i64::from(DEFAULT_MAX_DEPTH) < RECURSION_GUARD);
    }

    /// A thread deeper than the render guard is flattened, not recursed into —
    /// an unbounded chain would overflow the stack instead of raising.
    #[test]
    fn nest_stops_nesting_at_the_recursion_guard() {
        let depth_beyond = i64::try_from(MAX_NESTING).expect("fits") + 10;
        let rows: Vec<Comment> = (1..=depth_beyond)
            .map(|id| comment(id, (id > 1).then_some(id - 1), &format!("c{id}")))
            .collect();
        let flat = flatten(&nest(rows));
        let deepest = flat.iter().map(|(depth, _)| *depth).max().expect("nodes");
        assert_eq!(deepest, MAX_NESTING);
        assert_eq!(
            flat.len(),
            MAX_NESTING + 10,
            "every comment still renders; only the nesting is capped"
        );
    }
}

// ── Generic router ──────────────────────────────────────────────────────────

/// Configuration for the framework's generic comment [`router()`].
///
/// The router serves **every** `#[commentable]` model in the binary from one
/// pair of routes, dispatching on the `{commentable_type}` path segment through
/// the [`inventory`] registry. That is what makes adding comments to a second
/// model zero new routes and zero new queries.
#[cfg(all(feature = "db", feature = "maud"))]
#[derive(Clone)]
#[non_exhaustive]
pub struct CommentsConfig {
    /// Where the router is mounted, used to build each thread's form action.
    /// Must match the path passed to `nest`. Default `/comments`.
    pub mount_path: String,
    /// Session key holding the signed-in author's id. Default `user_id`.
    ///
    /// A request with no such key may still *read* a thread; posting is
    /// `401`, and the widget renders read-only with
    /// [`sign_in_prompt`](Self::sign_in_prompt) in place of the form.
    pub session_author_key: String,
    /// Rendered in place of the comment form for a signed-out visitor.
    pub sign_in_prompt: String,
    /// `aria-label` of the rendered region.
    pub label: String,
    /// Record-level authorization, called before **either** handler touches the
    /// database.
    ///
    /// The router authorizes the *tenant* (through the spec's tenant column)
    /// but knows nothing about a record's own visibility — it cannot, since it
    /// dispatches on a string. Without a hook here, mounting the router makes
    /// every registered model's threads world-readable by id and
    /// world-commentable by any signed-in user, whatever the app's own
    /// `Policy` says about the parent record.
    ///
    /// `None` (the default) allows everything, which is right for a forum or a
    /// blog where the records are public anyway. **An app with private,
    /// draft, or role-gated records must set this** — see
    /// [`CommentsConfig::authorize`].
    pub authorize: Option<CommentAuthorizer>,
    /// Called after a comment is successfully created through the router.
    ///
    /// The router deliberately owns no app-specific side effects, but a real
    /// app has them: a notification, a live-feed broadcast, a moderation
    /// queue, a search index. Without a hook here, adopting the generic router
    /// means *losing* whatever a hand-rolled route used to do on create, which
    /// is a reason not to adopt it.
    ///
    /// Runs **after** the comment's transaction has committed, so the row is
    /// already durable and visible to other connections. Its result is not
    /// awaited for correctness: a failing callback is logged and the request
    /// still succeeds, because a broken notifier must not un-post a comment
    /// the user already sees.
    pub on_comment: Option<CommentCreatedHook>,
}

/// The record-level authorization callback for [`CommentsConfig::authorize`].
///
/// Async because the interesting answers need the database: "is the viewer a
/// member of this ticket's project" is not a decision a synchronous predicate
/// over the request can make.
#[cfg(all(feature = "db", feature = "maud"))]
pub type CommentAuthorizer = std::sync::Arc<
    dyn Fn(CommentAccess) -> futures::future::BoxFuture<'static, bool> + Send + Sync,
>;

/// The post-create callback for [`CommentsConfig::on_comment`].
#[cfg(all(feature = "db", feature = "maud"))]
pub type CommentCreatedHook =
    std::sync::Arc<dyn Fn(CommentCreated) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// The comment [`CommentsConfig::on_comment`] is told about.
#[cfg(all(feature = "db", feature = "maud"))]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommentCreated {
    /// The model the comment was filed against, as stored in the
    /// discriminator column.
    pub commentable_type: String,
    /// The record it was filed against.
    pub parent_id: i64,
    /// The new comment's own id.
    pub comment_id: i64,
    /// The comment being replied to, if this is a reply.
    pub reply_to: Option<i64>,
    /// Who wrote it.
    pub author_id: i64,
    /// The body as accepted — already validated against the model's
    /// `max_body_bytes` and blank-body rules. Carried here so a notifier or a
    /// search indexer does not have to read back the row it was just told
    /// about.
    pub body: String,
}

/// What [`CommentsConfig::authorize`] is asked about.
#[cfg(all(feature = "db", feature = "maud"))]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommentAccess {
    /// The `commentable_type` path segment, already proven to name a
    /// registered model.
    pub commentable_type: String,
    /// The record's id, straight off the path and otherwise unvalidated.
    pub parent_id: i64,
    /// The signed-in author's id, or `None` for an anonymous reader.
    pub viewer_id: Option<i64>,
    /// `true` for the `POST` handler, `false` for the `GET` one — so a policy
    /// can allow reading while refusing to accept a comment.
    pub write: bool,
}

#[cfg(all(feature = "db", feature = "maud"))]
impl std::fmt::Debug for CommentsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommentsConfig")
            .field("mount_path", &self.mount_path)
            .field("session_author_key", &self.session_author_key)
            .field("sign_in_prompt", &self.sign_in_prompt)
            .field("label", &self.label)
            .field("authorize", &self.authorize.as_ref().map(|_| "<fn>"))
            .field("on_comment", &self.on_comment.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

#[cfg(all(feature = "db", feature = "maud"))]
impl CommentsConfig {
    /// Gate both handlers on a record-level check.
    ///
    /// ```rust,ignore
    /// CommentsConfig::default().authorize(|access| Box::pin(async move {
    ///     // e.g. re-use the app's own Policy for the parent record
    ///     access.viewer_id.is_some() || !private(&access.commentable_type, access.parent_id)
    /// }))
    /// ```
    ///
    /// A refusal is a `404`, not a `403`: telling an unauthorized caller that
    /// the record exists is an existence oracle, and it is the same answer they
    /// get for a record in another tenant.
    #[must_use]
    pub fn authorize<F>(mut self, authorize: F) -> Self
    where
        F: Fn(CommentAccess) -> futures::future::BoxFuture<'static, bool> + Send + Sync + 'static,
    {
        self.authorize = Some(std::sync::Arc::new(authorize));
        self
    }

    /// Set the post-create callback. See [`on_comment`](Self::on_comment).
    #[must_use]
    pub fn on_comment<F>(mut self, on_comment: F) -> Self
    where
        F: Fn(CommentCreated) -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static,
    {
        self.on_comment = Some(std::sync::Arc::new(on_comment));
        self
    }
}

#[cfg(all(feature = "db", feature = "maud"))]
impl Default for CommentsConfig {
    fn default() -> Self {
        Self {
            mount_path: "/comments".to_owned(),
            session_author_key: "user_id".to_owned(),
            sign_in_prompt: "Sign in to join the discussion.".to_owned(),
            label: "Comments".to_owned(),
            authorize: None,
            on_comment: None,
        }
    }
}

/// Mount the generic comment routes:
///
/// | Method | Path | Does |
/// |---|---|---|
/// | `GET` | `/{commentable_type}/{parent_id}` | render the thread fragment |
/// | `POST` | `/{commentable_type}/{parent_id}` | post a comment or reply, then re-render it |
///
/// `commentable_type` is matched against the [`inventory`] registry, so an
/// unregistered type is a `404` — the router can only ever reach a model that
/// actually declared `#[commentable]`.
///
/// ```rust,ignore
/// AppBuilder::new()
///     .nest("/comments", autumn_web::commentable::router(Default::default()))
/// ```
///
/// # Panics
///
/// If two `#[commentable]` models claim the same `commentable_type`. There is
/// no request-time answer that is not silently wrong — one model's rows would
/// be probed against the other's table — so this fails at wiring time, where
/// the fix (`#[commentable(type_name = "…")]`) is obvious.
///
/// **Mount it behind whatever authentication and CSRF middleware your app
/// already uses.** The `POST` handler reads the author's id from the session
/// key named in [`CommentsConfig::session_author_key`] and trusts it; it does
/// not itself authenticate, and it does not itself verify a CSRF token (autumn's
/// CSRF layer does, and the widget renders the hidden field for it).
#[cfg(all(feature = "db", feature = "maud"))]
pub fn router<S>(config: CommentsConfig) -> axum::Router<S>
where
    S: crate::db::DbState + Clone + Send + Sync + 'static,
{
    // Two models claiming one discriminator would render each other's threads
    // and probe each other's tables. The helpers' storage-scoped check runs
    // here too, for the shared-table case…
    assert_unique_discriminators();
    // …and the router additionally needs the STRICTER name-only rule. It
    // dispatches on `{commentable_type}` alone, so two models sharing a
    // discriminator are ambiguous at the URL even when their comments live in
    // different tables: `commentable_spec_for` returns whichever registered
    // first, and the other model is unreachable — or worse, served against the
    // wrong parent table. Storage isolation makes the helpers safe; it does
    // nothing for a route.
    assert!(
        duplicate_commentable_type().is_none(),
        "two #[commentable] models share the commentable_type {:?}, so the comment router \
         cannot tell `/comments/{}/…` apart: one of them would be unreachable. Give one a \
         `#[commentable(type_name = \"…\")]`, or serve them from your own routes instead of \
         mounting the generic router.",
        duplicate_commentable_type().unwrap_or_default(),
        duplicate_commentable_type().unwrap_or_default(),
    );
    // A sharded model routes every query through the shard its tenant selects;
    // this router checks out the control pool. There is no request-time answer
    // that is not silently wrong, so refuse at wiring time — the repository
    // helpers still work from the app's own (shard-aware) handlers.
    assert!(
        sharded_commentable_type().is_none(),
        "#[commentable] model {:?} is sharded, and the generic comment router cannot serve it: \
         it checks out the control database, while the model's repository helpers route through \
         the tenant's shard. Serve its comments from your own handlers using the generated \
         `{{Model}}Comments` methods.",
        sharded_commentable_type().unwrap_or_default(),
    );
    axum::Router::new()
        .route(
            "/{commentable_type}/{parent_id}",
            axum::routing::get(show_thread).post(post_comment),
        )
        .layer(axum::Extension(std::sync::Arc::new(config)))
}

/// The body of a comment/reply submission.
///
/// `reply_to` and `return_to` are `Option<String>` rather than their target
/// types because a browser submits an *empty* hidden input as `reply_to=`, and
/// `Option<i64>` would reject that outright rather than reading it as "not a
/// reply".
#[cfg(all(feature = "db", feature = "maud"))]
#[derive(Debug, serde::Deserialize)]
struct CommentSubmission {
    body: String,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    return_to: Option<String>,
}

/// The tenant this request is scoped to.
///
/// Fails closed, matching what a `tenant_scoped` repository does: when the
/// model carries a tenant column but no tenancy context was established, the
/// request is an error rather than a query against "the NULL tenant" — which
/// would quietly serve every untenanted row to a caller the middleware never
/// saw.
///
/// # Errors
///
/// [`AutumnError::internal_server_error_msg`] when the model is tenant-scoped
/// and no tenant is in scope: that is a middleware wiring mistake, not
/// something the caller can fix.
#[cfg(all(feature = "db", feature = "maud"))]
fn request_tenant(spec: &CommentableSpec) -> AutumnResult<Option<String>> {
    // A model with no tenant column emits no tenant predicate at all, so
    // reading the task-local would only ever produce a value nothing uses —
    // and neither does one whose repository deliberately opted out of
    // `tenant_scoped` while the model happens to carry a `tenant_id` column.
    let model = commentable_model_for_spec(spec).unwrap_or("");
    if !model_requires_tenant(model, spec.parent_tenant_column.is_some()) {
        return Ok(None);
    }
    crate::tenancy::CURRENT_TENANT
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .map(Some)
        .ok_or_else(|| {
            AutumnError::internal_server_error_msg(
                "This model is tenant-scoped but no tenant context was established for the \
                 comment routes — mount them inside the tenancy middleware.",
            )
        })
}

/// Run [`CommentsConfig::authorize`], if the app set one.
///
/// A refusal is `404` for the same reason a foreign-tenant parent is: a `403`
/// would confirm the record exists.
#[cfg(all(feature = "db", feature = "maud"))]
async fn authorize(
    config: &CommentsConfig,
    commentable_type: &str,
    parent_id: i64,
    viewer_id: Option<i64>,
    write: bool,
) -> AutumnResult<()> {
    let Some(authorize) = config.authorize.as_ref() else {
        return Ok(());
    };
    let allowed = authorize(CommentAccess {
        commentable_type: commentable_type.to_owned(),
        parent_id,
        viewer_id,
        write,
    })
    .await;
    if allowed {
        Ok(())
    } else {
        Err(AutumnError::not_found_msg("Comment target not found"))
    }
}

/// The authorization decision for one comment request, resolved as an
/// **extractor** rather than in the handler body.
///
/// This is load-bearing for pool safety, not a style choice. `Db` is an
/// eager extractor: by the time a handler body runs, its connection is already
/// checked out. `CommentsConfig::authorize` is documented as the place to run a
/// record-level policy check, and a policy check reads the database — so an
/// `authorize` call from the body would run while this request already holds a
/// connection, and at concurrency equal to the pool size every request could
/// pin one while waiting for a second that can never be issued.
///
/// axum runs extractors in argument order, so declaring this one **before**
/// `Db` makes "authorize, then check out" a property of the signature rather
/// than of a comment somebody could later reorder.
#[cfg(all(feature = "db", feature = "maud"))]
struct AuthorizedComment {
    /// The signed-in author, if any. `None` is allowed for a read and refused
    /// for a write.
    author_id: Option<i64>,
}

#[cfg(all(feature = "db", feature = "maud"))]
impl<S> axum::extract::FromRequestParts<S> for AuthorizedComment
where
    S: Send + Sync,
{
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let axum::Extension(config) =
            axum::Extension::<std::sync::Arc<CommentsConfig>>::from_request_parts(parts, state)
                .await
                .map_err(|_| {
                    AutumnError::internal_server_error_msg(
                        "comment router is not mounted with its CommentsConfig",
                    )
                })?;
        let axum::extract::Path((commentable_type, parent_id)) =
            axum::extract::Path::<(String, i64)>::from_request_parts(parts, state).await?;
        let session = crate::session::Session::from_request_parts(parts, state).await?;

        // `CommentAccess` promises the callback that `commentable_type` names a
        // registered model. Resolve it here, before the callback runs: an
        // unregistered type is a 404 whatever the app's policy would say, and
        // letting an arbitrary path string reach application code invites
        // policy or database work on a name that can never resolve -- and
        // breaks any callback that dispatches exhaustively over the models it
        // knows.
        resolve_spec(&commentable_type)?;

        // A write is the method that creates a comment; everything else reads.
        let write = parts.method == axum::http::Method::POST;
        let author_id = session_author(&session, &config).await;
        if write && author_id.is_none() {
            return Err(AutumnError::unauthorized_msg("Sign in to comment"));
        }

        authorize(&config, &commentable_type, parent_id, author_id, write).await?;
        Ok(Self { author_id })
    }
}

/// Render the thread for one record.
#[cfg(all(feature = "db", feature = "maud"))]
async fn show_thread(
    axum::Extension(config): axum::Extension<std::sync::Arc<CommentsConfig>>,
    axum::extract::Path((commentable_type, parent_id)): axum::extract::Path<(String, i64)>,
    axum::extract::Query(query): axum::extract::Query<ThreadQuery>,
    csrf: Option<crate::security::csrf::CsrfToken>,
    csrf_field: Option<crate::security::csrf::CsrfFormField>,
    // Declared before `Db` on purpose: authorization runs (and may read the
    // database) before this request holds a pooled connection. See
    // `AuthorizedComment`.
    AuthorizedComment { author_id }: AuthorizedComment,
    mut db: crate::db::Db,
) -> AutumnResult<maud::Markup> {
    let spec = resolve_spec(&commentable_type)?;
    let tenant = request_tenant(spec)?;
    let thread = comment_thread(
        &mut db,
        spec,
        &commentable_type,
        parent_id,
        tenant.as_deref(),
    )
    .await?;
    Ok(render(
        &config,
        spec,
        &commentable_type,
        parent_id,
        &thread,
        csrf.as_ref(),
        csrf_field.as_ref(),
        author_id.is_some(),
        query
            .return_to
            .as_deref()
            .filter(|p| is_safe_return_path(p)),
        None,
        None,
    ))
}

/// `?return_to=` on the `GET` fragment, so a thread served by the router can
/// round-trip a no-JS submit back to the page that embedded it.
#[cfg(all(feature = "db", feature = "maud"))]
#[derive(Debug, Default, serde::Deserialize)]
struct ThreadQuery {
    #[serde(default)]
    return_to: Option<String>,
}

/// Post a comment (or a reply) and re-render the thread.
#[cfg(all(feature = "db", feature = "maud"))]
#[allow(clippy::too_many_arguments)] // Every argument is a distinct axum
// extractor -- config, path, session, CSRF token, CSRF field name, htmx
// detection, the connection, and the form body. An axum handler's arguments
// ARE its request-state declaration; bundling them into a struct would only
// move the same list one level down.
async fn post_comment(
    axum::Extension(config): axum::Extension<std::sync::Arc<CommentsConfig>>,
    axum::extract::Path((commentable_type, parent_id)): axum::extract::Path<(String, i64)>,
    csrf: Option<crate::security::csrf::CsrfToken>,
    csrf_field: Option<crate::security::csrf::CsrfFormField>,
    htmx: crate::htmx::HxRequest,
    // Before the checkout, as in `show_thread`: a policy check that reads the
    // database must not run while this request is already holding a connection.
    AuthorizedComment { author_id }: AuthorizedComment,
    // NOT `Db`. An eager checkout here would be taken before axum runs the
    // `Form` extractor below, so the connection would be held for as long as
    // the client takes to send its body — which the client chooses. Enough
    // slow-body requests would pin the whole pool.
    deferred_db: crate::db::DeferredDb,
    axum::extract::Form(submission): axum::extract::Form<CommentSubmission>,
) -> AutumnResult<axum::response::Response> {
    use axum::response::IntoResponse as _;

    let spec = resolve_spec(&commentable_type)?;
    // The extractor refuses an unauthenticated write, so this is always `Some`.
    let author_id = author_id.ok_or_else(|| AutumnError::unauthorized_msg("Sign in to comment"))?;
    // An empty hidden input means "top-level", not "malformed": the widget
    // renders `reply_to` only on the per-node forms.
    let reply_to = match submission.reply_to.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(
            raw.parse::<i64>()
                .map_err(|_| AutumnError::bad_request_msg("Invalid reply target"))?,
        ),
    };
    let tenant = request_tenant(spec)?;
    // Only a relative, single-slash path is ever honoured, so a crafted
    // `return_to` cannot become an open redirect — and the same validated value
    // is echoed back into the re-rendered forms, so the NEXT no-JS submit still
    // knows where to come back to.
    let return_to = submission
        .return_to
        .as_deref()
        .filter(|path| is_safe_return_path(path));

    // The body is read and validated, so take the connection now. A malformed
    // submission is rejected above without ever touching the pool.
    let mut db = deferred_db.checkout().await?;

    let outcome = add_comment(
        &mut db,
        spec,
        &commentable_type,
        parent_id,
        author_id,
        &submission.body,
        reply_to,
        tenant.as_deref(),
    )
    .await;

    // A rejected body is shown, not thrown. htmx does not swap a non-2xx
    // response by default, so returning the error would make the button look
    // broken; re-rendering the thread with the message above the form is the
    // only feedback a no-JS visitor gets either.
    //
    // The created comment is carried past this match so the hook can be told
    // about it *after* this request has let go of its connection -- see below.
    let (created, error) = match outcome {
        Ok(created) => (Some(created), None),
        Err(err) if err.status() == http::StatusCode::UNPROCESSABLE_ENTITY => {
            (None, Some(err.to_string()))
        }
        Err(err) => return Err(err),
    };
    let redirecting = error.is_none() && !htmx.is_htmx && return_to.is_some();

    // Read everything this response still needs from the database BEFORE the
    // hook runs, so the connection can be released first.
    //
    // The result is HELD rather than `?`-propagated: the comment is already
    // committed by this point, and a transient failure re-reading the thread
    // must not cost it its notification, live-feed entry or search-index write.
    // Returning early here would leave a durable comment the rest of the system
    // never hears about — a silent inconsistency worse than the refresh failure
    // the caller is about to be told about anyway.
    let thread = if redirecting {
        Ok(Vec::new())
    } else {
        comment_thread(
            &mut db,
            spec,
            &commentable_type,
            parent_id,
            tenant.as_deref(),
        )
        .await
    };

    // Release the checkout before awaiting the hook. `on_comment` is documented
    // as the place for a database-backed side effect -- a notification that
    // resolves a username, a search index write -- and holding this connection
    // across that await would let `pool_size` concurrent submissions each pin
    // one while waiting for a second that can never be issued. The row is
    // already committed, so nothing here needs the connection any more.
    drop(db);

    if let Some(created) = created
        && let Some(hook) = config.on_comment.as_ref()
    {
        hook(CommentCreated {
            commentable_type: commentable_type.clone(),
            parent_id,
            comment_id: created.id,
            reply_to,
            author_id,
            // The body as *accepted*, not as submitted: `add_comment` trims
            // before validating and inserting, so the submitted form value can
            // differ from the stored row -- and padding a short body with
            // whitespace would otherwise hand the hook a payload larger than
            // `max_body_bytes` claims to allow.
            body: created.body,
        })
        .await;
    }

    // Now that the hook has run for the committed comment, a failed refresh can
    // surface.
    let thread = thread?;

    // The draft rides with the error, never without it: these two are the same
    // event — "we refused this, here it is back".
    //
    // Pinned to a reply form only if the REFRESHED thread still has one for
    // that comment. A reply target deleted between render and submit — or one
    // a client invented — leaves no form for the widget to fill, and the
    // `outerHTML` swap would erase the body exactly as it did before drafts
    // were carried at all. Falling back to the top-level form keeps the text on
    // screen where the visitor can copy it, which is the whole point.
    let draft = error.is_some().then(|| {
        let target = reply_to.filter(|id| reply_form_exists(&thread, *id, spec.max_depth));
        (target, submission.body.clone())
    });

    if redirecting && let Some(return_to) = return_to {
        return Ok(crate::Redirect::to(return_to).into_response());
    }

    Ok(render(
        &config,
        spec,
        &commentable_type,
        parent_id,
        &thread,
        csrf.as_ref(),
        csrf_field.as_ref(),
        true,
        return_to,
        error,
        // Only on rejection: a successful submit renders the new comment and
        // must leave the textarea empty, or the visitor is invited to post it
        // twice. `submission.body` is the raw text as typed, not the trimmed
        // value `add_comment` stored, so nothing the visitor wrote is lost.
        draft,
    )
    .into_response())
}

/// Whether the refreshed thread will render a reply form for `target`.
///
/// Mirrors the widget's own `can_reply`: a form is offered only while the
/// comment's depth is still under [`CommentableSpec::max_depth`], because the
/// write path refuses a deeper reply. A draft pinned to a comment with no form
/// would be dropped silently by the `outerHTML` swap.
#[cfg(all(feature = "db", feature = "maud"))]
fn reply_form_exists(nodes: &[CommentNode], target: i64, max_depth: u32) -> bool {
    nodes.iter().any(|node| {
        (node.comment.id == target && node.depth < max_depth as usize)
            || reply_form_exists(&node.replies, target, max_depth)
    })
}

/// Whether `path` is a same-origin relative path safe to `Location:`.
///
/// `//evil.example` and `https://evil.example` are browser-absolute; a
/// backslash is normalised to `/` by some browsers, so it is rejected too.
///
/// **Every** byte at or below `0x20` is rejected, not just CR/LF. The WHATWG
/// URL parser strips ASCII tab, LF and CR from a URL *before* parsing, and
/// `HeaderValue` happily carries a tab — so `/\t/evil.example` would pass a
/// CR/LF-only check, reach the browser intact, and be re-read as
/// `//evil.example`. The same blanket check keeps `DEL` and the other control
/// characters from reaching `HeaderValue::try_from`, which would otherwise turn
/// a bad request into a `500`.
#[cfg(all(feature = "db", feature = "maud"))]
fn is_safe_return_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && path.bytes().all(|byte| byte > 0x20 && byte != 0x7f)
}

/// Look a `commentable_type` up in the registry.
#[cfg(all(feature = "db", feature = "maud"))]
fn resolve_spec(commentable_type: &str) -> AutumnResult<&'static CommentableSpec> {
    commentable_spec_for(commentable_type)
        .ok_or_else(|| AutumnError::not_found_msg("Unknown commentable type"))
}

/// The signed-in author's id, when the session carries one.
#[cfg(all(feature = "db", feature = "maud"))]
async fn session_author(session: &crate::session::Session, config: &CommentsConfig) -> Option<i64> {
    session
        .get(&config.session_author_key)
        .await
        .and_then(|raw| raw.trim().parse::<i64>().ok())
}

/// Build the widget markup for one thread.
#[cfg(all(feature = "db", feature = "maud"))]
#[allow(clippy::too_many_arguments)] // Straight-line assembly of the widget's
// own configuration; every argument is a distinct piece of request state.
fn render(
    config: &CommentsConfig,
    spec: &CommentableSpec,
    commentable_type: &str,
    parent_id: i64,
    thread: &[CommentNode],
    csrf: Option<&crate::security::csrf::CsrfToken>,
    csrf_field: Option<&crate::security::csrf::CsrfFormField>,
    can_comment: bool,
    return_to: Option<&str>,
    error: Option<String>,
    // The rejected body and the form it came from, so a 422 re-render gives the
    // visitor their draft back instead of an empty textarea.
    draft: Option<(Option<i64>, String)>,
) -> maud::Markup {
    let mut widget = crate::widgets::CommentThread::from_spec(
        thread_dom_id(commentable_type, parent_id),
        thread_action(config, commentable_type, parent_id),
        spec,
    )
    .label(config.label.clone());
    if let Some(csrf) = csrf {
        widget = widget.csrf_token(csrf.token());
    }
    // `CsrfLayer` scans a URL-encoded body for the CONFIGURED field name only
    // (unlike the query-string path, which also accepts `_csrf`), so an app
    // that set `security.csrf.form_field` needs the widget's hidden input
    // renamed to match. Without this the fragment renders perfectly and every
    // no-JavaScript submit from it is a 403.
    if let Some(field) = csrf_field {
        widget = widget.csrf_field(field.0.clone());
    }
    if let Some(return_to) = return_to {
        widget = widget.return_to(return_to);
    }
    if let Some(error) = error {
        widget = widget.error(error);
    }
    if let Some((reply_to, body)) = draft {
        widget = widget.draft(reply_to, body);
    }
    if !can_comment {
        widget = widget
            .read_only()
            .sign_in_prompt(config.sign_in_prompt.clone());
    }
    crate::widgets::comment_thread(&widget, &crate::widgets::CommentView::from_thread(thread))
}

/// The DOM id the router renders a thread into.
///
/// **A host page that embeds a thread itself must use this same id**, or the
/// first htmx swap replaces its region with one carrying a different id and
/// every later swap misses. [`crate::widgets::CommentThread::from_spec`] and
/// this function together are what keep the two renders identical.
#[cfg(all(feature = "db", feature = "maud"))]
#[must_use]
pub fn thread_dom_id(commentable_type: &str, parent_id: i64) -> String {
    format!("autumn-comments-{commentable_type}-{parent_id}")
}

/// The form action the router serves, for a host page rendering its own
/// thread.
#[cfg(all(feature = "db", feature = "maud"))]
#[must_use]
pub fn thread_action(config: &CommentsConfig, commentable_type: &str, parent_id: i64) -> String {
    format!(
        "{}/{commentable_type}/{parent_id}",
        config.mount_path.trim_end_matches('/')
    )
}
