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
error: `#[query_budget(2)]` cannot be proven: a database query
       (`find_author`) runs inside a loop, so this handler's query count
       grows with the size of the collection — the classic N+1.

       Batch the per-row lookup into one query with `preload(...)`, put
       `#[query_cost(N)]` on the loop statement when the iteration count
       is bounded by something the analysis cannot see, or opt the handler
       out with `#[query_budget(unbounded, reason = ...)]`. See
       docs/guide/query-budgets.md.
  --> src/routes/posts.rs:9:5
   |
 9 |     for post in &posts {
   |     ^^^
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
| `return`, `break`, `continue` | the early path does not include the cost of the code it skips. `if cached { return repo.find_cached().await; } repo.find_fresh().await` is **1**. A `return` inside a loop does not reach the code after the loop. A `break` does |
| A loop whose body issues a query on a path that starts another pass | **unbounded** (rejected under a finite budget) |
| A loop with a literal bound (`for _ in 0..3`) | **× 3** for a path that starts another pass. A path that leaves the loop (`break`, `return`) is paid once: `for _ in 0..3 { repo.a().await?; break; }` is **1** |
| A loop whose body issues nothing | **0** — loops are free until they query |
| A chain rooted at a `Db` / repository handle | **1**, however many builder methods (`on_primary()`, `scoped()`, `limit()`, …) it carries — splitting the chain across `let` bindings does not change the count |
| `.preload(rows, Post::preload().author().tags())` | **one per association** — two here, the batched `WHERE … IN (…)` loads, plus **1** for a finder ahead of it in the same chain |
| A diesel executor call (`.load(&mut *db)`, `.first(…)`, `.get_result(…)`) | **1** |
| An associated function handed the handle (`Post::published(&mut db)`) | **reported** — put `#[query_cost(N)]` on the statement |
| `db.tx(\|conn\| …)` / `db.tx_with(…)` / `db.tx_immediate(…)` | **1**, plus the callback body counted **once** — the callback's `conn` is tracked, so a helper handed it is still reported |
| `repo.find_in_batches(…)` / `find_each(…)` | **unbounded** — a keyset walk issues one query per batch, a count set by the table's size |
| An `Option`/`Result` combinator closure (`map`, `and_then`, `unwrap_or_else`, …) | counted **once** when the receiver is known to be an `Option` or a `Result`; otherwise it may run per element |

A repository future is counted where it is **built**, not where it is awaited,
so collecting futures in a `.map(…)` and driving them with `join_all` later is
caught as the same N+1.

## How a handle is tracked

The analysis follows the handle through every name that holds it:

- **Bindings.** The analysis reads each of these binding forms: `let`,
  `let … else`, assignment, `if let`, `while let`, `match` arms, `for`
  patterns, closure parameters and transaction callback parameters. A type
  annotation also marks a handle: `let r: PgPostRepository = …`. An
  `Arc<PgPostRepository>`, `Box<…>`, `Rc<…>`, `dyn PostRepository` or
  `impl PostRepository` is a handle too. A type annotation made only of
  standard and primitive types, such as `let ids: Vec<i64> = …`, marks the
  binding as plain. rustc checks it, so it cannot hold a handle.
- **Scopes.** A `let` in a block ends with the block. An assignment to a name
  declared outside the block stays after the block. A value that a block, an
  arm or a `break` gives keeps what it holds:
  `let alias = { let moved = repo; moved };` makes `alias` a handle.
- **Branches.** After an `if`, a `match` or a loop, a name holds a handle when
  it holds one on any path. The bindings at a `return`, `break` or `continue`
  apply where that exit lands.
- **Containers.** A value built from a handle holds it: `[repo]`,
  `vec![repo]`, `(repo, 1)`, `Some(repo)`, `Ctx { db }`. A parameter of type
  `Vec`, `VecDeque`, `Option`, map, set, tuple, array or slice of a handle
  type also holds handles (`Vec<PgPostRepository>`, `Option<Db>`,
  `HashMap<i64, PgPostRepository>`). So does a `Result` with a handle or
  such a container on either side.
  - A name bound to a struct or tuple literal records what each part holds.
    In `let ctx = PageCtx { repo: &repo, user };`, `ctx.repo` is a handle
    and `ctx.user` is not.
  - An index (`repos[0]`), a field (`pair.0`), a pattern (`Some(r)`,
    `for r in repos`) or `?` gives a handle.
  - A method that the container's own type has (a `Vec` method on a `Vec`,
    an `Option` method on an `Option`) is not a query. Each type has its own
    list: `sort` is a `Vec` method, not a `VecDeque` one. The method gives a
    handle when it returns a part (`repos.remove(0)`, `maybe.unwrap()`), and
    a container when it returns a view or an `Option` of a part
    (`repos.iter()`, `repos.first()`, `deque.remove(0)`). Any other method
    on the container is reported (`repos.refresh_all()`, or an
    extension-trait `repos.ok()`).
  - A method or function given a handle may store it: after
    `list.push(repo)` or `fill(&mut list, &repo)`, `list` holds a handle. A
    callback stores what it returns (`slot.get_or_insert_with(|| &repo)`).
    Every method on a user struct that holds a handle (`ctx.clear()` on
    `Ctx { repo }`) is reported too.
  - A callback's result holds what the callback returns, whatever the
    receiver holds: `ids.iter().map(|_| &repo)` gives handles, and so do
    `fold`, `find_map`, `then` and `unwrap_or_else`. A `return` in the
    callback counts too. A closure that names a handle holds it, so
    `ids.iter().map(make)` and `make()` keep the handle.
  - A container of containers or of user values (`Vec<Vec<PgPostRepository>>`,
    `Option<Vec<…>>`, `[ctx]`, `repos.chunks(2)`, or
    `repos.iter().map(|r| Ctx { repo: r })`) keeps that shape for all its
    parts. Every method on it or on a part of it is reported.
    `Arc<Vec<…>>` is a plain container: a smart pointer adds no depth.
    `left.append(&mut right)` adds the parts of `right`, so `left` stays a
    plain container.
  - A helper handed the container is reported.

