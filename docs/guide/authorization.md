# Record-Level Authorization

`#[secured]` answers **"are you in?"** — is this request authenticated, and is
the user in one of the listed roles? It does not answer **"are you allowed
to act on *this specific record*?"** That is the question every multi-user
CRUD app has to answer at every write endpoint, and it is what Autumn's
`Policy` trait, the `#[authorize]` macro, and the `policy =` argument on
`#[repository]` exist for.

This guide covers:

- The [`Policy`](#the-policy-trait) trait and `PolicyContext`.
- [Scope queries](#scope-queries) for filtering list endpoints.
- The [`#[authorize]`](#the-authorize-attribute-macro) attribute macro.
- The [`#[repository(policy = ...)]`](#the-repository-policy-argument)
  argument that wires policies into auto-generated CRUD endpoints.
- The [403-vs-404 decision](#403-vs-404).
- [Common patterns](#common-patterns) (ownership, group membership,
  role-augmented checks).
- A [side-by-side migration](#migration-the-reddit-clone-update-handler)
  of the reddit-clone example.

## The `Policy` trait

Every multi-user resource gets one `Policy` impl. Default impls deny —
opting into a policy is safe by default, and a freshly-introduced policy
forbids every action until the developer explicitly allows one.

```rust,ignore
use autumn_web::authorization::{BoxFuture, Policy, PolicyContext};

#[derive(Default)]
pub struct PostPolicy;

impl Policy<Post> for PostPolicy {
    fn can_show<'a>(&'a self, _ctx: &'a PolicyContext, _post: &'a Post)
        -> BoxFuture<'a, bool>
    {
        Box::pin(async { true }) // posts are public
    }

    fn can_update<'a>(&'a self, ctx: &'a PolicyContext, post: &'a Post)
        -> BoxFuture<'a, bool>
    {
        Box::pin(async move {
            ctx.has_role("admin")
                || ctx.user_id_i64() == Some(post.author_id)
        })
    }

    fn can_delete<'a>(&'a self, ctx: &'a PolicyContext, post: &'a Post)
        -> BoxFuture<'a, bool>
    {
        Box::pin(async move {
            ctx.has_role("admin")
                || ctx.user_id_i64() == Some(post.author_id)
        })
    }
}
```

Register the policy on the app builder:

```rust,ignore
autumn_web::app()
    .routes(routes![...])
    .policy::<Post, _>(PostPolicy)
    .run()
    .await;
```

`PolicyContext` carries the resolved [`Session`](../api/session.md), the
authenticated user id (when any), the active role set, and a clone of the
database pool so policies can consult related rows. The trait is
object-safe — apps can hold `Arc<dyn Policy<Post>>` and swap
implementations in tests.

## Scope queries

`Policy` answers "which actions are allowed on this *one* record?"
`Scope` answers "which records is this user allowed to *see* in the list?"
The default impl returns an empty list so a missing scope opt-in fails
closed.

```rust,ignore
use autumn_web::authorization::{BoxFuture, PolicyContext, Scope};
use autumn_web::reexports::diesel_async::{AsyncPgConnection, RunQueryDsl};
use diesel::prelude::*;

#[derive(Default)]
pub struct PostScope;

impl Scope<Post> for PostScope {
    fn list<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        conn: &'a mut AsyncPgConnection,
    ) -> BoxFuture<'a, autumn_web::AutumnResult<Vec<Post>>> {
        Box::pin(async move {
            if ctx.has_role("admin") {
                posts::table.load(conn).await.map_err(Into::into)
            } else if let Some(uid) = ctx.user_id_i64() {
                posts::table
                    .filter(posts::author_id.eq(uid))
                    .load(conn)
                    .await
                    .map_err(Into::into)
            } else {
                Ok(Vec::new()) // anon -> empty
            }
        })
    }
}
```

Register alongside the policy: `.scope::<Post, _>(PostScope)`. When a
`#[repository(api = "/posts", scope = PostScope)]` repository is mounted,
its `GET /posts` endpoint invokes the registered scope automatically.

### `Post::scope(&ctx).load(&mut db).await?` in hand-written handlers

The `Scoped` blanket trait adds a `T::scope(&ctx)` method to every
type, mirroring Pundit's `policy_scope(Post)` and Phoenix
Bodyguard's `Bodyguard.scope/4`:

```rust,ignore
use autumn_web::prelude::*;
use autumn_web::authorization::{PolicyContext, Scoped};

#[get("/posts/mine")]
async fn list_my_posts(
    State(state): State<AppState>,
    session: Session,
    mut db: Db,
) -> AutumnResult<Json<Vec<Post>>> {
    let ctx = PolicyContext::from_request(&state, &session).await;
    let posts = Post::scope(&ctx).load(&mut *db).await?;
    Ok(Json(posts))
}
```

The same `PostScope` impl backs both the auto-generated
`#[repository(scope = PostScope)]` index endpoint and the hand-written
list above — one filter, two call sites.

## The `#[authorize]` attribute macro

Use `#[authorize]` on a handler when the resource is loaded via an
extractor. The macro short-circuits with the configured deny response
**before the handler body runs**.

```rust,ignore
use autumn_web::prelude::*;

#[get("/posts/{id}/edit")]
#[authorize("update", resource = Post)]
async fn edit_post(post: Post) -> AutumnResult<Markup> {
    Ok(html! { h1 { (post.title) } })
}
```

`from = <ident>` overrides the default snake-case binding when your
parameter is named differently:

```rust,ignore
#[delete("/posts/{id}")]
#[authorize("delete", resource = Post, from = target)]
async fn destroy(target: Post) -> AutumnResult<()> { /* ... */ Ok(()) }
```

For handlers that load the resource imperatively (typical Diesel
`first(&mut *db).await?`) call the runtime helper directly — same
semantics, inline:

```rust,ignore
use autumn_web::authorization::authorize;

let post: Post = posts::table.find(id).first(&mut *db).await?;
authorize::<Post>(&state, &session, "update", &post).await?;
```

## The `#[repository] policy =` argument

`#[repository(api = "/posts")]` auto-mounts JSON CRUD endpoints. Without
a `policy = ...` argument those endpoints accept writes from any
authenticated user — exactly the footgun the framework should not hand
out by default. Pair every `api =` with a `policy =`:

```rust,ignore
#[repository(Post, api = "/api/posts", policy = PostPolicy, scope = PostScope)]
trait PostRepository {
    fn find_by_subreddit_id(subreddit_id: i64) -> Vec<Post>;
}
```

The generated `GET /api/posts/{id}`, `POST /api/posts`,
`PUT /api/posts/{id}`, and `DELETE /api/posts/{id}` handlers each call the
registered `PostPolicy` before persisting; `GET /api/posts` calls the
registered `PostScope`.

In `prod` profile builds, `#[repository(api = "/...")]` **without** a
`policy =` is a startup-time error. Set
`[security] allow_unauthorized_repository_api = true` in `autumn.toml`
when the lack of authz is genuinely intended (e.g. a fully-public
read-only API).

## 403 vs 404

By default Autumn returns `404 Not Found` when a `Policy` denies an
action — clients cannot distinguish "the record exists but you cannot
touch it" from "the record does not exist." This mirrors Rails / Phoenix
defaults and avoids leaking record existence to unauthorized clients.

Flip to `403 Forbidden` via `autumn.toml` when the leak is acceptable:

```toml
[security]
forbidden_response = "403"
```

Tests set the same value via `TestApp::forbidden_response(...)`.

## Common patterns

### Ownership

```rust,ignore
fn can_update<'a>(&'a self, ctx: &'a PolicyContext, post: &'a Post)
    -> BoxFuture<'a, bool>
{
    Box::pin(async move { ctx.user_id_i64() == Some(post.author_id) })
}
```

### Role-augmented ownership

```rust,ignore
fn can_delete<'a>(&'a self, ctx: &'a PolicyContext, post: &'a Post)
    -> BoxFuture<'a, bool>
{
    Box::pin(async move {
        ctx.has_role("admin") || ctx.user_id_i64() == Some(post.author_id)
    })
}
```

### Group membership (consults related rows)

```rust,ignore
fn can_update<'a>(&'a self, ctx: &'a PolicyContext, doc: &'a Doc)
    -> BoxFuture<'a, bool>
{
    Box::pin(async move {
        let Some(pool) = ctx.pool.as_ref() else { return false };
        let mut conn = match pool.get().await {
            Ok(c) => c,
            Err(_) => return false,
        };
        let Some(user_id) = ctx.user_id_i64() else { return false };
        let is_member: bool = diesel::dsl::select(diesel::dsl::exists(
            memberships::table
                .filter(memberships::doc_id.eq(doc.id))
                .filter(memberships::user_id.eq(user_id)),
        ))
        .get_result(&mut conn)
        .await
        .unwrap_or(false);
        is_member
    })
}
```

## Migration: the reddit-clone update handler

**Before** — the hand-rolled snippet duplicated at every write endpoint:

```rust,ignore
let user_id: i64 = session
    .get("user_id").await
    .unwrap_or_default()
    .parse()
    .unwrap_or(0);
let post: Post = posts::table
    .filter(posts::slug.eq(&post_slug))
    .first(&mut *db).await?;
if post.author_id != user_id {
    return Err(AutumnError::forbidden_msg("You can only edit your own posts"));
}
```

**After** — typed, centralized, declarative:

```rust,ignore
let post: Post = posts::table
    .filter(posts::slug.eq(&post_slug))
    .first(&mut *db).await?;
autumn_web::authorization::authorize::<Post>(&state, &session, "update", &post).await?;
```

The same `PostPolicy` impl backs both the hand-written handler and the
auto-mounted `#[repository(api = "/api/posts", policy = PostPolicy)]`
endpoints. Permission rules live in one place; nothing in any handler
parses `user_id` from the session by hand any more, and
`git grep -n "author_id != user_id" examples/reddit-clone/` returns
empty.

## See also

- [Authentication](./authentication.md) — how a request acquires the identity
  these policies are evaluated against: sessions, login, and `#[secured]`.
- [Macro transparency: `#[authorize]`](./macro-transparency.md#authorize)
- [Coming from other frameworks](./coming-from-other-frameworks.md) — maps
  Pundit, Bodyguard, `@PreAuthorize`, and `before_action` onto autumn's
  `Policy` + `#[authorize]`.
- [`autumn_web::authorization`](../../autumn/src/authorization.rs) — the
  rustdoc for `Policy`, `Scope`, `PolicyContext`, `ForbiddenResponse`,
  and the `authorize` runtime helper.
