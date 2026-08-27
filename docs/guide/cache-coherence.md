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
$ autumn cache audit
🍂 autumn cache audit

4 cached reads (3 declared, 1 derived, 0 undetermined), 8 repository mutations, 0 acknowledged-stale

error: 1 cached read can be left stale by a repository write

  ProjectRepository::save mutates saas::models::Project
    but the cached read saas::repositories::cached_project_count is derived from it
    and is never invalidated.
      read     examples/saas/src/repositories.rs:41
      mutation examples/saas/src/repositories.rs:33
    fix: add #[invalidates(saas::repositories::cached_project_count)] to the write, or
         acknowledge the staleness with #[acknowledge_stale(reason = "…")].

$ echo $?
1
```

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

A cached read is identified by `module_path!() :: <fn name>` — the exact string
`make_cache_key` already prefixes every one of its keys with. The manifest's
identity and the runtime key space are the same string, by construction.

### Invalidation edges are checked by the compiler

`#[cached]` emits an identity constant beside the function it wraps.
`invalidates(crate::views::recent_posts)` rewrites the last path segment to that
constant, so the edge only compiles when it names a real `#[cached]` function
that is actually in scope:

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

### Derivation — the fallback

Omit `reads(...)` and the macro recovers what it can from the function's own
signature and body:

* a repository type anywhere in scope — `PgPostRepository`, `impl PostRepository`,
  a parameter, a turbofish — names `Post`;
* a reading associated call — `Post::find_all(db)`, `Post::find_by_id(…)` — names
  `Post`.

The manifest tags these `derived`. Derivation is sound for what it finds and
**silent about what it cannot see**: a dependency reached through a helper
function the analysis cannot read is missed. That is why a read it recovers
nothing from is not treated as having no dependencies.

### `undetermined` — reported, never failed

A cached read whose dependency set could not be established is recorded as
`undetermined`. The default gate does not fail on it, because a checker that
fails on what it merely could not read is a checker that gets deleted from CI.
It is reported in the manifest and in the audit's own output, and
`autumn cache audit --strict` turns it into a failure for apps that want the
stronger posture.

Read the summary line before you trust a green build:

```
12 cached reads (11 declared, 1 derived, 0 undetermined), 30 repository mutations, 0 acknowledged-stale
```

A green audit over mostly-`undetermined` reads proves very little, and says so.

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

A repository that declares any edge also gets a generated invalidator:

```rust
repo.save(&new).await?;
PgPostRepository::invalidate_declared_caches();
```

It returns whether every declared read was invalidated **completely** — see
[What this does not prove](#what-this-does-not-prove).

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
error. Every acknowledgement is an entry in the manifest, so an escape hatch is
always visible in review.

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

`id` must be the namespace the call site passes to the cache, so an invalidation
edge and the runtime key agree. `kind` is `Cached`, `Fragment` or `ReadThrough`.
A trailing `acknowledge_stale = "…"` opts the read out.

Because a manual declaration has no `#[cached]` function to hang an identity
constant on, `invalidates(...)` cannot name it; today such a read is covered by
`acknowledge_stale`, or by keeping it out of the mutated model's blast radius.

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
      "entries": [
        {
          "id": "saas::repositories::cached_project_count",
          "kind": "cached",
          "reads": ["saas::models::Project"],
          "provenance": "declared",
          "location": "examples/saas/src/repositories.rs:41"
        }
      ]
    },
    "mutations": {
      "provenance": "provable",
      "source": "macro:#[repository]",
      "entries": [
        {
          "name": "ProjectRepository::save",
          "model": "saas::models::Project",
          "table": "projects",
          "location": "examples/saas/src/repositories.rs:33"
        }
      ]
    },
    "invalidations": {
      "provenance": "declared",
      "source": "macro:#[repository(..., invalidates(...))]",
      "runtime_caveat": "the edge's target is proven … that the invalidator is actually CALLED on the write path is not proven by this slice",
      "entries": [
        { "mutation": "ProjectRepository::save", "read": "saas::repositories::cached_project_count" }
      ]
    }
  },
  "violations": [],
  "undetermined_reads": [],
  "excluded": [ … ]
}
```

Dimensions carry a **provenance class** with the same meaning as in the
[security posture manifest](security-posture-manifest.md): `provable` is
recovered from macro-expanded code alone, `declared` is something you wrote down
that the runtime then has to honour. `excluded` names what this slice
deliberately does not look at, so a reader can tell "we checked and it was fine"
from "we never looked".

## What this does not prove

* **That the invalidator runs.** The build proves the edge exists and that it
  names a real cached read. Whether `invalidate_declared_caches()` is actually
  called on the write path is the `invalidations` dimension's `runtime_caveat`;
  wiring it automatically through repository commit hooks is the next slice.
* **Complete invalidation under a shared backend.** A per-function Moka store
  holds only that function's entries, so clearing it *is* namespace
  invalidation. A process-level shared backend (`set_global_cache`, e.g. Redis)
  keys every cached function into one store with no way to enumerate a
  namespace, so `invalidate_namespace` returns `false` there — the honest signal
  that a backend-specific mechanism is needed.
* **Row- or column-level precision.** Granularity is the model/table. A read
  derived from one column of one row is treated as depending on the whole model,
  which over-approximates — a false *failure*, never a false pass.
* **Cross-service coherence.** Another service's writes are not in this binary's
  dependency graph.
* **TTL semantics.** A TTL bounds how long a value stays stale; it does not stop
  it becoming stale. Orthogonal, by design.

## Model identity, and one way it over-approximates

Models are matched on their **last path segment**, because the two sides learn
the name differently: a `#[repository]` always has the model type in scope and
publishes `core::any::type_name` (`blog::models::Post`), while a dependency
recovered from a `#[cached]` body may only be the bare ident (`Post`).

Two same-named models in different modules therefore collide, and the gate
over-approximates. That is the safe direction — a false failure, never a false
pass — and `reads(...)` with a fully-qualified path plus `acknowledge_stale` are
the release valves.

## Reference

### `#[cached]`

| Attribute | Meaning |
|-----------|---------|
| `key(a, b)` | build the cache key from these parameters only — this is what lets a cached read take the repository handle it reads through |
| `reads(Model, …)` | declared dependency set |
| `acknowledge_stale = "…"` | opt out of the gate, with a mandatory reason |

Apply `#[cached]` to a **free function**: the expansion emits items beside it,
which an `impl` block cannot hold.

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

Exit code is `0` when nothing can be left stale, `1` otherwise.
