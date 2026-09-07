#![allow(clippy::collapsible_else_if)]
//! # Autumn Macros
//!
//! Proc macros for the Autumn web framework.
//!
//! This crate provides:
//! - Route annotation macros (`#[get]`, `#[post]`, etc.)
//! - The `routes![]` collection macro
//! - The `#[autumn_web::main]` entry point macro (S-008)
//! - The `#[model]` attribute macro (S-018)
//!
//! Users should not depend on this crate directly — use `autumn-web` instead,
//! which re-exports everything.
//!
//! # Renamed or multi-versioned `autumn-web` dependencies (#1828)
//!
//! Every macro generates code that ultimately refers back to types in
//! `autumn-web` via absolute (`::`-rooted) paths like `::autumn_web::Route`.
//! By default that name is resolved automatically — via
//! [`proc-macro-crate`](https://docs.rs/proc-macro-crate) — from the
//! invoking crate's own `Cargo.toml`, so renaming the dependency (`web = {
//! package = "autumn-web" }`) just works with no macro changes.
//!
//! A crate that must depend on **two** differently-keyed copies of
//! `autumn-web` at once (e.g. mid-upgrade) is inherently ambiguous for that
//! automatic detection. Every attribute macro (`#[get]`, `#[model]`,
//! `#[repository]`, …) therefore also accepts an explicit `crate = "..."`
//! argument naming the extern-prelude identifier to use instead, e.g.
//! `#[get("/x", crate = "autumn_web_05")]`.

mod agent_authority;
mod api_doc;
mod authorize;
mod cached;
mod collect;
#[cfg(feature = "db")]
mod commentable;
mod crate_path;
mod edge;
mod edge_routes_macro;
mod event;
mod feature_flag;
mod graph;
mod i18n;
mod idempotency_guard;
mod inbound_mail;
mod job;
mod jobs_macro;
mod lifecycle;
mod listener;
mod listeners_macro;
mod mail_previews_macro;
mod mailer;
mod mailer_preview;
mod main_macro;
#[cfg(feature = "db")]
mod model;
mod oauth2_callback;
mod one_off_task;
mod one_off_tasks_macro;
mod openapi_schema;
mod param_helpers;
mod parse;
mod paths_macro;
mod public;
mod query_budget;
#[cfg(feature = "db")]
mod repository;
mod route;
mod routes_macro;
mod scheduled;
mod schema;
mod secured;
mod service;
mod sim_test;
mod static_route;
mod static_routes_macro;
mod step_up;
mod story_macro;
mod tasks_macro;
mod throttle;
mod ws;

use proc_macro::TokenStream;

/// Annotate an async function as a GET route handler.
///
/// Generates a companion `__autumn_route_info_{name}()` function that
/// returns a `Route` pairing the path with an Axum
/// handler. In debug builds, `#[axum::debug_handler]` is automatically
/// applied for improved error messages. This has zero cost in release
/// builds.
///
/// # Example
///
/// ```ignore
/// use autumn_web::get;
///
/// #[get("/hello")]
/// async fn hello() -> &'static str {
///     "Hello, Autumn!"
/// }
/// ```
///
/// # Route-level SEO defaults
///
/// A `seo(...)` argument declares per-page meta tag values once on the route
/// instead of rebuilding them in every handler. Take a `SeoMeta` parameter and
/// it arrives pre-populated; the builder is consuming, so the handler refines
/// the defaults with per-request data:
///
/// ```ignore
/// use autumn_web::get;
/// use autumn_web::seo::SeoMeta;
///
/// #[get("/about", seo(title = "About • My Blog", description = "Learn about us"))]
/// async fn about(seo: SeoMeta) -> Markup {
///     html! { head { (seo.render()) } }
/// }
///
/// #[get("/posts/{slug}", seo(og_type = "article"))]
/// async fn show(slug: Path<String>, seo: SeoMeta) -> Markup {
///     let seo = seo.title(format!("{} • Blog", *slug));
///     html! { head { (seo.render()) } }
/// }
/// ```
///
/// Accepted keys mirror the `SeoMeta` builder: `title`, `description`,
/// `canonical`, `og_title`, `og_description`, `og_image`, `og_type`, `og_url`,
/// `twitter_card`, `twitter_title`, `twitter_description`, `twitter_image`,
/// and `robots`. Values must be string literals; an unknown, repeated, or
/// empty `seo(...)` is a compile error. Every HTTP route macro accepts the
/// argument, as does [`macro@static_get`]; [`macro@ws`] does not, since a
/// WebSocket upgrade serves no crawlable document.
///
/// The argument supplies *values*, not markup: a handler that never takes a
/// `SeoMeta` parameter renders nothing regardless of what the attribute
/// declares.
#[proc_macro_attribute]
pub fn get(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(route::route_macro("GET", "get", attr, item.into())).into()
}

/// Annotate an async function as a POST route handler.
///
/// Generates a companion `__autumn_route_info_{name}()` function that
/// returns a `Route` pairing the path with an Axum
/// handler. In debug builds, `#[axum::debug_handler]` is automatically
/// applied for improved error messages. This has zero cost in release
/// builds.
///
/// # Example
///
/// ```ignore
/// use autumn_web::post;
///
/// #[post("/items")]
/// async fn create_item() -> &'static str {
///     "created"
/// }
/// ```
#[proc_macro_attribute]
pub fn post(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(route::route_macro("POST", "post", attr, item.into())).into()
}

/// Annotate an async function as a PUT route handler.
///
/// Generates a companion `__autumn_route_info_{name}()` function that
/// returns a `Route` pairing the path with an Axum
/// handler. In debug builds, `#[axum::debug_handler]` is automatically
/// applied for improved error messages. This has zero cost in release
/// builds.
///
/// # Example
///
/// ```ignore
/// use autumn_web::put;
///
/// #[put("/items/{id}")]
/// async fn update_item() -> &'static str {
///     "updated"
/// }
/// ```
#[proc_macro_attribute]
pub fn put(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(route::route_macro("PUT", "put", attr, item.into())).into()
}

/// Annotate an async function as a PATCH route handler.
///
/// Generates a companion `__autumn_route_info_{name}()` function and a typed
/// `__autumn_path_{name}(…) -> String` path helper.
///
/// # Example
///
/// ```ignore
/// use autumn_web::patch;
///
/// #[patch("/items/{id}")]
/// async fn patch_item() -> &'static str {
///     "patched"
/// }
/// ```
#[proc_macro_attribute]
pub fn patch(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(route::route_macro("PATCH", "patch", attr, item.into())).into()
}

