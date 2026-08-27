# Prove cached reads never serve stale data at compile time (#1716)

Planning record for the first slice of build-time cache coherence.

## 1. The problem, restated

Autumn owns both ends of the staleness dependency:

* **Reads** flow through `#[cached]` (`autumn-macros/src/cached.rs`), the
  fragment cache (`autumn/src/cache/fragment.rs`) and the read-through cache
  (`autumn/src/cache/read_through.rs`).
* **Writes** flow through `#[repository]` (`autumn-macros/src/repository.rs`).

Nothing links the two. A `#[cached] fn recent_posts()` derived from `Post` rows
keeps serving the old list after `PgPostRepository::save` inserts a new one,
and the build says nothing. This slice makes the link explicit, machine
readable, and enforceable.

## 2. Brainstorming — candidate mechanisms

| # | Mechanism | Verdict |
|---|-----------|---------|
| 1 | Pure `compile_error!` inside `#[repository]` | **Rejected.** A proc macro sees one item; the check needs every cached read in the whole binary. |
| 2 | Macro-emitted `inventory` descriptors + a CLI gate that runs the built binary in dump mode | **Chosen.** Exactly the `#[secured]`→`autumn routes audit` shape proven by #1604, and `#[repository]` already submits `RepositoryFacts` this way. |
| 3 | Boot-time assertion in `AppBuilder::run` | **Partially adopted** — as a *library* entry point (`coherence::audit()`) an app can assert in its own test, not as an unconditional runtime panic. |
| 4 | `build.rs` that re-parses `src/**/*.rs` with `syn` | **Rejected.** Duplicates macro knowledge, breaks on re-exports and cross-crate models. |
| 5 | `cache_coherence![...]` companion collection, like `routes![]` | **Rejected as the primary** — real compile-time, but every read must be hand-listed; zero-wiring is the whole point. Its companion-function *naming trick* is reused (see §5). |
| 6 | Type-level dependency tokens (`Cached<Reads<(Post,)>>`) | **Rejected for slice 1.** Elegant, total API break. |
| 7 | Runtime table-touch tracking through the `Db` handle | **Out of scope** — the issue explicitly scopes to the dependency graph, not storage. |

## 3. Reverse brainstorming — how would this feature fail?

*"How do we guarantee nobody uses this gate?"* — and the mitigation for each.

1. **Cry wolf.** Over-approximate a dependency set, fail a correct app, get
   deleted from CI. → Only reads with a **known** dependency set (declared or
   derived) are gated. A read whose set could not be established is
   `undetermined`: reported loudly, never failed, unless `--strict`.
2. **Prove nothing.** Silently pass every read the analysis cannot read, so the
   green build means nothing. → Every entry carries a **provenance** tag
   (`declared` / `derived` / `undetermined`) and the summary counts them, the
   same discipline `docs/guide/security-posture-manifest.md` already enforces.
   A green audit that is 90% `undetermined` says so on the tin.
3. **Escape-hatch rot.** `acknowledge_stale` everywhere, no reasons. → The
   reason string is **required and must be non-empty** (compile error
   otherwise), and every acknowledgement is an entry in the manifest.
4. **A declared edge that invalidates nothing.** `#[invalidates(...)]` silences
   the gate without any code running. → The edge target is resolved by **rustc
   itself** (§5) so it must be a real `#[cached]` function, the macro generates
   the callable invalidator, and the `invalidations` dimension is tagged
   `declared` with a `runtime_caveat` rather than claiming `provable`.
5. **Two models named `Post`.** Cross-module collision. → Matching is on the
   last path segment, so a collision over-approximates (a false failure, never
   a false pass); documented, with `acknowledge_stale` as the release valve.
6. **Break every existing app.** → New attributes are additive, and the
   `inventory` submission goes inside the function body so `#[cached]` keeps
   working on an associated function. An untouched `#[cached]` is gated only by
   what derivation can see in it — usually nothing, so `undetermined`; where the
   body does name a repository or a model finder it is `derived`, and *is*
   checked. That is intended, but it means "additive" is about compilation, not
   about the gate's verdict.
7. **Cost.** → `inventory` statics only; nothing runs unless the dump env var
   is set.

## 4. Six hats

