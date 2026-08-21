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

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
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
    /// The model's binding.
    pub spec: &'static CommentableSpec,
}

inventory::collect!(CommentableDescriptor);

/// The spec registered for `type_name`, or `None` when no `#[commentable]`
/// model in this binary claims it.
#[must_use]
pub fn commentable_spec_for(type_name: &str) -> Option<&'static CommentableSpec> {
    inventory::iter::<CommentableDescriptor>()
        .find(|descriptor| descriptor.type_name == type_name)
        .map(|descriptor| descriptor.spec)
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
            // `*parent < position` always holds for a well-formed thread (a
            // parent is inserted before its reply, and the query is ordered by
            // creation), but a clock skew or a hand-edited row could invert it.
            // Treating such a row as a root keeps the build acyclic.
            Some(&parent) if parent != position => children[parent].push(position),
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
    let live = if spec.parent_soft_delete {
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

    let ids: Vec<IdRow> = diesel::sql_query(format!(
        "WITH RECURSIVE __autumn_cmt_sub(id, depth) AS (\
           SELECT c.{pk}, CAST(0 AS BIGINT) FROM {comments} AS c \
            WHERE c.{pk} = {} AND c.{type_column} = {} AND c.{id_column} = {}{anchor_live} \
           UNION ALL \
           SELECT d.{pk}, s.depth + 1 \
             FROM {comments} AS d JOIN __autumn_cmt_sub AS s ON d.{parent_column} = s.id \
            WHERE s.depth < {RECURSION_GUARD} \
              AND d.{type_column} = {} AND d.{id_column} = {}{descendant_live}\
         ) SELECT id FROM __autumn_cmt_sub",
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
    .load::<IdRow>(conn)
    .await
    .map_err(AutumnError::from)?;

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
    // and probe each other's tables. There is no request-time answer that is
    // not silently wrong, so fail at wiring time, where the fix is obvious.
    assert!(
        duplicate_commentable_type().is_none(),
        "two #[commentable] models share the commentable_type {:?}: the shared comments \
         table cannot tell their rows apart. Give one of them \
         `#[commentable(type_name = \"…\")]`.",
        duplicate_commentable_type().unwrap_or_default(),
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
    // reading the task-local would only ever produce a value nothing uses.
    if spec.parent_tenant_column.is_none() {
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

/// Render the thread for one record.
#[cfg(all(feature = "db", feature = "maud"))]
async fn show_thread(
    axum::Extension(config): axum::Extension<std::sync::Arc<CommentsConfig>>,
    axum::extract::Path((commentable_type, parent_id)): axum::extract::Path<(String, i64)>,
    axum::extract::Query(query): axum::extract::Query<ThreadQuery>,
    session: crate::session::Session,
    csrf: Option<crate::security::csrf::CsrfToken>,
    csrf_field: Option<crate::security::csrf::CsrfFormField>,
    mut db: crate::db::Db,
) -> AutumnResult<maud::Markup> {
    let spec = resolve_spec(&commentable_type)?;
    let author_id = session_author(&session, &config).await;
    authorize(&config, &commentable_type, parent_id, author_id, false).await?;
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
    session: crate::session::Session,
    csrf: Option<crate::security::csrf::CsrfToken>,
    csrf_field: Option<crate::security::csrf::CsrfFormField>,
    htmx: crate::htmx::HxRequest,
    mut db: crate::db::Db,
    axum::extract::Form(submission): axum::extract::Form<CommentSubmission>,
) -> AutumnResult<axum::response::Response> {
    use axum::response::IntoResponse as _;

    let spec = resolve_spec(&commentable_type)?;
    let author_id = session_author(&session, &config)
        .await
        .ok_or_else(|| AutumnError::unauthorized_msg("Sign in to comment"))?;
    authorize(&config, &commentable_type, parent_id, Some(author_id), true).await?;
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
    let error = match outcome {
        Ok(_) => None,
        Err(err) if err.status() == http::StatusCode::UNPROCESSABLE_ENTITY => Some(err.to_string()),
        Err(err) => return Err(err),
    };

    if error.is_none()
        && !htmx.is_htmx
        && let Some(return_to) = return_to
    {
        return Ok(crate::Redirect::to(return_to).into_response());
    }

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
        true,
        return_to,
        error,
    )
    .into_response())
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
