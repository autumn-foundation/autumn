//! Database-model proc macros for the Autumn web framework.
//!
//! This crate provides the database-layer attribute macros:
//! `#[model]`, `#[commentable]` (consumed by `#[model]`), and `#[service]`.
//! It is split out of `autumn-macros` so a build that never touches the
//! database layer never compiles this codegen.
//!
//! Users should not depend on this crate directly — use `autumn-web`
//! instead, which re-exports everything behind its `db` feature.

mod commentable;
mod model;
mod service;

use proc_macro::TokenStream;

/// Attribute macro for Autumn database models.
///
/// Applies Diesel (`Queryable`, `Selectable`, `Insertable`) and Serde
/// (`Serialize`, `Deserialize`) derives, plus a `#[diesel(table_name)]`
/// attribute. The table name can be specified explicitly or inferred
/// from the struct name by converting `PascalCase` to `snake_case`
/// and appending `s`.
///
/// # Examples
///
/// Explicit table name:
///
/// ```ignore
/// use autumn_web::model;
///
/// #[model(table = "users")]
/// pub struct User {
///     pub id: i64,
///     pub name: String,
/// }
/// ```
///
/// Inferred table name (`BlogPost` -> `blog_posts`):
///
/// ```ignore
/// use autumn_web::model;
///
/// #[model]
/// pub struct BlogPost {
///     pub id: i64,
///     pub title: String,
/// }
/// ```
///
/// # Associations
///
/// Declare `#[belongs_to]`, `#[has_many]`, and `#[has_one]` on the struct to
/// get batched eager preloading for free — no hand-written join queries, no
/// N+1. Foreign keys and accessor names are inferred from the target's type
/// name, with `fk = ...` / `name = ...` overrides:
///
/// ```ignore
/// #[model]
/// #[belongs_to(User, fk = author_id)]  // fk on THIS model
/// #[has_many(Comment)]                 // fk (post_id) on the TARGET
/// pub struct Post {
///     #[id]
///     pub id: i64,
///     pub author_id: i64,
///     pub title: String,
/// }
/// ```
///
/// Preload associations through a repository (`Model::preload()` builds the
/// spec; `_with` nests into the related model's own associations):
///
/// ```ignore
/// let posts = repo.find_all().await?;
/// let posts = repo.preload(posts, Post::preload().author().comments()).await?;
/// for post in &posts {
///     let author = post.author()?;      // Result<Option<&Preloaded<User>>, NotLoaded>
///     let comments = post.comments()?;  // Result<&[Preloaded<Comment>], NotLoaded>
/// }
/// ```
///
/// An association that was not preloaded returns `NotLoaded` from its
/// accessor rather than issuing SQL — autumn never lazy-loads.
///
/// ## Many-to-many (`through =`)
///
/// Add `through = <join_table>` to `#[has_many]` to declare a many-to-many
/// association backed by a join table, with the same batched preload
/// semantics as `belongs_to`/`has_many`/`has_one`:
///
/// ```ignore
/// #[model]
/// #[has_many(Tag, through = post_tags)]  // join columns default to post_id / tag_id
/// pub struct Post {
///     #[id]
///     pub id: i64,
///     pub title: String,
/// }
/// ```
///
/// Join columns default to `{source}_id` / `{target}_id` and can be
/// overridden with `fk = ...` and `target_fk = ...`; the join table itself
/// needs no hand-written `diesel::table!` — the macro emits one and requires
/// a composite primary key on `(fk, target_fk)`:
///
/// ```sql
/// CREATE TABLE post_tags (
///     post_id BIGINT NOT NULL REFERENCES posts(id),
///     tag_id  BIGINT NOT NULL REFERENCES tags(id),
///     PRIMARY KEY (post_id, tag_id)
/// );
/// ```
///
/// `Post::preload().tags()` issues one batched `INNER JOIN` query (plus one
/// more per level of `_with` nesting) — a fixed number of queries regardless
/// of how many tags each post has. The generated `tags()` accessor returns
/// `&[Arc<Preloaded<Tag>>]` (rather than `has_many`'s plain
/// `&[Preloaded<Tag>]`): the same tag can legitimately be linked to more than
/// one currently-loaded post, so it's shared via `Arc` instead of being
/// duplicated per parent.
///
/// The association also generates three mutation helpers on the model's
/// `#[repository]` — `add_{singular}`, `remove_{singular}`, and
/// `set_{plural}` (replace-all) — each idempotent and requiring no
/// hand-written SQL:
///
/// ```ignore
/// repo.add_tag(post_id, tag_id).await?;      // idempotent: ON CONFLICT DO NOTHING
/// repo.remove_tag(post_id, tag_id).await?;   // idempotent: no-op if unlinked
/// repo.set_tags(post_id, &tag_ids).await?;   // replace-all, one transaction
/// ```
///
/// The `add_`/`remove_` singular is derived from the target *type* name, so a
/// model may declare at most one m2m association per target type by default —
/// a second one to the same target would generate colliding helpers (a compile
/// error). To declare two m2m associations to the same target (e.g. a
/// self-referential `followers`/`following` pair through one `Friendship` join
/// table), give each a distinct explicit `helper = "..."` override, which sets
/// the singular used for its `add_`/`remove_` helpers:
///
/// ```ignore
/// #[model]
/// #[has_many(User, through = friendships, name = followers,
///            fk = followed_id, target_fk = follower_id, helper = "follower")]
/// #[has_many(User, through = friendships, name = following,
///            fk = follower_id, target_fk = followed_id, helper = "following")]
/// pub struct User { /* ... */ }
/// // -> add_follower/remove_follower and add_following/remove_following
/// ```
///
/// # Votable (reactions)
///
/// `#[votable(by = <Reactor>)]` declares a reaction association (#1362): a
/// `(reactor, target)`-unique edge table plus an aggregate column maintained on
/// this model. It replaces the hand-written toggle/flip/upsert SQL and the
/// score recompute that every voting, liking or bookmarking feature otherwise
/// grows.
///
/// ```ignore
/// #[model]
/// #[votable(by = User, aggregate = sum)]   // signed up/down votes
/// pub struct Post {
///     #[id]
///     pub id: i64,
///     pub title: String,
///     pub score: i64,                      // the aggregate column
/// }
/// ```
///
/// Two modes: `aggregate = sum` (the default — signed values, `score =
/// SUM(value)`) and `aggregate = count` (unary likes — no value column,
/// `{name}_count = COUNT(*)`). Every name is inferred and every inference has
/// an override:
///
/// | Key | Default | Meaning |
/// |---|---|---|
/// | `by` | **required** | the reactor model, e.g. `User` |
/// | `aggregate` | `sum` | `sum` \| `count` |
/// | `name` | `vote` | reaction name; drives `table` and the count column |
/// | `table` | `pluralize(name)` → `votes` | the edge table |
/// | `reactor_fk` | `{snake(by)}_id` → `user_id` | edge column → reactor |
/// | `target_fk` | `{snake(Model)}_id` → `post_id` | edge column → this model |
/// | `value_column` | `value` (sum only) | the edge's signed value |
/// | `column` | `score` (sum) / `{name}_count` (count) | aggregate column |
///
/// A likes feature is therefore `#[votable(by = User, aggregate = count, name
/// = like)]` → table `likes`, column `like_count`. At most one `#[votable]` per
/// model.
///
/// `by` may name a hand-written struct — it is name-resolved at compile time
/// but carries no trait bound, so the reactor's `i64` primary key is
/// documented contract, not a compile check (the edge table binds the reactor
/// FK as `BIGINT`; a UUID-keyed reactor fails on first use with a database
/// type error). The **target** model's `#[id]` and aggregate column *are*
/// compile-checked as `i64`.
///
/// **Write `#[votable]` *below* `#[model]`.** It is consumed by `#[model]`, not
/// registered as an attribute in its own right, so an attribute macro written
/// above it never sees it — an error reading `cannot find attribute `votable`
/// in this scope` means the two lines are the wrong way round.
///
/// ## Required migration
///
/// The edge table is the user's to create, and its **composite `UNIQUE
/// (reactor_fk, target_fk)` is load-bearing**: it is the `ON CONFLICT` arbiter
/// the generated upsert names, and it is what makes "at most one edge per
/// (reactor, target)" a database guarantee. The value column is `SMALLINT`, the
/// aggregate column `BIGINT NOT NULL DEFAULT 0`, and the model's own primary key
/// must be `BIGINT`/`i64` (both edge foreign keys are bound as `i64`; a
/// UUID-keyed model is a compile error).
///
/// `NOT NULL` on both foreign keys is strongly recommended: `NULL`s are
/// distinct in a unique constraint, so a nullable column is not covered by the
/// arbiter. A nullable *target* FK is nevertheless tolerated when every row this
/// association writes is non-`NULL` — the shape an XOR edge table has (reddit-
/// clone's `votes` points at either a post or a comment), where the unique
/// constraint still fully covers the non-`NULL` rows `react()` creates.
///
/// The `CHECK` on `value` is load-bearing in sum mode: **`react()` does not
/// validate `value`** — it writes what it is given, and the sum is only
/// meaningful because the database refuses anything outside the legal set.
/// Never bind `value` straight from a request; map the request to `1` / `-1`
/// yourself. A violating value surfaces as a database error (a 500), not a
/// validation failure.
///
/// ```sql
/// CREATE TABLE votes (
///     id      BIGSERIAL PRIMARY KEY,
///     user_id BIGINT NOT NULL REFERENCES users(id),
///     post_id BIGINT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
///     value   SMALLINT NOT NULL CHECK (value IN (-1, 1)),
///     UNIQUE (user_id, post_id)          -- the ON CONFLICT arbiter
/// );
/// ALTER TABLE posts ADD COLUMN score BIGINT NOT NULL DEFAULT 0;
/// -- aggregate = count: drop the `value` column entirely.
/// ```
///
/// ## Generated helpers
///
/// A `{Model}Reactions` trait, blanket-implemented for the model's
/// `#[repository]`:
///
/// ```ignore
/// use autumn_web::repository::{Reaction, ReactionOutcome};
///
/// // sum mode. (count mode: `react(reactor_id, target_id)` — no value.)
/// let r: Reaction = posts.react(user_id, post_id, 1).await?;
/// r.value;      // Option<i16>: the reactor's reaction AFTER the call
/// r.aggregate;  // i64: the newly persisted score, ground truth at commit
/// r.outcome;    // Inserted | Flipped | Removed
///
/// let mine: Option<i16> = posts.reaction_of(user_id, post_id).await?;
/// ```
///
/// `react()` is race-safe: the same value again toggles the edge off, a
/// different value flips it in place, a new one inserts it — and the aggregate
/// is recomputed from ground truth (`SUM`/`COUNT`) and persisted in the **same
/// transaction**, so a reader never observes edge/aggregate disagreement. The
/// target row is locked (`SELECT ... FOR NO KEY UPDATE` on Postgres — it does
/// not conflict with the `FOR KEY SHARE` locks foreign-key checks take, so
/// concurrent inserts referencing the target do not queue behind votes;
/// `BEGIN IMMEDIATE` on `SQLite`) for the whole read-decide-write-recompute
/// window, so concurrent reactions on one target converge to at most one edge
/// per `(reactor, target)` and the persisted aggregate is exact even across
/// *different* reactors.
///
/// It is **not idempotent** — it is a toggle. Retrying a call that timed out can
/// invert the outcome, because the first attempt may have committed; callers
/// that need retry safety dedupe above this layer (an idempotency key on the
/// HTTP request). `reaction_of()` is a plain read: it follows the repository's
/// read route (so a replica may serve it) and does not pin read-your-writes, so
/// render from the `Reaction` that `react()` returned rather than re-reading.
///
/// When the model has a `deleted_at` field, reacting to a soft-deleted target
/// is `NotFound` and leaves its aggregate untouched.
///
/// Tenant-isolated on the same terms: when the model has a `tenant_id` field
/// **and** the repository is `#[repository(..., tenant_scoped)]`, both the
/// target lock and the aggregate `UPDATE` carry `tenant_id = <current
/// tenant>`, so another tenant's `target_id` is `NotFound` before any write and
/// `reaction_of()` reports `None` for it. No tenant context is an error (as for
/// any derived query) and `across_tenants()` opts out. A model without the
/// column emits none of this. The m2m `add_*` / `remove_*` helpers are not
/// covered — they remain id-scoped.
///
/// `react()` acquires its **own** pooled connection and does not join an
/// enclosing `Db::tx` — do not hold a `Db` extractor across the call on a small
/// connection pool.
#[proc_macro_attribute]
pub fn model(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) =
        match autumn_macros_support::crate_path::extract_crate_override(attr.into()) {
            Ok(pair) => pair,
            Err(err) => return err.into(),
        };
    let _guard = autumn_macros_support::crate_path::set_target(crate_override.as_deref());
    autumn_macros_support::crate_path::finalize(model::model_macro(attr, item.into())).into()
}