When the parts are not known, for example in a parameter or a destructured
value, every part counts as a handle. This can over-count, but it never
under-counts. A query future in an array (`join_all([repo.a(), repo.b()])`)
is counted where it is built, so the array does not count as a container.

## What the analysis refuses to guess

Anything the analysis cannot read is **reported**, never assumed query-free.
That is the whole point: a false positive costs you one annotation, a false
negative ships an N+1 to production.

- **A helper function handed the handle** — `load_links(&mut db, id)`. Its body
  is another function; the macro sees only the call.
- **An associated function handed the handle** — `Post::published(&mut db)`.
  It has the same shape as `ReportBuilder::build(&mut db)`, which can issue any
  number of queries. Put `#[query_cost(1)]` on the statement that calls a
  one-query finder.
- **A macro body that `await`s while naming the handle** — `html! { …
  (fetch(&mut db).await?) … }`. A macro body is token soup to `syn`. A template
  that merely *passes* the handle to a render helper is fine: only an `await`
  inside the body makes it suspicious, and logging/formatting macros
  (`tracing::debug!`, `format!`, …) are never suspicious.
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

Both annotations are read at **statement** level. You cannot annotate a
sub-expression or a call buried inside a closure — hoist it to its own
statement first. A compound statement *is* one statement, so
`#[query_cost(10)]` on a `for` loop declares the whole loop's cost, which is
the supported way to bound a loop whose iteration count the analysis cannot
see:

```rust
#[query_budget(10)]
pub async fn refresh(repo: PgPostRepository, ids: Vec<i64>) -> AutumnResult<()> {
    #[query_cost(10)]   // the page size is capped upstream at 10
    for id in ids {
        repo.reindex(id).await?;
    }
    Ok(())
}
```

Because the annotated statement's interior is not analysed, an annotation on a
loop or block hides everything inside it. Keep the scope as small as the fix
allows.


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
the route macro in either order. The same holds for `#[secured]`, `#[step_up]`,
`#[authorize]`, and `#[throttle]`, which wrap the body in an `async` block the
analysis walks through — pinned by the trybuild fixtures in
`autumn/tests/compile-pass/`:

```rust
#[get("/posts")]          // route macro outermost — preferred
#[query_budget(2)]
pub async fn index(repo: PgPostRepository) -> AutumnResult<Markup> { /* … */ }
```

Keeping the method attribute outermost matches the convention the other
handler attributes (`#[secured]`, `#[throttle]`) document, and keeps the
handler's real return type visible for OpenAPI response schemas.

Some attributes rewrite the handler body before `#[query_budget]` reads it:
`#[secured]`, `#[step_up]`, `#[authorize]`, and `#[throttle]` wrap it in an
`async` block, and `#[cached]` wraps it in an immediately-invoked closure. The
analysis walks through both shapes, so it never blames you for a closure you did
not write. (`#[cached]` requires `Hash` arguments and a `Clone + Deserialize`
return, so it cannot take a `Db` or repository extractor in the first place —
a cached function has no queries to budget.)

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

1. One repository-chain call, one diesel executor call, or one `preload`
   association issues one query.
2. A call site the analysis cannot read declares its own cost with
   `#[query_cost(N)]`, or is excluded with `#[query_exempt(reason = "…")]`.

A hand-written `#[repository]` method that issues more than one query must
therefore declare it at the call site. Both annotations are opt-ins a reviewer
can see in the diff — which is the point: the unprovable parts are visible
rather than assumed.

### Where the boundary sits

The analysis tracks a handle from where the signature names it (the `Db` /
repository extractor), through fields and conventionally-named accessors
(`self.repo`, `state.db`, `app.pool()`), through every binding and container
(see [How a handle is tracked](#how-a-handle-is-tracked)), and into transaction
callbacks. Two things sit outside it, by construction:

- **A handle obtained some other way** — for example a repository pulled off an
  application-state extractor by an application-specific method
  (`state.posts()`). Take the `Db` or repository extractor in the signature and
  the gate sees everything.
- **Queries issued through ambient state rather than a handle** — `Job::enqueue`
  writing a job row, audit sinks, session and flash writes. These reach the
  database without any handle in the handler's signature, so no static
  attribution is possible; they are the same class as the background-job work
  listed under Scope above.

### `proven_max` is not `query_count()`

The compile-time bound and the runtime counter measure deliberately different
things: `TestResponse::query_count` excludes transaction control statements
(`BEGIN`/`COMMIT`/`SAVEPOINT`/…), while the static model charges 1 for the
`db.tx(…)` call itself. A handler with one query inside one transaction has
`proven_max == 2` and `query_count() == 1`. Tune each against its own tool.

---

## See also

- [Repositories](repositories.md) — `#[repository]` and the derived finders
- [Dev Request Inspector](dev-inspector.md) — the runtime N+1 badge
- [Testing](testing.md) — driving handlers with `TestApp`; `TestResponse::assert_max_queries` / `assert_no_n_plus_one` are the runtime counterpart to this gate
