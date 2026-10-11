# Cache Coherence — Proving Cached Reads Are Never Stale

> Issue [#1716](https://github.com/autumn-foundation/autumn/issues/1716).
> Related: [Fragment Caching](fragment-caching.md), [Cache Stampede](cache-stampede.md),
> [Security Posture Manifest](security-posture-manifest.md), [Query Budgets](query-budgets.md).

Cache invalidation is the canonical hard problem because nothing in a normal
framework connects the two ends of it. A cached value is derived from rows; a
mutation changes those rows; and the only thing linking them is a developer
remembering to write `cache.invalidate(key)`. Forget it once and the app serves
wrong data — silently, indefinitely, to whichever customer happens to hit that
key.

Autumn owns both ends. Reads flow through `#[cached]`, the fragment cache and
the read-through cache; writes flow through `#[repository]`. That means the
build can assemble the dependency graph and **prove** coherence rather than
test for it.

```console
$ autumn cache audit -p saas          # after deleting the invalidates(...) clause
🍂 autumn cache audit

1 cached reads (1 declared, 0 derived, 0 undetermined), 16 repository mutations, 0 acknowledged-stale

error: 1 cached read can be left stale by a repository write

  the cached read saas::repositories::cached_project_count is derived from saas::models::Project,
    which 8 repository writes mutate without invalidating it:
      ProjectRepository::delete_by_id, ProjectRepository::delete_many, ProjectRepository::save, ProjectRepository::save_many, ProjectRepository::save_many_skip_invalid, ProjectRepository::update, ProjectRepository::update_many, ProjectRepository::upsert_many
    read at    examples/saas/src/repositories.rs:50
    written at examples/saas/src/repositories.rs:29
    fix: add invalidates(saas::repositories::cached_project_count) to the repository, or
         acknowledge the staleness with #[acknowledge_stale(reason = "…")].

$ echo $?
1
```

That is the shipped `examples/saas` with one clause removed; put it back and the
same command exits `0` with `✓ No cached read can be left stale by a repository
write.`

One missing edge is one finding block, not one per generated write method — the
fix is a single attribute, so the report names a single edit and lists the
writes it covers.

## Quick start

Declare what a cached read is derived from:

```rust
use autumn_web::cached;

#[cached(ttl = "30s", key(tenant_id), reads(Project), result)]
pub async fn cached_project_count(
    tenant_id: String,
    repo: &PgProjectRepository,
) -> AutumnResult<i64> {
    repo.count().await
}
```

Declare which cached reads a repository's writes dirty:

```rust
#[autumn_web::repository(Project, tenant_scoped, invalidates(cached_project_count))]
pub trait ProjectRepository {}
```

Then wire the CI gate:

```yaml
- name: Prove cache coherence
  run: autumn cache audit --manifest target/cache-coherence.json
```

## How it works

| Step | Where | What it produces |
|------|-------|------------------|
| 1 | `#[cached]` | which models the value is derived from, plus the read's identity |
| 2 | `#[repository]` | which model each write method mutates |
| 3 | `invalidates(...)` | an edge from a write to a read, resolved by **rustc** |
| 4 | `autumn cache audit` | the manifest, and a non-zero exit on any uncovered pair |

Steps 1–3 are `inventory` registrations emitted by the macros. Step 4 runs the
app's own binary with `AUTUMN_DUMP_CACHE_COHERENCE=1`, which is why the check is
whole-app: a cached read in one crate can be dirtied by a repository in another,
or by a plugin the app merely depends on, and link time is the only place all of
those registrations exist together. This is the same shape as
[`autumn routes audit`](route-auth-coverage.md).

### The rule

A `(read, write)` pair fails the build when **all four** hold:

1. the read's dependency set is known,
2. neither side is acknowledged-stale,
3. the read's dependency set contains the write's model, and
4. no invalidation edge names that read.

### Read identity is the cache-key namespace

A cached read is identified by `module_path!() :: <fn name>` (or `module_path!() :: <Type> :: <fn name>` with a `scope`) — the exact string
`make_cache_key` already prefixes every one of its keys with. The manifest's
identity and the runtime key space are the same string, by construction.

Two impls in one module can hold a same-named `#[cached]` function. Put
`#[cached_impl]` on each `impl` block. It adds the type to the identity:
`module::Products::get`. You can also write `#[cached(scope = "Products")]`.
Without a scope, both functions share one key space, and `autumn cache audit`
reports the shared identity. The scope is opt-in.

The scope is the type path without generics: `a::Store`. Two impls can still
share it: `impl Store<A>` and `impl Store<B>`, or two traits on one type with a
same-named function. Give those a different `scope = "..."` by hand.

```rust
use autumn_web::prelude::*;

#[cached_impl]
impl Products {
    #[cached]
    async fn get(id: i64) -> String { /* .. */ }
}
```

### Invalidation edges are checked by the compiler

`#[cached]` emits an identity constant beside the function it wraps.
`invalidates(crate::views::recent_posts)` rewrites the last path segment to that
constant, so the edge only compiles when it names a real `#[cached]` function
that is actually in scope. The constant inherits the function's visibility, so a
cached read named from another module has to be at least `pub(crate)`:

```
error[E0425]: cannot find value `__AUTUMN_CACHE_READ_ID__not_a_cached_function` in this scope
```

An invalidation edge in this system is therefore not a string in a table
somebody has to keep in sync. It is a resolved path.

## Declaring dependencies

### `reads(...)` — the strong claim

```rust
#[cached(reads(Post, crate::models::Comment))]
async fn feed() -> Vec<Entry> { … }
```

Each entry is a **path**, so a typo is a compile error at the declaration site.
The manifest tags these `declared`.

Declared beats derived: an explicit `reads(...)` replaces the analysis rather
than being cross-checked against it. That is the point — you know what the
function reads and the analysis only guesses — but it does mean
`#[cached(reads(Tag))]` on a function that actually reads `Post` audits clean.
The declaration is trusted; make it true.

### Derivation — the fallback

Omit `reads(...)` and the macro recovers what it can from the function's own
signature and body:

* the **concrete** repository struct anywhere in scope — `PgPostRepository`, a
  parameter, a turbofish — contributes the model *that repository declares*.
  The macro emits `<PgPostRepository>::__AUTUMN_MODEL_NAME`, the constant
  `#[repository]` puts on every generated struct, so rustc resolves the model
  and `#[repository(Comment)] trait ModerationRepository` correctly yields
  `Comment` rather than the `Moderation` its name suggests;
* a reading associated call on a model type, from a **closed list** of verbs
  (`find_all`, `find_by_id`, `count`, `exists_by_id`, `list`, `page`, `all`,
  `load`, `first`) — `Post::find_all(db)` names `Post`, `Post::find_by_slug(…)`
  does not.

A repository reached only through its **trait** — `repo: &impl PostRepository`
— is the one case derivation cannot resolve: the constant lives on the concrete
struct, and the trait's name is not evidence of anything. Such a read is
recorded `undetermined`, so it is reported and never gates. Take the concrete
`Pg*` type, or declare `reads(...)`.

The manifest tags these `derived`, and that tag is doing real work — it is a
weaker claim than `declared`:

* **Incomplete.** A dependency reached through a helper function the analysis
  cannot read is missed. That is why a read it recovers nothing from is recorded
  as `undetermined` rather than as having no dependencies.

It is not, however, *wrong*: every model in a derived set comes either from a
model type named outright or from a repository's own declaration, so a derived
dependency naming a model that does not exist — which would make a real
violation audit clean — cannot happen.

Declare `reads(...)` wherever the answer matters.

### `undetermined` — reported, never failed

A cached read whose dependency set could not be established is recorded as
`undetermined`. The default gate does not fail on it, because a checker that
fails on what it merely could not read is a checker that gets deleted from CI.
It is reported in the manifest and in the audit's own output, and
`autumn cache audit --strict` turns it into a failure for apps that want the
stronger posture.

An `undetermined` read that also carries `acknowledge_stale` is exempt from
that, `--strict` included: the acknowledgement is *the* opt-out from this gate,
and a read whose staleness the author has already signed off on must not fail
through a second door. It stays visible — under the acknowledged heading, with
its reason — rather than disappearing.

Read the summary line before you trust a green build:

```
12 cached reads (11 declared, 1 derived, 0 undetermined), 30 repository mutations, 0 acknowledged-stale
```

A green audit over mostly-`undetermined` reads proves very little, and says so.
Each undetermined read is listed with its `file:line`, as is every
acknowledged-stale opt-out and its reason — a hatch should never be just a
number in a summary line.

## Discharging an obligation

### 1. Declare the invalidation

```rust
// Every write on this repository dirties both reads.
#[repository(Post, invalidates(crate::views::recent_posts, crate::views::post_count))]
pub trait PostRepository {
    // A per-method edge ADDS to the trait-level ones; it never replaces them,
    // so annotating one method cannot silently drop the repository-wide
    // declaration.
    #[invalidates(crate::views::by_author)]
    fn delete_by_author_id(author_id: i64) -> ();
}
```

A repository that declares any edge invalidates those reads **after each
write commits**. You do not need to call anything:

```rust
repo.save(&new).await?; // committed, then the declared reads are dropped
```

Each generated write runs and commits its own transaction. When the write
ends, it calls `invalidate_declared_caches_async()`. See
[Commit-bound invalidation](#commit-bound-invalidation).

The repository also gets the invalidator itself, for a write that does not go
through the repository:

```rust
if !PgPostRepository::invalidate_declared_caches_async().await {
    // Already logged and counted. Stale entries can be served until the TTL.
}
```

`invalidate_declared_caches()` is the sync form. Both are `#[must_use]`: they
return whether every declared read was invalidated **completely**. An ignored
`false` means the stale value can still be served. See
[What this does not prove](#what-this-does-not-prove).

A method-level `#[invalidates(...)]` is folded into the same call. That
over-invalidates on other write paths, which is safe; the alternative — an edge
that discharges the gate with nothing callable behind it — is the paperwork this
feature exists to prevent.

### Commit-bound invalidation

Issue [#3056](https://github.com/autumn-foundation/autumn/issues/3056).

The order is: write, commit, then invalidate.

1. The write method runs its transaction and commits.
2. Before its `after_create` or `after_update` hook, or at its end if no hook
   follows, it bumps the epoch of each declared namespace.
3. It clears the local stores and asks the backend to drop the namespace.

Thus an `after_*` hook that reads a declared cached read gets the committed
row. Other readers also get it while the hook runs.

If you invalidate before the commit, a reader can refill the old value between
the `DEL` and the `COMMIT`. If you invalidate after the commit, step 2 stops the
insert of a reader that read the old row before the commit.

| Case | Result |
| --- | --- |
| The write returns `Err` | Invalidated too: a write can commit and then fail in an `after_*` hook. After a rollback, the cost is one cache miss. |
| The request is cancelled, or the write panics | A guard spawns the invalidation, so it still runs. A cancel during the `COMMIT` round trip can invalidate before the server applies the commit; `commit_hooks` covers that case. |
| `with_lock`, `find_or_create_by_*`, the retention sweep | Also invalidate. `find_or_create_by_*` skips it when it found the row, and the sweep skips it when it deleted nothing. |
| The backend sweep fails | Retried (Redis: 3 attempts, jittered backoff). Then `warn!` and `autumn_cache_invalidation_failures_total`. The write stays `Ok`: it is committed. |
| The repository has `commit_hooks` | The durable runner also invalidates, before `after_*_commit`. This covers a crash between the commit and the inline invalidation. A failure is logged and counted. It does not fail the row, so a cache outage does not delay or dead-letter your hooks. |
| A write with several commits (`save_many` in chunks) | One sweep per commit that a hook or the end follows. |
| Each write | One namespace sweep per declared read. On Redis this is a `SCAN MATCH` over the keyspace, so write latency grows with the keyspace. |
| A write inside `Db::tx` | The repository write uses its own connection and commits on its own, so it invalidates at its own commit. |

The local fence is per process. `RedisCache` also keeps a fill epoch in Redis.
An invalidation raises it before the sweep. A fill reads it on a miss and
stores its value only if it did not change. So a fill on **another replica**
that read the old row before the commit cannot write it back. A backend with no
shared epoch has only the per-process fence. On that backend, the stale
write-back can still happen.

A fill that starts after the invalidation but reads a **lagging read replica**
can cache the old row. A TTL bounds this case.

The shared epoch costs one `GET` on each miss. A hit costs nothing extra. The
store is one Lua call, in place of the plain `SET`.

The Lua call needs scripting. If you use a Redis ACL, the cache user needs
`+eval`, `+evalsha` and `+script|load`, and the key pattern `~{prefix}:*`. With
no scripting permission, no value is stored and every miss recomputes.
`RedisCache` logs one `warn!` when this happens.

### 2. Or acknowledge the staleness

```rust
#[cached(reads(Post), acknowledge_stale = "the ticker tolerates a 5s lag")]
async fn post_ticker() -> i64 { … }
```

```rust
#[repository(Post, acknowledge_stale = "seed-only writes, never runs in production")]
pub trait SeedPostRepository {}
```

The reason is **mandatory and must be non-blank** — an empty one is a compile
error, and "non-blank" is Unicode-aware, so a lone `U+2003 EM SPACE` does not
sneak past it. Every acknowledgement is an entry in the manifest, so an escape
hatch is always visible in review.

An acknowledged read is out of the gate entirely: it is never a violation, and
never a `--strict` undetermined failure either.

## Reads the macros cannot see

The fragment and read-through caches are plain function calls with a runtime
key; there is no annotated item to analyse. Declare them:

```rust
autumn_web::declare_cached_read! {
    id = "blog::routes::sidebar_fragment",
    kind = Fragment,
    reads = [crate::models::Post, crate::models::Tag],
}
```

`id` is the read's identity in the manifest **and** the key prefix an
invalidation sweeps for. It is not derived from the call site, so the call site
has to key under it:

```rust
// Fragment: the `_in` variants take the namespace.
autumn_web::cache::cache_fragment_global_in(
    "blog::routes::sidebar_fragment",
    format_args!("post:{}", post.id),
    post.updated_at.timestamp(),
    None,
    || html! { … },
);

// Read-through: build the key with the id as its namespace.
let key = autumn_web::cache::make_cache_key("blog::routes::sidebar_fragment", &(post.id,));
autumn_web::cache::get_or_compute(&my_cache, &key, None, || expensive());
```

The plain `cache_fragment` / `cache_fragment_global` build their own key
(`fragment:{len}:{identity}:{version}`). That prefix carries no per-read
identity — every fragment in the app shares it — so a sweep for
`blog::routes::sidebar_fragment` matches none of them, and the declared edge
clears nothing while reporting success. Use the `_in` variants for any fragment
you want a repository write to invalidate; the version token keeps working
exactly as before, and the namespace is the *additional* handle. Namespace the
id like a module path so it cannot collide with a `#[cached]` function's. `kind` is `Cached`, `Fragment` or `ReadThrough`; each model is a
path (not an arbitrary type — `Vec<Post>` is rejected, because a wrapper's name
is not a model's); a trailing `acknowledge_stale = "…"` opts the read out, with
the same non-empty-reason rule the attributes enforce.

A call site that does **not** declare itself is invisible to the gate. A clean
audit says nothing about it, which is why `undeclared_cache_call_sites` is named
in the manifest's `excluded` list.

**Declaring a read does not make it invalidatable.** `#[cached]` registers its
own store, so `invalidate_namespace` reaches it. A manually declared read over a
cache *you* own — `cache_fragment(Some(&my_cache), …)`,
`get_or_compute(&my_cache, …)` — is not registered anywhere, so
`invalidate_namespace(id)` clears nothing and still returns `true`: "no store
registered" is indistinguishable from "the function has not run yet". Register
it once, at startup, to close that:

```rust
autumn_web::cache::coherence::register_namespace_store(
    "blog::routes::sidebar_fragment",
    my_cache.clone(),
);
```

It returns the namespace's fill epoch. Sample it before computing a value, then
insert through `with_fill_fence` — `#[cached]` does this for you — or a fill
already in flight when an invalidation lands will write its stale value back
afterwards. For a shared backend, also read `Cache::fill_epoch` and store with
`insert_cached_fenced`. See "The fill fence" in the `invalidate_namespace`
rustdoc.

```rust
let sampled = epoch.load(std::sync::atomic::Ordering::Acquire);
let value = expensive();
autumn_web::cache::coherence::with_fill_fence(&epoch, sampled, || {
    my_cache.insert(&key, &value, ttl);
});
```

`namespace_epoch(id)` gets that counter for a namespace you did not register a
store against — a call site writing into the process-global backend, which is
what `cache_fragment_in` does internally.

`with_fill_fence` re-checks the epoch and runs the insert as one indivisible
step, and `invalidate_namespace` bumps the epoch under the same fence. Checking
the epoch yourself and inserting afterwards is not equivalent: an invalidation
can bump and clear between the check and the insert, and the value then lands
*after* the clear — stale until its TTL, or forever without one.

Because a manual declaration has no `#[cached]` function to hang an identity
constant on, `invalidates(...)` cannot name it; today such a read is covered by
`acknowledge_stale`, or by keeping it out of the mutated model's blast radius.
Invalidate it at runtime with
`autumn_web::cache::coherence::invalidate_namespace(id)`, which takes the same
id string.

## Interaction with `#[query_budget]`

A cached read that takes a repository handle is, to
[`#[query_budget]`](query-budgets.md), a free function handed the handle — and
that analysis conservatively calls such a function `unbounded`, because what it
does with the handle is another function's business. So a handler that calls one
loses its budget:

```rust
#[query_budget(2)]
#[get("/dashboard")]
async fn dashboard(repo: PgProjectRepository) -> AutumnResult<Response> {
    let total = cached_project_count(tenant_id, &repo).await?;   // ← unbounded
    …
}
```

That is correct as far as the budget analysis can see: on a cache miss the call
really does issue a query. Use the escape hatch the budget already provides —
`#[query_cost(1)]` on the statement — which is both honest (a miss costs one
round trip, a hit costs none) and keeps the budget on the rest of the handler,
rather than reaching for `#[query_budget(unbounded, …)]` and losing the gate
entirely.

## Asserting coherence from your own tests

The audit is available as a library call, so an app can gate on it without the
CLI:

```rust
#[test]
fn the_app_is_provably_cache_coherent() {
    let manifest = autumn_web::cache::coherence::audit();
    assert!(
        manifest.violations.is_empty(),
        "{}",
        autumn_web::cache::coherence::format_diagnostic(&manifest.violations)
    );
}
```

`examples/saas/tests/cache_coherence.rs` is the worked version, and it proves
both halves: the app audits clean **and** the gate fires the moment the
invalidation edge is removed. A green result that cannot be made red is not
evidence of anything.

## The manifest

```console
$ autumn cache audit --manifest target/cache-coherence.json --json
```

```json
{
  "schema_version": 1,
  "dimensions": {
    "cached_reads": {
      "provenance": "provable",
      "source": "macro:#[cached] / declare_cached_read!",
      "runtime_caveat": "each entry's EXISTENCE is proven … but its dependency set is only as strong as the entry's own `provenance` field says …",
      "entries": [
        {
          "id": "saas::repositories::cached_project_count",
          "kind": "cached",
          "reads": ["saas::models::Project"],
          "provenance": "declared",
          "location": "examples/saas/src/repositories.rs:50"
        }
      ]
    },
    "mutations": {
      "provenance": "provable",
      "source": "macro:#[repository]",
      "runtime_caveat": "every entry is proven, but the SET is not exhaustive: only `#[repository]` write methods are here …",
      "entries": [
        {
          "name": "PasswordResetTokenRepository::delete_by_id",
          "model": "saas::models::PasswordResetToken",
          "table": "password_reset_tokens",
          "location": "examples/saas/src/repositories.rs:71"
        }
      ]
    },
    "invalidations": {
      "provenance": "provable",
      "source": "macro:#[repository(..., invalidates(...))]",
      "runtime_caveat": "the edge's target is proven … Each generated write method calls the invalidator after its own transaction commits … Not proven: that the backend sweep succeeds …",
      "entries": [
        {
          "mutation": "ProjectRepository::delete_by_id",
          "read": "saas::repositories::cached_project_count"
        }
      ]
    }
  },
  "violations": [],
  "undetermined_reads": [],
  "excluded": [ … ]
}
```

(Abridged: the real document lists all 16 mutations and the full caveat text.)

### Two provenance vocabularies, deliberately

The **dimension** level uses the same three classes as the
[security posture manifest](security-posture-manifest.md) — `provable`,
`declared`, `runtime-only` — with the same meanings and the same rubric. All
three dimensions here are `provable`: each is recovered from macro-expanded code
with no config read and no process started. Where a dimension has an adjacent
weak step it carries a `runtime_caveat` rather than being demoted, which is that
rubric's stated tie-breaker — the invalidation edge is genuinely proven, and
what is *not* proven is that the backend sweep succeeds.

The **entry** level of `cached_reads` uses a second, narrower vocabulary —
`declared` / `derived` / `undetermined` — describing how one read's dependency
set was established. It shares the `declared` spelling with the dimension
classes and means something different; read them as two scopes, not one scale.

`excluded` names what this slice deliberately does not look at, so a reader can
tell "we checked and it was fine" from "we never looked".

## What this does not prove

* **That the invalidation succeeds.** Each generated write calls the
  invalidator after it commits, so the call is no longer up to you. The build
  does not prove that the backend sweep succeeds. A failure is retried, then
  logged and counted in `autumn_cache_invalidation_failures_total`. Alert on
  that counter.
* **Complete invalidation under an opaque backend.** Namespace invalidation
  clears every store registered for the read *and* asks the registered backend
  to drop the namespace — `MokaCache` by iteration, `RedisCache` by a `SCAN
  MATCH` one segment narrower than its `clear`. A backend that cannot
  pattern-match its key space, or whose sweep errored, returns `false` from
  `Cache::invalidate_namespace`, and `invalidate_declared_caches()` reports that
  `false` verbatim rather than letting you believe the value is gone. It is
  `#[must_use]` for exactly that reason.
* **A fill that reads a lagging read replica.** A fill that starts after the
  invalidation reads the database again. If that read goes to a replica that
  has not applied the write, the old row is cached with a current epoch.
* **A fill in flight on another replica.** With `RedisCache`, the shared epoch
  fences it out, and a `true` is valid for all replicas. With a custom backend
  that has no `fill_epoch`, the fence is per process. Another replica's fill
  that started before the invalidation can then write a stale value back, and a
  `true` is valid for this process only. The shared fence covers `#[cached]` and
  `cache_fragment_in`. It does not cover `get_or_compute`, `Cache::clear`, or a
  key-level `invalidate`.
* **Loss of the epoch key.** Redis must keep the epoch keys
  (`{prefix}:__autumn_epoch__:*`). Eviction, `FLUSHDB`, or a failover to a replica that
  missed the bump can reset one to 0. A stale fill that sampled 0 can then
  pass. Do not let an eviction policy remove these keys.
* **A snapshot taken before the call.** The shared epoch is read after the
  miss. If the caller read its data in an earlier transaction or snapshot, an
  invalidation between that read and the miss is not fenced.
* **Which writes exist.** Only `#[repository]` write methods are in the mutated
  set. A hand-rolled repository, a raw diesel `update`/`insert`/`delete`, a
  migration, or a job that writes directly is invisible to the gate — so a
  cached read those dirty audits clean. The manifest names this under
  `excluded`, along with `CacheResponseLayer`, whose entries are keyed by URI
  with no annotated item to derive a dependency set from.
* **Cascades and counters declared on the *model*.** A
  `#[repository(..., dependent(...))]` cascade **is** covered: it registers a
  write against the child model it deletes or nullifies, resolved from that
  child repository's own published model rather than guessed from its type name
  (`on_delete = restrict` is excluded — it probes and never writes). But
  `#[has_many(Child, dependent = destroy)]` and
  `#[belongs_to(Parent, counter_cache)]` are declared on the model, which
  `#[repository]` cannot see, so the writes they cause carry no descriptor.
  Declare such a cascade on the repository to bring it into the graph.
* **That a declared dependency set is true.** `reads(...)` replaces the analysis
  rather than being checked against it, so naming the wrong model audits clean.
  A `derived` set is an approximation of a different kind — see
  [Derivation](#derivation--the-fallback).
* **Row- or column-level precision.** Granularity is the model/table. A read
  derived from one column of one row is treated as depending on the whole model,
  which over-approximates — a false *failure*, never a false pass.
* **Cross-service coherence.** Another service's writes are not in this binary's
  dependency graph.
* **TTL semantics.** A TTL bounds how long a value stays stale; it does not stop
  it becoming stale. Orthogonal, by design.

## Model identity, and one way it over-approximates

The two sides learn a model's name differently. A `#[repository]` always has the
model type in scope and publishes `core::any::type_name` (`blog::models::Post`);
so does `reads(...)`. But a dependency the macro *derived* from a function body
may only be the bare ident (`Post`), because the model type is often not in
scope at the cached function at all — a `PgPostRepository` parameter does not
bring `Post` with it.

So matching compares **full paths when both sides have one**, which keeps
`plugin::models::User` and `crate::models::User` apart, and falls back to the
bare type name only when one side is all the analysis could recover. In that
fallback a same-named model in another module collides and the gate
over-approximates — the safe direction, a false failure rather than a false
pass. Declaring `reads(crate::models::User)` resolves it, because a declared set
is always fully qualified.

## Reference

### `#[cached]`

| Attribute | Meaning |
|-----------|---------|
| `key(a, b)` | build the cache key from these parameters only — this is what lets a cached read take the repository handle it reads through. **Every parameter you leave out must not be able to change the result**, or the cache will serve one parameter's answer for another's; the gate does not check this |
| `reads(Model, …)` | declared dependency set |
| `acknowledge_stale = "…"` | opt out of the gate, with a mandatory reason |

`#[cached]` still works wherever it did before, associated functions included:
the coherence registration is placed inside the function body precisely so an
`impl` block can hold it.

### `#[repository]`

| Attribute | Meaning |
|-----------|---------|
| `invalidates(path, …)` | edges for every write method |
| `acknowledge_stale = "…"` | opt every write out, with a mandatory reason |
| `#[invalidates(path, …)]` on a method | edges for that method, in addition to the trait-level ones |
| `#[acknowledge_stale(reason = "…")]` on a method | opt that method out |

### `autumn cache audit`

| Flag | Meaning |
|------|---------|
| `-p, --package` | package to build and run |
| `--bin` | binary target, for multi-bin packages |
| `--manifest PATH` | write the JSON manifest to a file |
| `--json` | emit the manifest to stdout instead of the human report |
| `--strict` | also fail on `undetermined` reads |
| `--features F` | Cargo features to build the audited binary with (repeatable) |
| `--all-features` | build the audited binary with every feature |
| `--no-default-features` | build the audited binary without default features |
| `--release` | build and audit the release binary (`target/release`) |
| `--profile NAME` | build and audit under a Cargo profile (`target/NAME`; `dev`/`test` use `target/debug`, `bench` `target/release`) |

**Audit the feature set you deploy.** The manifest describes the binary that
produced it. A `#[cached]` read or a `#[repository]` write behind a feature the
build does not enable is not compiled in at all, so it cannot appear in the
manifest and cannot be found incoherent — a default-feature audit exits green
on a configuration it never looked at. Pass the same feature flags your release
build uses; the audit echoes them when they are not Cargo's defaults, so the
report says which build it is talking about. The same holds for the profile: a
read behind `#[cfg(not(debug_assertions))]` exists only in a release build, so
pass `--release` (or `--profile NAME`) to audit the profile you ship.

Exit code is `0` when nothing can be left stale, `1` otherwise.
