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

## Update: votable (#1362)

Added `#[votable(by = <Reactor>, aggregate = sum|count)]` as a third
association kind on `#[model]` — a `(reactor, target)`-unique edge table plus
an aggregate column (`score` / `{name}_count`) maintained on the target. It
reuses this ADR's machinery rather than introducing a parallel one: the edge
table is a hidden per-association `diesel::table!` (the m2m pattern), and the
generated `{Model}Reactions` trait (`react` / `reaction_of`) is
blanket-implemented over the same `M2mConnSource<Model = M>` bound the m2m
mutation traits use, so `#[repository]` needed no change at all.

**Design space for the write path, and why the pessimistic lock won.** The
acceptance criteria asked for "a single upsert/delete, no read-then-write
window". We deliberately implemented something different — and stronger — and
record the alternatives here:

- **Lock-free upsert + separate aggregate recompute** (the closest reading of
  the literal AC, and what reddit-clone did by hand). *Rejected.* It fixes the
  same-user double-click and leaves the far more dangerous bug untouched: two
  **different** reactors on the same target each run `SELECT SUM(value)`
  against a snapshot that excludes the other's uncommitted edge, then both
  write the same score. The persisted aggregate is permanently off by one vote,
  in the ordinary production workload, with no error anywhere.
- **Delta arithmetic (`score = score + :delta`) from an upsert's `RETURNING`.**
  *Rejected.* The delta needs the *old* value, which Postgres cannot return
  from an upsert before PG 18's `RETURNING OLD.*`. Smuggling it via an extra
  `prev_value` column would force a schema change and break the "works on the
  table you already have" property. Accumulated aggregates also preserve any
  historical drift forever, where a recompute is self-healing.
- **A single data-modifying CTE** (`WITH del AS (DELETE … RETURNING) INSERT …
  ON CONFLICT DO UPDATE`). *Rejected.* It reads as an elegant one-statement
  answer and is wrong: the `INSERT`'s unique-index arbiter runs against the
  statement snapshot in which the just-deleted row is still live, so the insert
  conflicts with it and `DO UPDATE` fails with `tuple to be updated was already
  modified`. Data-modifying CTEs are also unsupported on SQLite.
- **`xmax = 0` inserted/updated discrimination.** *Rejected* on the same
  grounds as folklore in general: it is a documented-nowhere implementation
  detail that reviewers cannot check and that has no SQLite analogue.
- **Per-edge `SELECT … FOR UPDATE`.** *Rejected.* `FOR UPDATE` on zero rows
  locks nothing, so the insert race — the one the issue cares about — is
  exactly the case it fails to cover.
- **Advisory locks / `SERIALIZABLE` + retry.** *Rejected* as heavier failure
  surfaces (hash collisions serialising unrelated pairs; a retry policy and
  latency tail) for no additional correctness over a row lock.
- **A database trigger or generated column.** *Rejected* — SQL the user must
  write and maintain is the opposite of a declarative Rust attribute, and it
  diverges per backend.
- **Chosen: a pessimistic lock on the *target row*** (`SELECT id FROM targets
  WHERE id = $t FOR NO KEY UPDATE` on Postgres; `BEGIN IMMEDIATE`'s
  database-wide write lock on SQLite), taken *before* the edge is read and held
  to commit, with the edge read/branch/write and the `SUM`/`COUNT` recompute
  and the aggregate `UPDATE` all inside it. The literal AC ("single
  upsert/delete") is not met; the property it exists to buy — the decision
  cannot be invalidated between reading and acting on it — is met and extended
  to cover the aggregate. The correctness argument is one paragraph a reviewer
  can verify (mutual exclusion per target ⇒ equivalent to some serial
  execution ⇒ prove the invariant serially), rather than snapshot lawyering. It
  costs per-target write serialisation and seven round trips per call
  (`BEGIN`, S1-S5, `COMMIT`), both documented as known limits.
  **`FOR NO KEY UPDATE`, not `FOR UPDATE`:** the two are equally exclusive
  against each other, so the mutual-exclusion argument is unchanged, but
  `FOR UPDATE` also conflicts with the `FOR KEY SHARE` lock a referencing
  insert takes — under it, inserting a comment on a post would queue behind
  every vote on that post. `react()` only writes a non-key column, so it takes
  the weaker mode. The target row is the row we must exclusively
  lock for the `UPDATE` anyway, so the design extends the existing critical
  section backwards over three short statements — it changes the constant, not
  the asymptotics. The `ON CONFLICT (reactor_fk, target_fk) DO UPDATE` is still
  emitted on the insert branch (unreachable under the lock) so a lock-bypassing
  writer produces an idempotent update rather than a `23505`.

**Deviation from the issue: no commit hooks.** AC3 asked for the aggregate to
be recomputed "atomically in the same transaction … (reusing
`repository_commit_hooks`)". Those two clauses are mutually exclusive.
`repository_commit_hooks` is a durable *post-commit* queue: only the enqueue is
in-transaction, and hook bodies run later on a different connection with
retries and a dead-letter path. A hook therefore cannot be atomic with the edge
mutation, and the window between commit and hook execution is precisely the
edge/aggregate disagreement the normative half of AC3 forbids. We honoured the
normative clause with an in-transaction `UPDATE` and treated the parenthetical
as a non-binding implementation suggestion. `react()` enqueues no hook. An
`after_react` hook — for SSE fan-out, notifications, moderation — *is* the
correct use of the durable queue and is recorded as a follow-up; it would
replace reddit-clone's current fire-and-forget `publish_oob`.

**Count-mode arity.** `aggregate = count` emits `react(reactor_id, target_id)`
with **no** `value` parameter, so the two modes have different arities. The
alternative considered was a uniform `react(reactor_id, target_id, value)` in
both modes with a runtime check that count-mode callers pass `1`. Rejected: a
count edge table has no value column (its rows are pure membership, exactly
like an m2m join row), so a `value` parameter would be a meaningless argument
validated at run time instead of eliminated at compile time. `reaction_of`
*does* keep a uniform `Option<i16>` return in both modes (count mode yields
`Some(1)`), which is what keeps view and widget code mode-independent — the
asymmetry is deliberate and confined to the write side.

**Nullable target FKs are tolerated.** The generated hidden `table!` declares
the edge's target FK non-nullable, so nothing `react()` writes can be `NULL`,
but the *DDL* column may be nullable — which it already is in reddit-clone,
whose `votes` table is an XOR over `post_id` / `comment_id`. A Postgres unique
constraint treats `NULL`s as distinct, so `UNIQUE (user_id, post_id)`
constrains exactly the rows this association writes and ignores the comment
votes. We chose to document and test that coexistence
(`react_is_exact_when_the_edge_table_has_a_nullable_target_fk`) rather than
require `NOT NULL`, which would have made "works on the table you already
have" false for the flagship example.

**Scope cuts recorded as follow-ups:** at most one `#[votable]` per model
(a second is a directed compile error, since `{Model}Reactions` / `react` /
`reaction_of` would be ambiguous); no batch `reaction_of_many`, so feed pages
render un-highlighted controls rather than an N+1; no `aggregate = sum(delta)`
fast mode for very high-cardinality targets; and `M2mConnSource` keeps its
m2m-specific name pending a mechanical `AssocConnSource` rename.

See `docs/guide/votable.md` for the user-facing treatment,
`autumn-macros/src/lib.rs`'s `#[model]` doc comment for the attribute grammar
and required migration, and `examples/reddit-clone` (`Post` ← `User` via
`votes`) for the reference implementation — including the conversion that
deleted the example's hand-written vote SQL.