/// Define a service for cross-model orchestration and non-DB side effects.
///
/// Generates a `XxxServiceImpl` struct with dependency injection via
/// `FromRequestParts`, so it can be used as a handler parameter just
/// like repositories.
///
/// Use `#[service]` when your logic orchestrates **multiple repositories**
/// or involves **non-DB side effects** (email, API calls, etc.).
/// For single-model CRUD and validation, use `#[repository]` instead.
///
/// # Examples
///
/// ```ignore
/// use autumn_web::service;
///
/// #[service]
/// pub trait OrderService {
///     fn deps(order_repo: PgOrderRepository, inventory_repo: PgInventoryRepository);
///
///     async fn place_order(&self, req: PlaceOrderRequest) -> AutumnResult<Order>;
/// }
///
/// // You implement the business logic:
/// impl OrderServiceImpl {
///     pub async fn place_order(&self, req: PlaceOrderRequest) -> AutumnResult<Order> {
///         let order = self.order_repo.save(&req.into()).await?;
///         self.inventory_repo.reserve(order.id).await?;
///         Ok(order)
///     }
/// }
///
/// // Then use it in handlers, just like a repository:
/// #[get("/orders/{id}")]
/// async fn get_order(svc: OrderServiceImpl) -> AutumnResult<Json<Order>> {
///     // ...
/// }
/// ```
#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) =
        match autumn_macros_support::crate_path::extract_crate_override(attr.into()) {
            Ok(pair) => pair,
            Err(err) => return err.into(),
        };
    let _guard = autumn_macros_support::crate_path::set_target(crate_override.as_deref());
    autumn_macros_support::crate_path::finalize(service::service_macro(attr, item.into())).into()
}