/// Annotate an async function as a DELETE route handler.
///
/// Generates a companion `__autumn_route_info_{name}()` function that
/// returns a `Route` pairing the path with an Axum
/// handler. In debug builds, `#[axum::debug_handler]` is automatically
/// applied for improved error messages. This has zero cost in release
/// builds.
///
/// # Example
///
/// ```ignore
/// use autumn_web::delete;
///
/// #[delete("/items/{id}")]
/// async fn remove_item() -> &'static str {
///     "removed"
/// }
/// ```
#[proc_macro_attribute]
pub fn delete(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(route::route_macro("DELETE", "delete", attr, item.into())).into()
}

/// Annotate an OAuth2/OIDC callback handler.
///
/// This is a convenience alias for `#[get(\"...\")]`, intended for OAuth
/// callback endpoints such as `/auth/github/callback`.
#[proc_macro_attribute]
pub fn oauth2_callback(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(oauth2_callback::oauth2_callback_macro(attr, item.into())).into()
}

/// Collect annotated route handlers into a `Vec<Route>`.
///
/// Each handler must have been annotated with a route macro (`#[get]`,
/// `#[post]`, etc.) which generates a companion
/// `__autumn_route_info_{name}()` function.
///
/// # Example
///
/// ```ignore
/// use autumn_web::{get, post, routes};
///
/// #[get("/hello")]
/// async fn hello() -> &'static str { "hello" }
///
/// #[post("/create")]
/// async fn create() -> &'static str { "created" }
///
/// let all_routes = routes![hello, create];
/// ```
#[proc_macro]
pub fn routes(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(routes_macro::routes_macro(input.into())).into()
}

/// Emit a `pub mod paths { … }` that re-exports each handler's typed path helper.
///
/// Takes the same comma-separated handler list as [`routes!`]. Each entry
/// exposes its `__autumn_path_{name}` companion under the short name:
///
/// ```ignore
/// autumn_web::paths![show_post, create_post, posts::index];
/// // expands to:
/// pub mod paths {
///     pub use super::__autumn_path_show_post as show_post;
///     pub use super::__autumn_path_create_post as create_post;
///     pub use super::posts::__autumn_path_index as index;
/// }
/// ```
///
/// Call this once at the top of the module where your handlers live (or a
/// sibling module) so consumers can write `use crate::routes::paths;` and
/// then `paths::show_post(id)`.
#[proc_macro]
pub fn paths(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(paths_macro::paths_macro(input.into())).into()
}

/// Set up the async runtime for an Autumn application.
///
/// This is a thin wrapper around `#[tokio::main]`. The real
/// framework setup happens in `autumn_web::app().run()`.
///
/// # Example
///
/// ```ignore
/// #[autumn_web::main]
/// async fn main() {
///     autumn_web::app()
///         .routes(routes![hello])
///         .run()
///         .await;
/// }
/// ```
///
/// # Runtime arguments
///
/// All optional; with none of them the runtime is
/// `Builder::new_multi_thread().enable_all()`, tokio's own defaults.
///
/// | Argument | Value | Effect |
/// | --- | --- | --- |
/// | `flavor` | `"multi_thread"` (default) or `"current_thread"` | picks the `Builder` constructor |
/// | `worker_threads` | `usize` expression | `Builder::worker_threads` (multi-thread only) |
/// | `max_blocking_threads` | `usize` expression | `Builder::max_blocking_threads` |
/// | `thread_name` | `Into<String>` expression | `Builder::thread_name` |
/// | `thread_stack_size` | `usize` expression | `Builder::thread_stack_size` |
/// | `thread_keep_alive` | duration string, e.g. `"30s"` | `Builder::thread_keep_alive` |
/// | `configure` | path to `fn(&mut tokio::runtime::Builder)` | runs last, after the arguments above |
///
/// ```ignore
/// fn tune_runtime(builder: &mut autumn_web::reexports::tokio::runtime::Builder) {
///     builder.on_thread_start(|| eprintln!("runtime thread started"));
/// }
///
/// #[autumn_web::main(worker_threads = 4, thread_name = "autumn-worker", configure = tune_runtime)]
/// async fn main() {
///     autumn_web::app().routes(routes![hello]).run().await;
/// }
/// ```
///
/// The numeric arguments take arbitrary expressions, not just literals, so
/// `worker_threads = std::thread::available_parallelism().map_or(4, |n| n.get())`
/// works. `configure` is the escape hatch for `Builder` methods this list does
/// not name.
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(main_macro::main_macro(attr, item.into())).into()
}

/// Annotate an async function as a deterministic simulation test (S-1797, W1).
///
/// Expands into a synchronous `#[test]` function that reads a seed from the
/// `AUTUMN_SIM_SEED` environment variable (hex `0x..` or decimal, default `0`),
/// builds a paused current-thread tokio runtime, constructs a
/// `autumn_web::sim::Sim` from the seed, runs the async body, and prints a
/// copy-pasteable replay line on panic before re-propagating the failure.
///
/// The annotated function must be `async` and take exactly one argument — the
/// [`Sim`](../autumn_web/sim/struct.Sim.html) handle.
///
/// # Example
///
/// ```ignore
/// use autumn_web::sim::Sim;
/// use autumn_web::sim_test;
///
/// #[sim_test]
/// async fn deterministic(mut sim: Sim) {
///     assert_eq!(sim.seed, 0);
/// }
/// ```
#[proc_macro_attribute]
pub fn sim_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(sim_test::sim_test_macro(attr, item.into())).into()
}

/// Annotate an async inbound mail handler function.
///
/// Generates a companion `{name}_handler_info()` function that returns an
/// `InboundMailHandlerInfo` ready to be passed to `InboundMailRouter::handler`.
///
/// # Attributes
///
/// - `to = "address@example.com"` — exact recipient match.
/// - `to = "replies+{token}@app.example"` — plus-address routing; the captured
///   token is available via `InboundEmail::plus_token()`.
/// - `to = "prefix+*"` — local-part prefix match.
/// - `processing = "sync"` | `"background"` (default: `"background"`).
///
/// # Example
///
/// ```rust,ignore
/// #[inbound_mail(to = "support@company.com")]
/// async fn handle_support(email: InboundEmail) -> AutumnResult<()> {
///     tracing::info!(from = %email.from, "support email received");
///     Ok(())
/// }
///
/// // Registration:
/// InboundMailRouter::new()
///     .handler(handle_support_handler_info())
/// ```
#[proc_macro_attribute]
pub fn inbound_mail(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(inbound_mail::inbound_mail_macro(attr, item.into())).into()
}

