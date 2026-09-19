# The Architecture Graph

`autumn graph` answers structural questions about an application by asking the
**binary itself**, not by reading the codebase.

```console
$ autumn graph impact Post
Changing model Post affects 1 repositor(y/ies), 12 route(s) and 2 job(s)
  repository PostRepository over Post
  route      GET /api/posts
  route      GET /api/posts/{id}
  route      DELETE /r/{sub_slug}/posts/{post_slug}
  route      GET /
  route      POST /r/{sub_slug}/posts/{post_slug}/tags
  route      GET /r/{sub_slug}/posts/{post_slug}
  route      GET /posts/{post_id}
  route      POST /submit
  route      POST /r/{sub_slug}/posts/{post_slug}
  route      GET /r/{slug}
  route      POST /posts/{post_id}/downvote
  route      POST /posts/{post_id}/upvote
  job        post_publication
  scheduled task hot-rank-calculator (every 15m)
```

Autumn already declares every architectural element through proc-macros it
owns — `#[route]`, `#[static_get]`, `#[model]`, `#[repository]`, `#[job]`,
`#[scheduled]`, `#[task]`. Until now none of that survived expansion as
anything you could query: "which routes touch this table" meant reading the
whole codebase, and every architecture diagram was stale the moment it was
drawn.

The graph is derived at compile time from those declarations and assembled
inside the binary. There is no reflection gap, no external registry to keep in
sync, and no side file to go stale relative to the code that is running.

## The commands

```console
autumn graph show                              # the whole graph
autumn graph touches posts                     # which routes and jobs reach a table
autumn graph impact Post                       # what a change to a model affects
autumn graph show --json                       # the document, for other tools
autumn graph show --manifest architecture.json # write it
autumn graph show --check architecture.json    # fail on drift  (the CI gate)
```

`touches` and `impact` accept a model name, its table, a repository trait, the
generated `Pg*` implementation type, a job name, or a node id. Table names match
case-insensitively; type names do not, because `Post` and `post` are different
Rust items.

`--check` compares against a committed copy and names *what* moved:

```console
✗ The architecture graph has drifted from the committed copy:
  ~ route GET /r/{slug}
      auth secured -> none
  - read route:app::routes::posts::index -> model:app::models::Post
```

That second line is the interesting one: a route that quietly stopped reading a
table is a refactor nobody reviewed.

## From the running binary

The same graph is served from `/actuator/graph`:

```console
$ curl -s localhost:3000/actuator/graph | jq '.completeness'
{
  "declared_routes": 39,
  "mounted_routes": 39,
  "models": 4,
  "repositories": 3,
  "jobs": 4,
  "generated_routes": 4,
  "opaque_mounted_routers": 1,
  "unmodelled_mounted_routes": [
    "GET /_autumn/inspect",
    "GET /actuator/graph",
    "…",
    "WS /ws/feed",
    "WS /ws/r/{slug}"
  ]
}
```

`unmodelled_mounted_routes` is long on purpose: it names every served endpoint
the graph has no macro declaration for — the framework's own probes, actuator
and dev endpoints as well as the app's `#[ws]` handlers. Naming them is what
stops the completeness section reading as a complete account of the served
surface when it is not.

It is **sensitive-gated**, like `/actuator/env`: the document names every route,
its auth requirement, and the table each one touches — a map of exactly where an
attacker would look first. It is available when `actuator.sensitive = true`
(the default in dev), and absent in production unless you opt in.

A process that never built an application router answers `503` with an
explanation rather than `404`, so "this process published no graph" reads
differently from "this build has no such endpoint".

A **worker** (`AUTUMN_ROLE=worker`) serves the probe-only router and mounts no
application route, so its graph reports every declared route as
`mounted: false` and names them under `unmounted_routes`. That is the honest
answer to "what does *this process* serve" — the elements are still compiled
in, they are just not being served here. Its
`unmodelled_mounted_routes` is empty rather than listing the probes and
actuator it does serve; that gap is tracked separately.

One number can legitimately differ between `autumn graph show` and
`/actuator/graph`: `opaque_mounted_routers`. The dump exits before startup
adds any configuration-driven router (a blob store, the SEO endpoints, an
inbound-mail webhook), so a running binary can count more of them than the
committed document does.

## What is in the graph

**Nodes** — one per macro-declared element:

| Node kind        | Declared by                        | Carries |
|------------------|------------------------------------|---------|
| `route`          | `#[get]`, `#[post]`, …             | method, mounted path, auth requirement |
| `static_route`   | `#[static_get]`                    | the same; the node kind is the pre-rendered marking |
| `model`          | `#[model]`                         | its table, and its relations' edge tables |
| `repository`     | `#[repository]`                    | its model, table, `Pg*` type, auto-API prefix |
| `job`            | `#[job]`                           | the registered job name |
| `scheduled_task` | `#[scheduled]`                     | the schedule as declared |
| `one_off_task`   | `#[task]`                          | the registered task name |

