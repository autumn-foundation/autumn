# Compile-Time Query Budgets

`#[query_budget(N)]` makes a route's database query count a **build-time
contract**. Declare the ceiling on the handler, and the build fails if any
statically reachable path can issue more than `N` queries — on every branch,
whether or not a test exercises it.

```rust
use autumn_web::{get, query_budget};

#[get("/posts")]
#[query_budget(2)]
pub async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
    let posts = repo.find_all().await?;                                // 1
    let posts = repo.preload(posts, Post::preload().author()).await?;  // 2
    Ok(render(&posts))
}
```

Autumn can do this because it owns 100% of the query-issuing surface: every
statement reaches the database through a `#[repository]` method, a `preload`
batch, or a diesel-async executor handed the request's `Db` handle. The handle
is always named in the handler's signature, so the queries a handler can issue
are visible to the build.

## How this differs from the runtime tools

Autumn already ships two N+1 tools, and both are reactive:

| Tool | When it fires | Coverage |
|---|---|---|
| [Dev inspector](dev-inspector.md) N+1 badge | while you browse | only paths you happen to click |
| `TestResponse::assert_max_queries` (issue #1262) | while a test runs | only paths a test exercises |
| **`#[query_budget(N)]`** | **`cargo build`** | **every reachable path, tested or not** |

They are complements, not replacements. The compile-time gate proves an upper
bound; the runtime tools show you the actual SQL. Nothing about
`#[query_budget]` runs during a request — production enforcement remains the
job of request timeouts and load shedding.

---

## The worked example: a page listing rows and their associations

### Red build

A post index that renders each post's author. The author lookup sits inside the
loop over posts — the classic N+1:

```rust
use autumn_web::{get, query_budget};

#[get("/posts")]
#[query_budget(2)]
pub async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
    let posts = repo.find_all().await?;
    let mut authors = Vec::new();
    for post in &posts {
        authors.push(repo.find_author(post.author_id).await?);
    }
    Ok(render(&posts, &authors))
}
```

`cargo build` fails:

```text
error: `#[query_budget(2)]` cannot be proven: a database query (`find_author`)
       runs inside a loop, so this handler's query count grows with the size of
       the collection — the classic N+1

       Fix it by batching the lookup with `preload(...)`, by declaring the call
       site with `#[query_cost(N)]` / `#[query_exempt(reason = ...)]`, or by
       opting the handler out with `#[query_budget(unbounded, reason = ...)]`
  --> src/routes/posts.rs:9:5
   |
 9 |     for post in &posts {
   |     ^^^^^^^^^^^^^^^^^^
```

Note what did *not* have to happen: no test ran, no request was made, and the
page was never opened in a browser.

### Green build

Replace the per-row lookup with one batched `preload`:

```rust
use autumn_web::{get, query_budget};

#[get("/posts")]
#[query_budget(2)]
pub async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
    let posts = repo.find_all().await?;                                // 1
    let posts = repo.preload(posts, Post::preload().author()).await?;  // 2
    for post in &posts {
        let _author = post.author()?;   // already loaded — not a query
    }
    Ok(render(&posts))
}
```

The loop is still there; it just no longer issues anything. Two queries,
budget of two, clean build.

Both halves are compiled in CI as trybuild fixtures — see
`autumn/tests/compile-fail/query_budget_n_plus_one.rs` and
`autumn/tests/compile-pass/query_budget_valid.rs`.

---

## How the count is computed

| Construct | Cost |
|---|---|
| Straight-line statements | **sum** |
| `if` / `match` arms | **maximum** — only one arm runs, so the bound is the worst one |
| A loop whose body issues a query | **unbounded** (rejected under a finite budget) |
| A loop with a literal bound (`for _ in 0..3`) | body cost **× 3** |
| A loop whose body issues nothing | **0** — loops are free until they query |
| A chain rooted at a `Db` / repository handle | **1**, however many builder methods (`on_primary()`, `scoped()`, …) it carries |
| `.preload(rows, Post::preload().author().tags())` | **one per association** — two here, the batched `WHERE … IN (…)` loads |
| A diesel executor call (`.load(&mut *db)`, `.first(…)`, `.get_result(…)`) | **1** |
| `db.transaction(\|conn\| …)` | **1**, plus the callback body counted **once** |

A repository future is counted where it is **built**, not where it is awaited,
so collecting futures in a `.map(…)` and driving them with `join_all` later is
caught as the same N+1.

## What the analysis refuses to guess

Anything the analysis cannot read is **reported**, never assumed query-free.
That is the whole point: a false positive costs you one annotation, a false
negative ships an N+1 to production.

- **A helper function handed the handle** — `load_links(&mut db, id)`. Its body
  is another function; the macro sees only the call.
- **A macro body that names the handle** — `html! { … (fetch(&mut db)) … }`. A
  macro body is token soup to `syn`.
- **A closure that may run per element** — anything that isn't a transaction
  callback.
- **A `preload` spec that isn't a literal builder chain** — a spec built
  elsewhere and passed in as a variable.

Each of these fails with a diagnostic naming the call and the annotation that
resolves it.

## Escape hatches

### `#[query_budget(unbounded, reason = "…")]` — the whole handler

For legitimately dynamic work, where a ceiling would be a lie:

```rust
#[query_budget(unbounded, reason = "operator backfill, bounded by an explicit page size")]
pub async fn backfill(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<()> {
    for id in ids {
        repo.reindex(id).await?;
    }
    Ok(())
}
```

The `reason` is optional to the compiler and expected by review: it is the
sentence the next reader needs.

### `#[query_cost(N)]` — declare one statement's cost

Use it when a helper's query count is known but not visible:

```rust
#[query_budget(3)]
pub async fn show(mut db: Db) -> AutumnResult<Markup> {
    #[query_cost(2)]
    let links = load_links(&mut db, 1).await?;   // counted as 2
    let tags = tags::table.load(&mut *db).await?; // counted as 1
    Ok(render(&links, &tags))
}
```

The annotated statement's interior is not analysed — `N` is taken as given, so
keep it honest and next to the helper it describes.

### `#[query_exempt(reason = "…")]` — drop one statement from the ledger

For a call the analysis flags but which issues nothing:

```rust
#[query_budget(1)]
pub async fn show(mut db: Db) -> AutumnResult<Markup> {
    let posts = posts::table.load(&mut *db).await?;
    #[query_exempt(reason = "reads the warm cache only; verified query-free")]
    let banner = banner_for(&mut db).await?;
    Ok(render(&posts, &banner))
}
```

Both statement annotations are consumed by `#[query_budget]` and never reach
rustc — they are only meaningful inside an annotated function.

---

## Attribute order

`#[query_budget]` reads the handler and emits it unchanged, so it composes with
the route macro in either order:

```rust
#[get("/posts")]          // route macro outermost — preferred
#[query_budget(2)]
pub async fn index(repo: PgPostRepository) -> AutumnResult<Markup> { /* … */ }
```

Keeping the method attribute outermost matches the convention the other
handler attributes (`#[secured]`, `#[throttle]`) document, and keeps the
handler's real return type visible for OpenAPI response schemas.

It also works on plain helper functions, not just routes — useful for pinning a
shared query helper's cost where it is defined.

## What the expansion leaves behind

Each annotated function gets a hidden constant recording the contract and the
proof:

```rust
assert_eq!(__AUTUMN_QUERY_BUDGET_index.declared, Some(2));
assert_eq!(__AUTUMN_QUERY_BUDGET_index.proven_max, Some(2));
assert_eq!(__AUTUMN_QUERY_BUDGET_index.headroom(), Some(0));
```

See [`StaticQueryBudget`](https://docs.rs/autumn-web/latest/autumn_web/query_budget/struct.StaticQueryBudget.html).
It carries no runtime behaviour; it exists so tests and tooling can read the
proof back.

## Scope of the first slice

Deliberately out of scope for now:

- Allocation, CPU, and latency/cost budgets (the north star; this slice is
  query count only).
- Queries issued inside background jobs, `#[scheduled]` tasks, or plugin code.
- Runtime enforcement or per-route SLO dashboards — this is a build-time gate,
  not a production limiter.
- Proving exact counts for fully dynamic control flow. The gate targets the
  static and loop-shaped cases and requires an explicit annotation elsewhere.

## Soundness contract

Within an annotated function, every construct that can issue a query is either
counted or reported — never silently skipped. Counting rests on two framework
contracts, both of which the macro states in its diagnostics:

1. One repository-chain call issues one query, and one `preload` association
   issues one batched query.
2. A call site the analysis cannot read declares its own cost with
   `#[query_cost(N)]`, or is excluded with `#[query_exempt(reason = "…")]`.

A hand-written `#[repository]` method that issues more than one query must
therefore declare it at the call site. Both annotations are opt-ins a reviewer
can see in the diff — which is the point: the unprovable parts are visible
rather than assumed.

---

## See also

- [Repositories](repositories.md) — `#[repository]` and the derived finders
- [Dev Request Inspector](dev-inspector.md) — the runtime N+1 badge
- [Testing](testing.md) — driving handlers with `TestApp`; `TestResponse::assert_max_queries` / `assert_no_n_plus_one` are the runtime counterpart to this gate