/// Generate `send_*` and `deliver_later_*` helpers for a mailer impl block.
#[proc_macro_attribute]
pub fn mailer(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(mailer::mailer_macro(attr, item.into())).into()
}

/// Register zero-argument mail preview methods for the dev mail preview UI.
#[proc_macro_attribute]
pub fn mailer_preview(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(mailer_preview::mailer_preview_macro(attr, item.into())).into()
}

/// Collect `#[mailer_preview]` impl blocks into runtime preview registrations.
#[proc_macro]
pub fn mail_previews(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(mail_previews_macro::mail_previews_macro(input.into())).into()
}

/// Define a widget story for the `/_stories` gallery:
/// `story!{ "Group", "Name", { ... } }`.
///
/// The brace-delimited block is **both** executed for the live render and
/// captured byte-for-byte (comments and formatting included) as the displayed
/// source snippet, so the shown code is provably the code that rendered. The
/// block must be a self-contained expression evaluating to `maud::Markup`:
/// it is coerced to a plain `fn() -> Markup`, so capturing anything from the
/// surrounding environment is a compile error.
#[proc_macro]
pub fn story(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(story_macro::story_macro(input.into())).into()
}

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
#[cfg(feature = "db")]
#[proc_macro_attribute]
pub fn model(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(model::model_macro(attr, item.into())).into()
}

/// Derive a field-accurate `OpenApiSchema` impl for a plain struct with named
/// fields, or a unit-variant enum (issue #1972).
///
/// Use it on a handler-arg type — a `Query<T>` param struct, a non-`#[model]`
/// `Json<T>` request body, or an enum appearing in either — so its `OpenAPI`
/// component schema and MCP tool `inputSchema` describe the real contract
/// instead of degrading to a generic `{"type":"object"}` placeholder, without a
/// hand-written impl or an `OpenApiConfig::register_schema` call.
///
/// **Structs**: each field becomes a JSON-schema property (nullable `Option<T>`,
/// `Vec<T>` arrays, inline primitives, `$ref`s for other named types) and every
/// non-`Option` field is `required` — mirroring the schema `#[model]` already
/// generates.
///
/// **Enums**: all-unit-variant enums become the closed string set
/// `{"type":"string","enum":[…]}` that serde puts on the wire, honoring
/// `#[serde(rename)]` / `#[serde(rename_all)]` / `#[serde(skip)]`. A
/// data-carrying variant is a compile error rather than a guess: serde's
/// representation for those depends on `#[serde(tag/content/untagged)]`, so an
/// inferred shape could confidently advertise a contract the handler does not
/// accept. Write the impl by hand and register it for that case.
///
/// # Examples
///
/// ```ignore
/// use autumn_web::openapi::OpenApiSchema;
///
/// #[derive(serde::Deserialize, OpenApiSchema)]
/// struct SearchParams {
///     q: String,
///     limit: Option<i32>,
///     status: Option<Status>,
/// }
///
/// #[derive(serde::Deserialize, OpenApiSchema)]
/// #[serde(rename_all = "snake_case")]
/// enum Status {
///     Open,
///     InProgress,
/// }
/// ```
#[proc_macro_derive(OpenApiSchema)]
pub fn derive_openapi_schema(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(openapi_schema::derive_openapi_schema(input).into()).into()
}

/// Derive a repository with CRUD operations and derived queries.
///
/// Generates a `PgXxxRepository` struct implementing the annotated trait,
/// with auto-generated CRUD methods and query-by-name derived methods.
///
/// # Read replica routing
///
/// When `database.replica_url` is configured, generated read-only methods
/// (`find_by_id`, `find_all`, `count`, `paginate`, `cursor_page`, derived
/// `find_by_*`, search reads) acquire their connection from the replica
/// pool; mutating methods always use the primary. Add `primary_reads` to
/// pin a read-after-write-sensitive repository's reads to the primary, or
/// call the generated `on_primary()` method to pin a single call chain
/// (read-your-writes).
///
/// # Examples
///
/// ```ignore
/// use autumn_web::repository;
///
/// #[repository(Post)]
/// trait PostRepository {
///     fn find_by_published(published: bool) -> Vec<Post>;
/// }
///
/// // Reads pinned to the primary even when a replica is configured.
/// #[repository(LedgerEntry, primary_reads)]
/// trait LedgerEntryRepository {}
///
/// // Cache coherence (#1716): every write below can strand
/// // `views::recent_posts`, so the edge is declared here. The path resolves
/// // to the identity constant `#[cached]` generates beside that function, so
/// // naming anything else does not compile.
/// #[repository(Post, invalidates(crate::views::recent_posts))]
/// trait CoherentPostRepository {
///     // A per-method edge adds to the trait-level ones.
///     #[invalidates(crate::views::by_author)]
///     fn delete_by_author_id(author_id: i64) -> ();
/// }
/// ```
///
/// # Cache coherence (issue #1716)
///
/// Every generated write method publishes which model it mutates, so
/// `autumn cache audit` can fail the build when a write's model appears in a
/// `#[cached]` read's dependency set with no invalidation covering the pair.
/// Discharge the obligation with `invalidates(...)` — on the attribute for
/// every write, or as `#[invalidates(...)]` on one trait method — or opt out
/// with `acknowledge_stale = "reason"`. A repository that declares any edge
/// also gets a generated `invalidate_declared_caches()` for its write paths to
/// call. See `docs/guide/cache-coherence.md`.
#[cfg(feature = "db")]
#[proc_macro_attribute]
pub fn repository(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(repository::repository_macro(attr, item.into())).into()
}

/// Declare a scheduled background task.
///
/// # Examples
///
/// ```ignore
/// #[scheduled(every = "5m", name = "cleanup")]
/// async fn cleanup(state: AppState) -> AutumnResult<()> { Ok(()) }
///
/// #[scheduled(cron = "0 0 0 * * *", name = "nightly")]
/// async fn nightly(state: AppState) -> AutumnResult<()> { Ok(()) }
/// ```
#[proc_macro_attribute]
pub fn scheduled(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(scheduled::scheduled_macro(attr, item.into())).into()
}