* **White (what is known).** `#[cached]` keys are already
  `concat!(module_path!(), "::", fn)` — a natural stable read identity.
  `#[repository]` already submits `RepositoryFacts` through `inventory`. The
  repository macro **regenerates** the trait, so method-level attributes are
  free to consume. `trybuild` fixtures and the companion-function convention
  (`__autumn_route_info_*`) exist.
* **Red (instinct).** The thing that kills this feature is a false failure on
  day one. Default posture must be: gate only what is proven.
* **Black (risks).** `repository.rs` is 25k lines — the edit must be additive
  and localized. Derivation heuristics are approximations. `type_name` carries
  generics. The `db` feature may be off.
* **Yellow (upside).** No framework has this. Zero wiring. `#[invalidates(path)]`
  is checked by the compiler, not by a string table. The read identity is the
  cache-key namespace, so a later runtime prefix invalidation is free.
* **Green (ideas kept).** Reuse the cache-key namespace as the read id; emit a
  `pub const __AUTUMN_CACHE_READ_ID__<fn>` companion so `invalidates(path)` is
  type-checked; emit a per-function invalidator so the edge is callable.
* **Blue (process).** Strict red→green→refactor, bottom up: pure checker →
  manifest → macro surface → inventory wiring → CLI gate → example + docs.

## 5. Design

### Read identity

A cached read is identified by its **cache-key namespace**:
`concat!(module_path!(), "::", <fn name>)` — the same string
`make_cache_key` already prefixes every entry with.

### Compiler-checked invalidation edges

`#[cached] fn recent_posts()` additionally emits

```rust
#[doc(hidden)]
pub const __AUTUMN_CACHE_READ_ID__recent_posts: &str =
    concat!(module_path!(), "::", "recent_posts");
```

with the function's own visibility. `#[repository(Post, invalidates(crate::views::recent_posts))]`
rewrites the last path segment to that const and references it. A typo, a
non-`#[cached]` target, or a private target out of scope is a **rustc error at
the write site** — the edge cannot be faked.

### Descriptors (`autumn/src/cache/coherence.rs`)

```rust
pub struct CachedReadDescriptor {
    pub id: &'static str,
    pub kind: ReadKind,                     // Cached | Fragment | ReadThrough
    pub reads: &'static [fn() -> &'static str],
    pub provenance: DependencyProvenance,   // Declared | Derived | Undetermined
    pub acknowledged_stale: Option<&'static str>,
    pub location: &'static str,
}

pub struct MutationDescriptor {
    pub repository: &'static str,
    pub method: &'static str,
    pub model: fn() -> &'static str,
    pub table: &'static str,
    pub invalidates: &'static [&'static str],
    pub acknowledged_stale: Option<&'static str>,
    pub location: &'static str,
}
```

Both `inventory::collect!`ed. A `declare_cached_read!` macro covers fragment /
read-through call sites the proc macros cannot see.

### The rule

A violation is a `(read, mutation)` pair where

* the read's provenance is **not** `Undetermined`, and
* neither side is acknowledged-stale, and
* the read's dependency set intersects the mutation's model, and
* the mutation's `invalidates` does not contain the read's id.

`--strict` additionally reports every `Undetermined` read.

### Manifest and gate

`AUTUMN_DUMP_CACHE_COHERENCE=1` makes the app print the manifest and exit.
`autumn cache audit` builds the binary, runs it in that mode, writes
`--manifest <path>`, prints a precise diagnostic naming *read, mutation, and
shared model*, and exits non-zero.

## 6. TDD plan

| Step | Red | Green |
|------|-----|-------|
| 1 | `coherence` unit tests: matching, the rule, acknowledgement, strict mode | the pure checker |
| 2 | manifest shape / stable-order / provenance tests | `CoherenceManifest` + JSON |
| 3 | `cached.rs` macro tests: `reads(...)`, `acknowledge_stale`, derivation | attribute parsing + body derivation + codegen |
| 4 | `repository.rs` macro tests: `invalidates(...)`, per-method attrs | config parsing + descriptor emission |
| 5 | integration: seeded staleness corpus (100% flagged) + control app (0) | wiring end to end |
| 6 | `autumn-cli` audit tests: exit code, diagnostic, manifest artifact | `cache audit` command |
| 7 | trybuild compile-fail fixtures | diagnostics |
| 8 | example app red/green + guide + changelog | docs |

## 7. Explicitly out of slice

Automatic invocation of the declared invalidator from the write path
(commit-hook wiring), row/column-level precision, cross-service coherence, TTL
semantics, and Redis/moka internals.