A route is a node whether or not the app mounts it. One that no `routes![]`
list mounts is reported in `completeness.unmounted_routes` — a route that
silently stopped being served is drift, not a detail.

**Edges** — one per relationship, each carrying its `access` (`read`, `write`,
`read/write`) and its `provenance`:

* `declaration` — stated outright. `#[repository(Post)]` names its model;
  `#[repository(api = "/api/posts")]` names the mount prefix of a CRUD surface
  no `#[route]` declares, so those routes are attributed to it too.
* `signature` — an extractor the handler declares. A handler taking
  `PgPostRepository` provably reaches everything that repository does.
* `body` — a name in the item's own tokens: a model type, a `diesel` table
  module, or an identifier inside a raw-SQL string literal.

## How routes and jobs are linked

A repository states its model. A route does not — it just *names* things. So
the route and job macros collect the candidate names in their own tokens and
publish them; the framework resolves them at link time against what actually got
declared.

A name is a candidate when it sits next to a `::` path separator
(`posts::table`, `Post::find`, `crate::schema::subreddits`), when it is
type-shaped (`Post`, `PgPostRepository`), or when it appears inside a string
literal that reads as SQL — which is how a scheduled task doing
`sql_query("UPDATE posts SET hot_rank = …")` is still linked to the `posts`
table.

The collection is a deliberate superset with no stop-list. A missing edge is a
false negative in an impact answer — the one failure this feature cannot afford
— while a candidate that resolves to nothing is simply dropped.

## What the derivation cannot see

The graph carries these in its own `limits` field, so the document cannot be
read as more than it is:

* **Cross-function indirection.** Edges come from one item's own tokens. A
  handler that calls a helper in another module, and reaches the table only
  from there, is not linked to it. Following that call is dynamic call-graph
  tracing, which this slice deliberately excludes.
* **Name-based resolution.** A type alias or a `use … as …` rename is not
  resolved, and a model whose name matches a common type is linked wherever
  that name appears.
* **Runtime-assembled SQL.** Only string literals that read as SQL are scanned;
  a query built from fragments at runtime is invisible.
* **Relation granularity.** `#[votable]` and `#[commentable]` put their
  generated methods on the model's *repository*, so a route holding that
  repository is reported as reaching the edge table whether or not it calls
  them. (Without this, `autumn graph touches votes` would miss every upvote
  route, because none of them names `votes`.)
* **Access is declared intent.** A route's access is its HTTP method — safe
  methods read, everything else writes. A job's is whether its tokens carry
  mutation evidence. Neither is an executed statement.
* **Routes mounted by other mechanisms.** A `#[ws]` handler or a framework
  endpoint is not a `#[route]` and is not a node; those mounts are *named* in
  `completeness.unmodelled_mounted_routes` — with two exceptions still to
  close: the OpenAPI JSON and Swagger routes an app enables with
  `.openapi(...)`, and a worker's own probe and actuator mounts. A raw `merge`/`nest` router is
  worse than unmodelled — it exposes no API to list its endpoints at all, so
  they cannot be named, only counted, in
  `completeness.opaque_mounted_routers` (the same count `autumn routes audit`
  hard-fails its coverage gate on). Either way the document says what it could
  not see rather than reading as a complete account of the served surface.
* **Cross-module `use`.** Only an item's own tokens are read, so a
  module-level `use crate::schema::posts::dsl::*` followed by a bare
  `posts.filter(…)` names no candidate. Write `posts::table` or the model type
  in the handler to be linked.

## Keeping it honest

Because nodes come from the declarations themselves, nothing can be missing
without a macro changing. `examples/reddit-clone/tests/architecture_graph.rs`
proves it the only way that is not circular: it censuses the reference app's
*sources* for every declaring attribute, runs the binary's own graph dump, and
fails when the two disagree. It also pins the recall claim — `impact Post` must
return every hand-verified handler and job that reaches the `posts` table,
listed by name.

Wire the drift gate into CI wherever you run the app's build:

```yaml
- run: autumn graph show --check architecture-graph.json --release
```

`--release` matters for the same reason it does for `autumn data-flow`: the
graph describes the binary that produced it, and an element behind
`#[cfg(not(debug_assertions))]` exists only in the build that ships.

## Related

* [Route auth coverage](route-auth-coverage.md) — proves each route's auth posture; the
  graph *states* it.
* [Data classification](data-classification.md) — which columns can leave the
  process.
* [The Agent Authority Envelope](agent-authority.md) — what an agent-callable
  handler is allowed to do.
* [Cache coherence](cache-coherence.md) — that no write leaves a cached read
  stale.