/// Declare an on-demand background job.
///
/// Route latency-sensitive work to a named queue with `queue = "..."`. Workers
/// drain queues in the priority order configured under `[jobs] queues` in
/// `autumn.toml`, so a flood of low-priority jobs can't delay a critical one.
/// Jobs with no `queue` land on the `"default"` queue.
///
/// ```ignore
/// #[job(queue = "critical", max_attempts = 5)]
/// async fn send_password_reset(state: AppState, args: ResetArgs) -> AutumnResult<()> {
///     Ok(())
/// }
///
/// // autumn.toml — strict priority (or weighted: { critical = 4, default = 1 }):
/// // [jobs]
/// // queues = ["critical", "default", "low"]
/// SendPasswordResetJob::enqueue(ResetArgs { user_id: 1 }).await?;
/// ```
///
/// Accept an optional third `JobContext` argument to report progress and
/// record a terminal result/error for jobs enqueued with `enqueue_tracked`
/// (the companion struct gains `enqueue_tracked` / `enqueue_tracked_for`
/// alongside `enqueue`):
///
/// ```ignore
/// #[job(name = "export_orders")]
/// async fn export_orders(state: AppState, args: ExportArgs, ctx: JobContext) -> AutumnResult<()> {
///     ctx.set_progress(50, Some("Rows 1200/5000")).await?;
///     ctx.set_result(serde_json::json!({ "download_url": "/blob/abc.csv" }));
///     Ok(())
/// }
///
/// let handle = ExportOrdersJob::enqueue_tracked(ExportArgs { account_id: 1 }).await?;
/// println!("poll at {}", handle.status_path());
/// ```
#[proc_macro_attribute]
pub fn job(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(job::job_macro(attr, item.into())).into()
}

/// Declare a typed domain event.
///
/// Applies the serde + `Clone`/`Debug` derives the event bus needs and
/// implements `autumn_web::events::Event` with a stable `NAME` (the struct
/// name by default, or `#[event(name = "...")]`).
///
/// ```ignore
/// #[event]
/// struct UserSignedUp { user_id: i64 }
/// ```
#[proc_macro_attribute]
pub fn event(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(event::event_macro(attr, item.into())).into()
}

/// Declare an event listener that reacts to a typed `#[event]`.
///
/// Runs **synchronously** (in-request) by default, or **durably** (enqueued on
/// the `#[job]` queue, surviving restarts with retry + DLQ) with `durable`.
///
/// ```ignore
/// #[listener(UserSignedUp, durable, max_attempts = 5)]
/// async fn send_welcome_email(state: AppState, event: UserSignedUp) -> AutumnResult<()> { Ok(()) }
/// ```
#[proc_macro_attribute]
pub fn listener(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(listener::listener_macro(attr, item.into())).into()
}

/// Collect `#[listener]` handlers into a `Vec<ListenerInfo>`.
#[proc_macro]
pub fn listeners(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(listeners_macro::listeners_macro(input.into())).into()
}

/// Declare a one-off operational task runnable with `autumn task <name>`.
#[proc_macro_attribute]
pub fn task(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(one_off_task::task_macro(attr, item.into())).into()
}

/// Annotate an async function as a statically pre-rendered GET route.
///
/// Like `#[get]`, this generates a route companion function. Additionally,
/// it generates a `__autumn_static_meta_{name}()` companion that registers
/// the route for static HTML generation at build time.
///
/// Phase 1: path parameters are **not** supported. Use `#[get]` for
/// parameterized routes.
///
/// # Example
///
/// ```ignore
/// use autumn_web::static_get;
///
/// #[static_get("/about")]
/// async fn about() -> &'static str {
///     "About us"
/// }
/// ```
///
/// # Route-level SEO defaults
///
/// Accepts the same `seo(...)` argument as [`macro@get`], so pre-rendered
/// pages carry the declared meta tags. Static generation drives the same
/// router as the live server, so the values reach the handler identically in
/// both modes:
///
/// ```ignore
/// #[static_get("/about", seo(title = "About • My Blog", og_type = "website"))]
/// async fn about(seo: SeoMeta) -> Markup {
///     html! { head { (seo.render()) } }
/// }
/// ```
#[proc_macro_attribute]
pub fn static_get(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(static_route::static_get_macro(attr, item.into())).into()
}

/// Collect `#[scheduled]` task handlers into a `Vec<TaskInfo>`.
///
/// ```ignore
/// let all_tasks = tasks![cleanup, nightly];
/// ```
#[proc_macro]
pub fn tasks(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(tasks_macro::tasks_macro(input.into())).into()
}

/// Collect `#[job]` handlers into a `Vec<JobInfo>`.
#[proc_macro]
pub fn jobs(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(jobs_macro::jobs_macro(input.into())).into()
}

/// Collect `#[task]` handlers into a `Vec<OneOffTaskInfo>`.
#[proc_macro]
pub fn one_off_tasks(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(one_off_tasks_macro::one_off_tasks_macro(input.into())).into()
}

/// Secure a route handler with authentication and optional role checks.
///
/// Applied before a route macro (`#[get]`, `#[post]`, etc.), this macro
/// injects an authentication guard at the top of the handler. The guard
/// checks the session for the configured auth key (default: `"user_id"`)
/// and, when roles are specified, verifies the user's role matches.
///
/// Returns `401 Unauthorized` if not authenticated, or `403 Forbidden`
/// if the user lacks the required role.
///
/// # Forms
///
/// - `#[secured]` -- require authentication only
/// - `#[secured("admin")]` -- require a specific role
/// - `#[secured("admin", "editor")]` -- require any of the listed roles
///
/// # Example
///
/// ```ignore
/// use autumn_web::prelude::*;
///
/// #[get("/admin")]
/// #[secured("admin")]
/// async fn admin_panel() -> AutumnResult<&'static str> {
///     Ok("welcome, admin")
/// }
/// ```
#[proc_macro_attribute]
pub fn secured(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(secured::secured_macro(attr, item.into())).into()
}

/// Declare a route handler as deliberately public (unauthenticated).
///
/// `#[public]` injects no runtime guard — it is a compile-time *marker* that
/// records intent. The route macros surface it as `ApiDoc::public`, which the
/// build-time security classifier (`autumn routes audit`) uses to distinguish
/// a route that is *meant* to be open from one whose auth posture was simply
/// never declared. Applying it makes an otherwise-unclassified route pass the
/// audit gate, exactly like adding a [`#[secured]`](macro@secured) guard does.
///
/// # Example
///
/// ```ignore
/// use autumn_web::prelude::*;
///
/// #[get("/health")]
/// #[public]
/// async fn health() -> &'static str { "ok" }
/// ```
#[proc_macro_attribute]
pub fn public(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(public::public_macro(attr, item.into())).into()
}

