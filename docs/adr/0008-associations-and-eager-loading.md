# ADR 0008: Declarative Associations and Eager Loading for `#[model]` / `#[repository]`

- Status: Accepted
- Date: 2026-06-14
- Deciders: Autumn maintainers
- Tags: database, macros, repository, performance, ergonomics

## Context

Autumn ships `#[model]` and `#[repository]` for typed Postgres access via
diesel-async, but offers no first-class way to declare or batch-load record
relationships. Every content-heavy example reaches for raw diesel
`inner_join`s and per-row foreign-key fetches: `reddit-clone`'s post view
hand-wrote a join across posts/users/subreddits and re-fetched the author
per post, and the single-post view assembled comments and their authors by
hand. The v0.1 PRD explicitly punted on relations (open question #4), and
`docs/stories/S-018.md` deferred `belongs_to`/`has_many` out of v0.1.

Once the dev-mode N+1 detector (#701) lands, developers *see* the problem but
have no idiomatic fix. Without an autumn-shaped answer, downstream apps either
copy join boilerplate forever or learn the hard way that Postgres p99
collapses at scale.

### Prior art

- **Rails / ActiveRecord** `includes(:author, comments: :author)` emits
  batched `IN` queries — ergonomic, but couples the model to a query DSL and
  keeps the implicit-lazy-load footgun (`post.comments` silently fires SQL).
- **Phoenix / Ecto** `preload: [:author, comments: :author]` — explicit, no
  auto-fetch, batched. The closest spiritual match to autumn's "make work
  visible" stance.
- **Django** distinguishes `select_related` (JOIN) from `prefetch_related`
  (extra query); the Pythonic flavor doesn't map cleanly onto typed Rust.
- **SeaORM / Loco.rs** generate per-pair `Related` impls; users routinely
  complain about that macro surface.

## Decision

Add declarative associations to `#[model]` and an explicit, batched `preload`
to `#[repository]`. The shape is Phoenix-flavored: explicit preload by name,
no implicit lazy loading, and a typed `NotLoaded` sentinel when an
un-preloaded association is accessed.

### 1. Declaring associations

Associations are struct-level attributes on a `#[model]`:

```rust
#[autumn_web::model]
#[belongs_to(User, fk = author_id)]
#[belongs_to(Subreddit)]      // fk inferred: subreddit_id
#[has_many(Comment)]          // fk inferred on Comment: post_id
pub struct Post { /* ... */ }
```

Foreign keys are inferable by convention and overridable with `fk = …`:

- `belongs_to(Target)` — fk on *this* model, default `{target_snake}_id`;
  accessor name is the fk minus `_id` (`author_id` → `author`).
- `has_many(Target)` / `has_one(Target)` — fk on the *target*, default
  `{source_snake}_id`; accessor names are `{target_snake}s` / `{target_snake}`.

The target's table name follows the same inference as `#[model]`
(`snake_case` + `s`). Targets with a custom `#[model(table = …)]` are not yet
supported as association targets (see Consequences).

### 2. What codegen emits

For each model, `#[model]` generates:

- A `{Model}Preload` **spec builder** — one optional, boxed nested spec per
  association, with a fluent `name()` / `name_with(nested_spec)` per
  association, plus `Model::preload()`.
- A `{Model}Associations` **accessor trait**, implemented for
  `Preloaded<{Model}>`:
  - `belongs_to`/`has_one` → `Result<Option<&Preloaded<Target>>, NotLoaded>`
    (`Ok(None)` = preloaded but no matching row; `Err` = not preloaded).
  - `has_many` → `Result<&[Preloaded<Target>], NotLoaded>`.
- An `impl Preloadable for {Model}` whose `load_associations` issues the
  batched queries and recurses into nested specs.

No per-pair `Related` impl is required — the schema and the association set
live in one place, on the model.

`#[repository]` gains a `preload(records, spec)` method returning
`Vec<Preloaded<Model>>`.

### 3. Storage and the wrapper type

A `Preloaded<T>` wraps a record plus a type-erased `Associations` store and
`Deref`s to `T`, so field access keeps working and generated accessors add the
relations. `belongs_to`/`has_one` store `Option<Arc<Preloaded<Target>>>` —
`Arc` because many parents can share one related record, and cloning the Arc
into each parent is cheap and avoids deep clones. `has_many` stores an owned
`Vec<Preloaded<Target>>` (each child belongs to exactly one parent, so no
sharing is needed).

### 4. Batching contract

`load_associations` issues **at most one SQL statement per association per
level**:

- `belongs_to`/`has_one`: collect the (deduplicated) keys and issue one
  `WHERE id IN (...)` (belongs_to) or `WHERE fk IN (...)` (has_one).
- `has_many`: one `WHERE fk IN (...)`, then group the rows by fk client-side.

Nested specs recurse on the *flat* set of already-loaded children, so
`posts.preload(comments.author)` is `comments` (1) + `comments.author` (1),
never one author query per comment. There is **no implicit lazy loading**:
accessing an un-preloaded association returns `NotLoaded` rather than issuing
SQL.

### 5. Interaction with primary/replica topology

`#[repository]::preload` acquires its connection via the same
`__autumn_acquire_read_conn()` used by every generated read finder, so preload
SQL runs against the **same role** the parent query would use: replica when a
healthy replica is configured, primary otherwise, or a `503` under the
`FailReadiness` fallback policy. `repo.on_primary().preload(...)` pins the
whole chain — finder and preload — to the primary for read-your-writes.

All statements for a single `preload` call run on **one** pooled connection,
so a preload never splits across roles mid-flight.

### 6. Interaction with `CursorPage`

`preload` runs **after** the overfetch. A cursor finder overfetches
`size + 1` rows to compute `has_next`, truncates to `size`, and only then are
the surviving records wrapped and preloaded — so the dropped overfetch row
never triggers association queries, and the preload key set matches the page
exactly.

## Consequences

- **New public API → ships in the next minor**: `Preloaded`, `NotLoaded`, the
  `preload` module, `impl_preloadable_leaf!`, generated `{Model}Preload` /
  `{Model}Associations` types, and the repository `preload` method are all
  additive. They are recorded under `## [Unreleased]` in `CHANGELOG.md`; per
  repo convention the workspace version is bumped (to the next minor) only when
  a release is cut, and the SemVer gate enforces a minor bump for these new
  public items. No existing `autumn-web` surface changes.
- **Manual models as targets**: a hand-written model (e.g. `reddit-clone`'s
  `User`, kept manual so `password_hash` is never auto-serialized) that is the
  *target* of an association must implement `Preloadable`. The
  `autumn_web::impl_preloadable_leaf!(User)` macro provides a one-line leaf
  impl (loads nothing of its own, so it can be wrapped/preloaded but not
  nested into).
- **Disambiguating associations**: multiple associations to the same target
  are supported via `name = …` (e.g. `#[has_many(Post, fk = author_id, name =
  authored)]` and `#[has_many(Post, fk = approver_id, name = approved)]`),
  which overrides the derived accessor/store name.
- **Target read scoping (tenant isolation + soft-delete) is enforced, keyed
  off the target's *repository* config**: a preload hides the same rows the
  target's repository finders do. Each `#[model]` generates a
  `__autumn_preload_retain` helper that the loader calls on freshly loaded
  target rows; filtering is **in-memory** after the batched `IN` load (the
  loading model can't add a typed `.filter()` on columns it can't name). The
  decision to scope is **not** inferred from field presence — a model may have
  a `deleted_at` (audit/history) or `tenant_id` column without its repository
  opting into `soft_delete` / `tenant_scoped`, and in that case finders don't
  filter, so neither does preload. Instead:
  - The retain is generated behind a *compile-time* column guard (it only
    references `deleted_at` / `tenant_id` when the field exists) but gated at
    *runtime* on the target repository's config, surfaced via the default-
    `false` `AutumnPreloadScopeExt` trait whose
    `__autumn_repo_soft_delete_scope` / `__autumn_repo_tenant_scope` the
    `#[repository(..., soft_delete, tenant_scoped)]` macro overrides with
    inherent fns (inherent wins over the blanket default).
  - Tenant scoping additionally honors `across_tenants()`: a repository's
    `preload` publishes its cross-tenant choice as the ambient
    `PRELOAD_ACROSS_TENANTS` task-local, which the retain reads (and which
    propagates through nested levels), so `repo.across_tenants().preload(...)`
    skips the tenant predicate on every level — matching finders for
    admin/reporting.
  Net effect: a cross-tenant `belongs_to` parent reads back as `Ok(None)` and
  cross-tenant / soft-deleted `has_many` children are excluded — but only when
  the target's repository actually scopes that way. Hand-written models that
  are association targets get an identity retain via `impl_preloadable_leaf!`
  (no auto-scoping); models with no scoping config are unaffected.
- **No per-association *custom* filtering (follow-up)**: beyond the target's
  own tenant/soft-delete scoping, preload loads *all* matching rows of an
  association keyed on the foreign key — there is no scoped/filtered preload
  for arbitrary predicates (e.g. "only top-level comments"). Callers needing
  that filter client-side after preloading or keep a hand-written scoped query.
- **Scope / limitations** (deliberately out of scope for this slice):
  polymorphic associations, write-side cascades, cross-database/shard
  preloading, and ORM-style implicit lazy loading. Keys are assumed `i64` and
  primary keys named `id`, matching the rest of the repository layer;
  association targets must use the inferred table name. Nullable foreign
  keys and custom-table targets are follow-ups. `has_and_belongs_to_many` /
  join tables shipped as a `through =` follow-up — see "Update: many-to-many
  (#1324)" below.

## Success metrics (reddit-clone, before/after)

- Single-post page with 50 comments: from `2 + N` round trips to `≤ 4`
  (post, post.author, comments, comments.author).
- List view: `1 + K` for a `K`-association index, independent of result-set
  size — asserted by `tests/preload_pg_integration.rs`
  (`preload_is_batched_no_n_plus_one`), which proves the statement count for a
  2-comment post equals that for a 40-comment post.

## Update: many-to-many (#1324)

Extended `#[has_many]` with `through = <join_table>` — a HABTM-style join
covering the pure association (two FK columns, no join-row payload), reusing
this ADR's `Preloadable`/`{Model}Preload`/`NotLoaded` machinery rather than
introducing a parallel system:

- **Join table**: `#[model]` emits a hidden per-association `diesel::table!`
  for the join table (so two models can both declare `through =` the same
  physical table without colliding types) rather than requiring a
  hand-written `schema.rs` entry. Join columns default to `{source}_id` /
  `{target}_id`, overridable with `fk = ...` / `target_fk = ...`; the join
  table needs a composite primary key on both columns.
- **Preload**: one batched `INNER JOIN` per association level, same "no N+1"
  contract as `belongs_to`/`has_many`/`has_one`. Unlike plain `has_many`,
  the *same* target row can legitimately belong to more than one
  currently-loaded parent (that's the point of many-to-many), so
  `#[has_many(..., through = ...)]`'s stored/accessor type is
  `Vec<Arc<Preloaded<Target>>>` rather than `has_many`'s owned
  `Vec<Preloaded<Target>>` — targets are deduplicated by id before recursing
  into nested `_with` specs, then shared via `Arc::clone` across every
  linking parent (mirroring how `belongs_to`/`has_one` already share a
  parent-side target). Getting this wrong silently drops nested associations
  for every parent but the first one sharing a target — covered by
  `m2m_nested_through_path` in `examples/reddit-clone/tests/m2m_pg_integration.rs`.
- **Mutations**: each `through =` association generates a small mutation
  trait (`add_{singular}` / `remove_{singular}` / `set_{plural}`) blanket-
  implemented for any repository whose new `M2mConnSource::Model` associated
  type matches — `#[repository(Model, ...)]` implements `M2mConnSource`
  unconditionally, and the `Model` bound is what keeps method resolution
  unambiguous when a model has more than one `through =` association, or two
  models' mutation traits are both in scope. `add_*` is idempotent via
  `ON CONFLICT DO NOTHING` on the join table's composite key; `set_*` wraps
  delete-then-insert in one transaction.
- **Scoping**: preload reads apply the target's existing tenant/soft-delete
  scoping via a new per-row `__autumn_preload_keep` (the row-at-a-time
  sibling of the batch `__autumn_preload_retain` this ADR introduced —
  needed because m2m loaders pair each row with its parent key before
  grouping, so a `Vec::retain` would lose that pairing). Mutation helpers
  write the join table directly and do **not** apply tenant scoping, hooks,
  or broadcasts on the join row — documented limitation, not a gap to close
  in this slice.

See `autumn-macros/src/lib.rs`'s `#[model]` doc comment for the full
`through =` syntax and `examples/reddit-clone` (`Post` ↔ `Tag` via
`post_tags`) for the reference implementation.