/// Declare a read-path route as eligible to run in the edge capsule (#1790).
///
/// `#[edge]` injects no runtime guard and does not rewrite the handler
/// signature — it is a compile-time *marker*, like
/// [`#[public]`](macro@public). The route macro reads it back and emits an
/// extra `__autumn_edge_route_{name}()` companion returning an
/// `autumn_edge::EdgeRoute`, while gating the native (`autumn_web`) companions
/// behind `#[cfg(not(target_arch = "wasm32"))]` so the same handler source
/// compiles for both the origin binary and the `wasm32-wasip1` capsule.
/// Marking a handler makes it *eligible*; listing it in
/// [`edge_routes!`](macro@edge_routes) is what puts it in the capsule.
///
/// # Forms
///
/// - `#[edge]` — the handler needs no platform seam
/// - `#[edge(needs(kv))]` — the handler reads the edge key-value cache
///   (`autumn_edge::EdgeCache`), which the host must provide; a request
///   arriving at an edge without that capability falls through to the origin
///
/// # Restrictions
///
/// The edge lane is read-path only and carries no session, auth, or database
/// state, so these are compile errors:
///
/// - a method other than `#[get]`;
/// - combining with `#[secured]`, `#[authorize]`, `#[step_up]`, or
///   `#[throttle]` (in either attribute order);
/// - `#[static_get]` (already pre-rendered), `#[ws]`, or `#[oauth2_callback]`.
///
/// # Example
///
/// ```ignore
/// use autumn_edge::prelude::{EdgeCache, Path};
/// use autumn_web::{edge, get};
///
/// #[get("/greet/{name}")]
/// #[edge]
/// async fn greet(Path(name): Path<String>) -> String {
///     format!("Hello, {name}!")
/// }
///
/// #[get("/note/{id}")]
/// #[edge(needs(kv))]
/// async fn note(Path(id): Path<String>, cache: EdgeCache) -> String {
///     cache.get_string(&id).unwrap_or_else(|| "not cached".to_owned())
/// }
/// ```
#[proc_macro_attribute]
pub fn edge(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(edge::edge_macro(attr, item.into())).into()
}

/// Collect `#[edge]` handlers into a `Vec<EdgeRoute>` (#1790).
///
/// The edge-lane counterpart of [`routes!`](macro@routes): each entry resolves
/// to the handler's `__autumn_edge_route_{name}()` companion, so a handler that
/// was never marked `#[edge]` fails to resolve rather than silently vanishing
/// from the capsule.
///
/// ```ignore
/// use autumn_web::{edge, edge_routes, get};
///
/// #[get("/greet")]
/// #[edge]
/// async fn greet() -> &'static str { "hi" }
///
/// pub fn edge_routes() -> Vec<autumn_edge::EdgeRoute> {
///     edge_routes![greet]
/// }
/// ```
#[proc_macro]
pub fn edge_routes(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(edge_routes_macro::edge_routes_macro(input.into())).into()
}

/// Require fresh ("step-up") authentication before a route handler runs.
///
/// The handler is guarded by a freshness check on the session's
/// `last_strong_auth_at` claim. When the claim is missing or older than
/// `max_age` the request is handled as follows:
///
/// - **Browser clients** (no `application/json` in `Accept`): redirect to
///   `/reauth?return_to=<current-path>`.
/// - **API / JSON clients** (`Accept: application/json`): `401 Unauthorized`
///   with an RFC 7807 problem-details body (`type` =
///   `"https://autumn.rs/probs/step-up-required"`) and a
///   `WWW-Authenticate: StepUp max-age=N` hint header.
///
/// # Forms
///
/// - `#[step_up]` — default max-age (5 minutes, or the global `[auth.step_up]`
///   config override)
/// - `#[step_up(max_age = "5m")]` — custom per-route max-age
///
/// # Example
///
/// ```ignore
/// use autumn_web::prelude::*;
///
/// // Requires re-authentication within the last 5 minutes.
/// #[delete("/account")]
/// #[step_up]
/// async fn destroy_account() -> AutumnResult<Redirect> {
///     // ... delete account ...
///     Ok(Redirect::to("/bye"))
/// }
///
/// // Custom max-age.
/// #[post("/auth/mfa/remove")]
/// #[step_up(max_age = "2m")]
/// async fn remove_mfa() -> AutumnResult<&'static str> {
///     Ok("MFA removed")
/// }
/// ```
#[proc_macro_attribute]
pub fn step_up(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(step_up::step_up_macro(attr, item.into())).into()
}

/// Apply a per-route rate limit to a handler.
///
/// The handler is guarded by an additional rate limiter that composes with
/// (and, for the annotated route, is stricter than) the global limiter
/// configured under `[security.rate_limit]`. Requests denied by either
/// limiter respond with `429 Too Many Requests` including a `Retry-After`
/// header and the standard `x-ratelimit-*` headers.
///
/// # Forms
///
/// - `#[throttle(limit = 5, per = "1m")]` — inline limit; keying strategy
///   matches the global limiter.
/// - `#[throttle(limit = 5, per = "1m", key = "ip" | "principal" | "token")]`
///   — inline limit with an explicit key strategy override.
/// - `#[throttle("login")]` — reference a named limiter defined in
///   `[security.rate_limit.named.login]`.
///
/// # Example
///
/// ```ignore
/// use autumn_web::prelude::*;
///
/// #[post("/login")]
/// #[throttle(limit = 5, per = "1m", key = "ip")]
/// async fn login() -> AutumnResult<&'static str> {
///     Ok("welcome back")
/// }
/// ```
///
/// # Limitations
///
/// Like the sibling `#[secured]` / `#[step_up]` guards it mirrors, the throttle
/// check runs inside the handler after `FromRequestParts` extractors, but body
/// extractors (`Json` / `Form` / `Multipart`) are parsed by Axum *before* the
/// throttle check, so an over-limit client can still incur request-body parsing
/// before receiving its `429`. For hard pre-body protection, combine with the
/// global limiter layer under `[security.rate_limit]`.
///
/// # Attribute ordering
///
/// `#[throttle]` may be written above or below the route method attribute
/// (`#[get]` / `#[post]` / …) — both orders enforce throttling identically
/// (including idempotency-replay accounting) and both produce the same
/// `OpenAPI` response schema. When `#[throttle]` expands first it rewrites
/// the return type to `Response` (like the sibling `#[secured]` / `#[step_up]`
/// / `#[authorize]` guards), but the route macro recovers the original
/// `Json<T>` return type from the guard's generated body, so response-schema
/// inference does not depend on expansion order (#1677).
#[proc_macro_attribute]
pub fn throttle(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(throttle::throttle_macro(attr, item.into())).into()
}

/// Bound the number of database queries a handler can issue — at compile time.
///
/// `#[query_budget(N)]` fails the build when any statically reachable path
/// through the handler can issue more than `N` queries. The canonical case is
/// the N+1: a repository or `Db` call inside a loop over a runtime-sized
/// collection. Because the gate runs during `cargo build`, it fires on every
/// branch — including the ones no test exercises.
///
/// # Example
///
/// ```ignore
/// use autumn_web::{get, query_budget};
///
/// #[get("/posts")]
/// #[query_budget(2)]
/// async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
///     let posts = repo.find_all().await?;                              // 1
///     let posts = repo.preload(posts, Post::preload().author()).await?; // 2
///     Ok(render(&posts))
/// }
/// ```
///
/// Written with a per-row `repo.find_author(...)` inside a `for` loop instead,
/// the same handler fails to compile with a diagnostic pointing at the loop.
///
/// # How the count is computed
///
/// * Straight-line statements **sum**; `if` / `match` arms take the
///   **maximum** (the worst reachable path).
/// * A loop whose body issues a query is **unbounded**, unless the iterable
///   has a literal compile-time bound (`for _ in 0..3`), in which case the
///   body cost is multiplied.
/// * A method chain rooted at a `Db` / repository handle is **one** query,
///   however many builder methods (`on_primary()`, `scoped()`, …) it carries.
/// * `.preload(rows, Post::preload().author().tags())` costs **one query per
///   association** — the batched `WHERE ... IN (...)` loads.
/// * Anything opaque — a helper function handed the handle, a macro body that
///   names it, a closure that may run per element — is **reported**, never
///   assumed query-free.
///
/// # Escape hatches
///
/// * `#[query_budget(unbounded, reason = "…")]` — opt the whole handler out.
/// * `#[query_cost(N)]` on a statement — declare an opaque call's cost.
/// * `#[query_exempt(reason = "…")]` on a statement — drop it from the ledger.
///
/// Both statement annotations are consumed by this macro and never reach
/// rustc. See `docs/guide/query-budgets.md` for the full guide.
#[proc_macro_attribute]
pub fn query_budget(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(query_budget::query_budget_macro(attr, item.into())).into()
}

/// Declare a handler agent-operable, under a named authority grant.
///
/// `#[agent_operable(grant = RefundDrafter)]` walks the handler body, derives
/// the effects it can prove — row writes, unbounded writes, cross-tenant
/// access, outbound HTTP, webhook fan-out, background jobs — and fails the
/// build when the named `authority_grant!` does not allow one of them. The
/// check is a `const` assertion respanned onto the offending call site, so it
/// works across crates and fires during `cargo build` on every branch,
/// including the ones no test exercises.
///
/// # Example
///
/// ```ignore
/// use autumn_web::prelude::*;
///
/// autumn_web::authority_grant! {
///     /// Draft-only refund authority for the support agent.
///     pub RefundDrafter {
///         writes: [Refund],
///         outbound: ["https://api.stripe.com/v1/refunds"],
///         jobs: [NotifyFinanceJob],
///         reversibility: compensable,
///     }
/// }
///
/// #[post("/refunds")]
/// #[api_doc(mcp, summary = "Draft a refund")]
/// #[agent_operable(grant = RefundDrafter)]
/// async fn draft_refund(
///     repo: PgRefundRepository,
///     payouts: PgPayoutRepository,
///     client: Client,
///     Json(body): Json<NewRefund>,
/// ) -> AutumnResult<Json<Refund>> {
///     let refund = repo.create(&body).await?;               // allowed: `writes: [Refund]`
///     client.post("https://api.stripe.com/v1/refunds")      // allowed: `outbound: [...]`
///         .json(&refund)
///         .send()
///         .await?;
///     NotifyFinanceJob::enqueue(NotifyFinanceArgs { refund_id: refund.id }).await?;
///     Ok(Json(refund))
/// }
/// ```
///
/// Adding `payouts.delete_all().await?` to that body fails the build: `payouts`
/// is a tracked repository handle, and the grant allows no unbounded write to
/// `Payout`.
///
/// # What is proved, and what is only declared
///
/// * A write, unbounded write or cross-tenant access reached through a handle
///   named in the signature is **proved**.
/// * An outbound call with a literal absolute URL is **proved**; one whose host
///   is resolved from config through a named client is **declared**.
/// * A webhook dispatch delivers to subscriber-supplied URLs, so it is granted
///   by topic (`webhooks: ["refund.drafted"]`), never by URL prefix.
/// * `rate` / `spend` caps are **declared**: this slice records them, it does
///   not enforce them.
/// * Anything opaque — a helper handed the handle, a `format!`-built URL, a
///   `tokio::spawn` that detaches the effect from the request it is audited
///   under — is reported, never assumed effect-free. A helper is opaque
///   whether it is free (`wipe(repo)`) or associated (`Billing::wipe(repo)`):
///   an associated function is another function's business too, and a static
///   finder that really only reads (`Post::find_published(&mut db)`) is
///   discharged with `#[agent_effect(none, reason = "…")]` rather than
///   assumed. A local alias for an effect verb (`let schedule =
///   NotifyFinanceJob::enqueue;`) is classified against the path it names.
///
/// # Escape hatch
///
/// `#[agent_effect(...)]` on a statement declares what the analysis cannot
/// read: `#[agent_effect(writes(Refund), reason = "…")]`, or
/// `#[agent_effect(none, reason = "…")]` for a statement verified
/// effect-free. Declared effects are still checked against the grant — the
/// hatch declares, it never grants — and a `reason` is mandatory. The
/// annotation is consumed by this macro and never reaches rustc.
///
/// See `docs/guide/agent-authority.md` for the full guide.
#[proc_macro_attribute]
pub fn agent_operable(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(agent_authority::agent_operable_macro(attr, item.into())).into()
}

/// Gate a route handler on a named feature flag.
///
/// If the flag is disabled for the current actor, the handler responds with
/// `404 Not Found` by default. Provide a `fallback` function to return a
/// custom response instead.
///
/// The flag key is resolved against the `FeatureFlagService` stored in the
/// `AppState` extensions. Unknown flags are treated as **disabled**
/// (fail-closed).
///
/// # Forms
///
/// - `#[feature_flag("key")]` — return 404 when disabled
/// - `#[feature_flag("key", fallback = my_fn)]` — call `my_fn()` when disabled
///
/// # Example
///
/// ```ignore
/// use autumn_web::prelude::*;
///
/// #[get("/beta")]
/// #[feature_flag("beta_dashboard")]
/// async fn beta_dashboard() -> Markup {
///     html! { h1 { "Beta Dashboard" } }
/// }
/// ```
///
#[proc_macro_attribute]
pub fn feature_flag(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(feature_flag::feature_flag_macro(attr, item.into())).into()
}

/// Enforce a record-level authorization policy on a route handler.
///
/// Resolves the `Policy`
/// registered for the named resource type and calls the matching
/// action method. Short-circuits with the configured deny response
/// (default `404`, optionally `403`) before the handler body runs.
///
/// Coexists with `#[secured]`: `#[secured]` answers "are you in?",
/// `#[authorize]` answers "are you allowed to act on *this record*?"
///
/// # Forms
///
/// ```ignore
/// // Resource arg is auto-detected by snake-cased type name (Post -> `post`).
/// #[authorize("update", resource = Post)]
/// async fn update_post(post: Post) -> AutumnResult<...> { ... }
///
/// // Explicit binding name (overrides the snake-case default).
/// #[authorize("delete", resource = Post, from = target)]
/// async fn destroy(target: Post) -> AutumnResult<...> { ... }
/// ```
#[proc_macro_attribute]
pub fn authorize(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(authorize::authorize_macro(attr, item.into())).into()
}

/// Collect `#[static_get]` handlers into a `Vec<StaticRouteMeta>`.
///
/// ```ignore
/// use autumn_web::prelude::*;
///
/// #[static_get("/about")]
/// async fn about() -> &'static str { "About" }
///
/// let metas = static_routes![about];
/// ```
#[proc_macro]
pub fn static_routes(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(static_routes_macro::static_routes_macro(input.into())).into()
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
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(service::service_macro(attr, item.into())).into()
}

/// Cache the return value of a function based on its arguments.
///
/// Wraps a function with an in-memory cache backed by a per-function
/// static `Cache` (from `autumn_web::cache::Cache`). Key arguments
/// must implement `Hash + Eq + Clone`; the return type must be `Clone`.
///
/// # Attributes
///
/// | Attribute | Example | Description |
/// |-----------|---------|-------------|
/// | `ttl` | `"5m"` | Time-to-live per entry (uses `parse_duration` syntax) |
/// | `max` | `1000` | Max entries; oldest evicted on overflow |
/// | `result` | (flag) | Only cache `Ok` values; pass `Err` through uncached |
/// | `key` | `key(tenant_id)` | Build the key from *these* parameters only |
/// | `reads` | `reads(Post, Comment)` | Declared cache-coherence dependency set |
/// | `acknowledge_stale` | `"5s TTL is tight enough"` | Opt out of the coherence gate |
///
/// # Cache coherence (issue #1716)
///
/// Every annotated function publishes which models its value is derived from,
/// and `autumn cache audit` fails the build when a `#[repository]` write can
/// leave that value stale with no invalidation covering the pair. `reads(...)`
/// declares the dependency set; without it the macro derives what it can from
/// the signature and body, and a function nothing could be recovered from is
/// recorded as `undetermined` — reported, never gated. See
/// `docs/guide/cache-coherence.md`.
///
/// # Examples
///
/// ```ignore
/// use autumn_web::cached;
///
/// // Cache with 5-minute TTL, max 100 entries, only cache Ok values
/// #[cached(ttl = "5m", max = 100, result)]
/// async fn get_user(id: i64) -> AutumnResult<User> {
///     db.find(id).await
/// }
///
/// // A repository-backed read: the handle is not part of the value's
/// // identity, so `key(...)` keeps it out of the cache key, and `reads(...)`
/// // tells the coherence gate what a write to `Project` would strand.
/// #[cached(ttl = "30s", key(tenant_id), reads(Project), result)]
/// async fn project_count(tenant_id: String, repo: &PgProjectRepository)
///     -> AutumnResult<i64>
/// {
///     repo.count().await
/// }
///
/// // Cache forever with no size limit
/// #[cached]
/// async fn get_config() -> Vec<String> {
///     load_config_from_disk()
/// }
/// ```
#[proc_macro_attribute]
pub fn cached(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(cached::cached_macro(attr, item.into())).into()
}

/// Enrich a route handler's auto-generated `OpenAPI` documentation.
///
/// Applied on top of a route macro (`#[get]`, `#[post]`, etc.), this
/// attribute lets you override or add documentation fields that cannot
/// be inferred from the handler signature (summaries, descriptions,
/// tags, custom success status codes).
///
/// The route macro consumes this attribute and folds the metadata into
/// the route's `ApiDoc`. When no route macro is applied, the attribute
/// is a no-op.
///
/// # Supported keys
///
/// | Key | Type | Effect |
/// |-----|------|--------|
/// | `summary` | string | Short one-line description |
/// | `description` | string | Longer multi-line description |
/// | `tag` | string | Single `OpenAPI` tag for grouping |
/// | `tags` | `[string, ...]` | Multiple `OpenAPI` tags |
/// | `operation_id` | string | Override the default operation id |
/// | `status` | integer | Success HTTP status code (defaults to `200`) |
/// | `hidden` | flag / bool | Exclude the route from the generated spec |
/// | `mcp` | flag / bool | Expose this endpoint as an MCP tool (`mcp = false` force-excludes it). Requires the `mcp` feature and a `mount_mcp` call. |
///
/// # Examples
///
/// ```ignore
/// use autumn_web::prelude::*;
///
/// #[get("/users/{id}")]
/// #[api_doc(summary = "Fetch a user by id", tag = "users")]
/// async fn get_user(Path(id): Path<i32>) -> String {
///     format!("User {id}")
/// }
///
/// #[post("/users")]
/// #[api_doc(description = "Create a new user", status = 201)]
/// async fn create_user(Json(req): Json<serde_json::Value>) -> Json<serde_json::Value> {
///     Json(req)
/// }
///
/// #[get("/internal/metrics")]
/// #[api_doc(hidden)]
/// async fn metrics() -> &'static str { "" }
/// ```
#[proc_macro_attribute]
pub fn api_doc(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Rust expands attribute macros top-down (outermost first), so if the
    // user writes
    //
    //   #[api_doc(summary = "...")]
    //   #[get("/x")]
    //   async fn handler() { ... }
    //
    // this macro fires BEFORE `#[get]` and would strip `#[api_doc]` from
    // the item — the route macro would then never see the overrides.
    //
    // To support both orderings, we detect any pending route attribute
    // (`get`, `post`, etc.) sitting below us and reorder: we remove the
    // route attribute and emit it as the NEW outermost attribute, and
    // we re-attach `#[api_doc(...)]` to the function body. Rust then
    // expands the route macro next, which finds and consumes the
    // preserved `#[api_doc]` via the usual attribute-list walk.
    //
    // `#[api_doc(...)]` never itself emits an `::autumn_web` path — it only
    // reorders attributes for the paired route macro to expand next, and
    // that route macro's own `crate = "..."` (see `get`, `post`, etc.) is
    // what actually governs the combined expansion. Still strip a `crate =
    // "..."` given here (rather than leaving it for the reordering below to
    // re-embed and only later fail with a confusing raw parse error out of
    // `api_doc::extract`), so every attribute macro accepts and validates
    // the argument uniformly even though this one has nothing to apply it
    // to.
    let (_crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let out: proc_macro2::TokenStream = api_doc_standalone(attr.into(), item).into();
    let _guard = crate_path::set_target(None);
    crate_path::finalize(out).into()
}

const ROUTE_ATTR_NAMES: &[&str] = &["get", "post", "put", "delete", "patch", "static_get", "ws"];

/// Return `true` when an attribute names one of the Autumn route macros.
///
/// We match on the **last** path segment so qualified forms like
/// `#[autumn_web::get("/x")]`, `#[autumn_macros::post("/x")]`, or
/// even `#[crate::get("/x")]` are recognized alongside the bare
/// `#[get("/x")]`. Unqualified identifiers are covered by the same
/// logic because their path has a single segment.
fn is_route_attribute(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .is_some_and(|name| ROUTE_ATTR_NAMES.contains(&name.as_str()))
}

fn api_doc_standalone(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr_ts: proc_macro2::TokenStream = attr.into();
    let mut input_fn: syn::ItemFn = match syn::parse(item.clone()) {
        Ok(f) => f,
        // Not a function (e.g. applied to a struct) — leave it alone so
        // the user sees the usual "expected function" error from rustc.
        Err(_) => return item,
    };

    let route_idx = input_fn.attrs.iter().position(is_route_attribute);

    let Some(idx) = route_idx else {
        // Standalone `#[api_doc]` with no paired route macro is a no-op;
        // route metadata is only emitted through route macros.
        return quote::quote! { #input_fn }.into();
    };

    let route_attr = input_fn.attrs.remove(idx);
    let preserved: syn::Attribute = syn::parse_quote! {
        #[api_doc(#attr_ts)]
    };
    input_fn.attrs.insert(0, preserved);

    quote::quote! {
        #route_attr
        #input_fn
    }
    .into()
}

/// Annotate an async function as a WebSocket route handler.
///
/// The function follows the **two-function pattern**: it runs at HTTP
/// upgrade time (with access to Axum extractors) and returns a closure
/// implementing `WsHandler` (from `autumn_web::ws::WsHandler`) that handles the live WebSocket connection.
///
/// The macro generates a GET route that performs the WebSocket upgrade,
/// so it integrates seamlessly with `routes![]`.
///
/// # Examples
///
/// ```ignore
/// use autumn_web::prelude::*;
/// use autumn_web::ws::{WebSocket, Message, WsHandler};
///
/// // Minimal echo handler
/// #[ws("/echo")]
/// async fn echo() -> impl WsHandler {
///     |mut socket: WebSocket| async move {
///         while let Some(Ok(msg)) = socket.recv().await {
///             if let Message::Text(text) = msg {
///                 socket.send(Message::Text(text)).await.ok();
///             }
///         }
///     }
/// }
///
/// // With extractors (runs before upgrade)
/// #[ws("/chat")]
/// async fn chat(state: AppState) -> impl WsHandler {
///     let channels = state.channels().clone();
///     |mut socket: WebSocket| async move {
///         // use channels + socket
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn ws(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(ws::ws_macro(attr, item.into())).into()
}

/// Translate an i18n key, with **compile-time validation** that the key
/// exists in the default locale's `.ftl` file.
///
/// Re-exported as `autumn_web::t!` (and `autumn_web::prelude::t!`) when the
/// `i18n` feature is enabled on `autumn-web`.
///
/// # Forms
///
/// ```ignore
/// // Without args:
/// t!(locale, "welcome.title")
/// // With named args (Project Fluent's `{ $name }` placeable syntax):
/// t!(locale, "welcome.greeting", name = "Ada")
/// ```
///
/// # Compile-time behaviour
///
/// At expansion time the macro reads `$CARGO_MANIFEST_DIR/i18n/<default>.ftl`
/// (where `<default>` is the value of the `AUTUMN_I18N_DEFAULT_LOCALE`
/// environment variable, defaulting to `"en"`). If the key is not present,
/// the macro emits a `compile_error!` pointing at the literal so the build
/// fails with a clear diagnostic — including a "did you mean" suggestion
/// for typos within Levenshtein distance 3.
///
/// If the file does not exist (e.g. an app that just enabled the feature
/// flag and has not yet authored translations), the macro degrades to a
/// pure runtime call so the build still succeeds. The runtime path will
/// produce the visible `{$key}` marker on miss.
#[proc_macro]
pub fn t(input: TokenStream) -> TokenStream {
    let _guard = crate_path::set_target(None);
    crate_path::finalize(i18n::t_macro(input.into())).into()
}

/// Turn a plain state enum into a statically-verified lifecycle.
///
/// Given a declared `initial` state, one or more `terminal` states, and a set
/// of `transitions`, `#[lifecycle]` preserves the original enum and appends:
///
/// 1. Metadata consts (`LIFECYCLE_INITIAL`, `LIFECYCLE_TERMINALS`,
///    `LIFECYCLE_STATES`, `LIFECYCLE_TRANSITIONS`) and a `can_transition_to`
///    runtime check on the enum. Because these reference the enum's own
///    variants, a declared state that is not a real variant is a compile error.
/// 2. A typestate transition module named after the enum in `snake_case`, whose
///    `Machine<S>` exposes a consuming `to_<target>` method *only* for declared
///    edges — firing an undeclared transition does not compile.
///
/// # Example
///
/// ```ignore
/// use autumn_web::lifecycle;
///
/// #[lifecycle(
///     initial = Draft,
///     terminal(Archived),
///     transitions(
///         Draft -> Published,
///         Published -> Archived,
///         Published -> Draft,
///     )
/// )]
/// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// pub enum ArticleState { Draft, Published, Archived }
///
/// let m = article_state::Machine::<article_state::Draft>::start();
/// let m = m.to_published();      // only declared edges exist as methods
/// assert_eq!(m.current(), ArticleState::Published);
/// assert!(ArticleState::Draft.can_transition_to(&ArticleState::Published));
/// ```
#[proc_macro_attribute]
pub fn lifecycle(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (crate_override, attr) = match crate_path::extract_crate_override(attr.into()) {
        Ok(pair) => pair,
        Err(err) => return err.into(),
    };
    let _guard = crate_path::set_target(crate_override.as_deref());
    crate_path::finalize(lifecycle::lifecycle_macro(attr, item.into())).into()
}
