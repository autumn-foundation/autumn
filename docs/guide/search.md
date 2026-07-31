# Search: keyword and vector (`autumn-search`)

Autumn ships full-text search *primitives* in core: the `#[searchable]` model
attribute, a Postgres `tsvector` column, and a `#[repository(searchable)]`
`search()` method. That covers "search this table."

`autumn-search` is the **subsystem** on top: declare a model searchable, get an
index that stays in sync as records change, and query it — by keyword or by
semantic similarity — through one engine-agnostic API. It is an optional plugin
crate; an app that never installs it pays nothing.

- **Crate:** `autumn-search`
- **Issue:** [#1191](https://github.com/autumn-foundation/autumn/issues/1191)
- **Builds on:** [#842](https://github.com/autumn-foundation/autumn/issues/842)
  (in-core FTS), which it subsumes as one backend rather than replacing.

---

## 1. Mark a model searchable

```rust
#[autumn_web::model]
#[searchable(language = "english")]
pub struct Article {
    #[id]
    pub id: i64,

    #[searchable(weight = "A")]
    pub title: String,

    // `embed` nominates the field whose text is embedded for vector search.
    #[searchable(weight = "B", embed)]
    pub body: String,

    pub tenant_id: Option<String>,
}
```

`#[searchable]` is the single source of truth. From it, `#[model]` derives an
`IndexDefinition` (index name, language dictionary, weighted field list, embed
field) and a per-record `SearchDocument`. You never write either by hand.

| Attribute | Meaning |
|---|---|
| `#[searchable]` on the struct | the model has a search index; language defaults to `simple` |
| `#[searchable(language = "english")]` | text-search dictionary |
| `#[searchable]` on a field | index it at the lowest weight (`D`) |
| `#[searchable(weight = "A")]` | index it at weight `A` (A > B > C > D) |
| `#[searchable(embed)]` | **also** embed this field for vector search |

Rules the macro enforces at compile time:

- at most **one** `embed` field — a record has one embedding, so two is
  ambiguous rather than last-wins;
- `embed` requires the model-level `#[searchable]`;
- `#[encrypted]` and `#[searchable]` remain mutually exclusive (#805).

A `#[searchable]` model whose primary key is not `i64` keeps its #842 behaviour
and simply has no plugin index — search indexes key on `i64`.

The key **column** does not have to be called `id`. `#[id] pub note_id: i64` is
carried into the index definition as `key_column`, and the Postgres document
source selects, filters, and paginates on it — so a model over a legacy table
that still has an unrelated `id` column backfills correctly rather than keying
half its documents off the wrong value.

A `tenant_id` column of type `String` / `Option<String>` is picked up
automatically: its value is carried into every document, and the index is
marked **tenant-scoped**. Querying a tenant-scoped index with no tenant in
scope is then `SearchError::TenantContextMissing`, not a search across every
tenant — the same posture `#[repository(tenant_scoped)]` takes, and it is what
protects the paths that are easy to forget (a route mounted outside the tenancy
layer, a `#[job]`, a background task).

A model with no `tenant_id` column is unaffected and needs no tenant scope. If
a model has a `tenant_id` column that is *not* a tenant (a device id, say),
register a definition with the flag cleared:

```rust
let mut definition = Reading::index_definition();
definition.tenant_scoped = false;
SearchPlugin::new().index_definition(definition)
```

---

## 2. Keep the index in sync

```rust
use autumn_search::SearchSyncHooks;

// `#[repository]`'s `hooks =` takes a plain type NAME, not a generic type
// expression, so alias the generic first.
type ArticleSearchHooks = SearchSyncHooks<Article, NewArticle, UpdateArticle>;

#[autumn_web::repository(Article, hooks = ArticleSearchHooks, commit_hooks = true)]
pub trait ArticleRepository {}
```

Create, update, and delete now enqueue a durable reindex job. `commit_hooks =
true` writes the intent to Autumn's commit-hook queue **inside the same
transaction as the mutation**, so a rolled-back transaction never leaves a
phantom document and a process dying between commit and enqueue is recovered by
the queue rather than lost.

If the repository already has hooks, compose instead of replacing — call
`autumn_search::enqueue_reindex_for(&record)` (or `enqueue_unindex_for`) from
your own `after_*_commit`.

### Why the job payload is `(index, id)` and not the record

A reindex instruction carries the index name and the primary key. The handler
**re-reads the row** — for a delete as much as an upsert:

- row present ⇒ upsert the document;
- row absent (hard-deleted, or soft-deleted and therefore filtered out by the
  source) ⇒ delete the document.

Every instruction is the same operation: *converge this id*. So create, update,
delete, a replayed job, a lost delete event, and a row changed by direct SQL all
reach the same index state, in any order. At-least-once delivery is safe by
construction, a stale payload can never write stale text into the index, and a
late delete cannot evict a record that has since been recreated under the same
primary key. It is also what makes the `(index, id)` dedup key sound: repeated
writes collapse to one job precisely because the ops are interchangeable.

`ReindexOp` survives as intent, and as the one place the paths differ: with no
`DocumentSource` installed a delete still removes the document (it needs
nothing to re-read), while an upsert reports `SourceUnavailable`.

Convergence needs one more thing to hold on a multi-worker deployment. The
dedup key is released when a job **starts**, not when it finishes — otherwise a
write landing mid-reindex would be swallowed and the index would keep the
pre-write text. That means two jobs for one record can be in flight at once,
and because each re-reads the source they can interleave badly: A reads, a
write lands, B reads and writes the new state, then A writes the old one. So
the reindex job also carries a **concurrency cap of one per record**
(`(index, id)`, not per job type — distinct records still reindex fully in
parallel). The follow-up job is still enqueued immediately; it just waits for
the running one, then re-reads and converges.

---

## 3. Mount the plugin

```rust
use std::sync::Arc;
use autumn_search::SearchPlugin;

autumn_web::app()
    .plugin(
        SearchPlugin::new()
            .postgres()                       // FTS + pgvector backend
            .embedder(Arc::new(MyEmbedder))   // your provider
            .visibility(Arc::new(MyVisibility))
            .index::<Article>()               // one line per searchable model
            .index::<Note>(),
    )
    .routes(routes![...])
    .run()
    .await;
```

Adding a searchable model is one builder line: the reindex job is keyed on the
*index name*, not the model, so there is no per-model job, handler, or
generated glue.

### Configuration

```toml
[search]
queue = "search"            # the #[job] queue reindex/backfill run on
batch_size = 500            # rows per backfill batch
enabled = true              # false ⇒ index writes are no-ops, queries empty
embedding_dimensions = 768  # declared width; enables the pgvector fast path
```

The plugin reads the config itself at boot, so `enabled = false` is the
incident switch it claims to be: a config change, not a deploy — and it stops
index writes (including a purging backfill) without failing writes to the
model. Pass `SearchPlugin::config(...)` to configure in code instead; doing so
also stops the file being read.

`[search]` is resolved through the **same profile layering the runtime uses**,
so a per-environment switch works the way you would expect:

| Layer | Wins over |
|---|---|
| `autumn.toml` `[search]` | — |
| `[profile.<name>.search]` in `autumn.toml` | the base section |
| `autumn-<profile>.toml` `[search]` | the inline profile section |
| `AUTUMN_SEARCH__QUEUE` / `__BATCH_SIZE` / `__ENABLED` / `__EMBEDDING_DIMENSIONS` | every file |

```toml
# Search off in production, on everywhere else.
[profile.prod.search]
enabled = false
```

The active profile comes from `AUTUMN_ENV` → `AUTUMN_PROFILE` → `--profile` →
`AUTUMN_IS_DEBUG=0` ⇒ `prod` → `dev`, exactly as core resolves it, and each
filename is looked up through `AUTUMN_MANIFEST_DIR` before the working
directory. A plugin that read only the base `autumn.toml` would silently ignore
the kill switch in the one environment you reach for it.

An unknown key under `[search]` is an **error**, not a warning — a typo'd
`queu = "indexing"` would otherwise silently leave indexing on the default
queue with no signal at all. A malformed value in an `AUTUMN_SEARCH__*` var is
ignored instead: a bad env var must not take down a process that would boot
fine on its file config.

`embedding_dimensions` is checked against the installed `Embedder` at startup.
If they disagree the app **refuses to boot**, because the alternative is
silent: every index write succeeds, the vector column rejects the wrong-width
value, and semantic search returns nothing forever.

---

## 4. Query it

The plugin installs the client as an `AppState` extension, so a handler reaches
it the same way it reaches a `BlobStore`:

```rust
let search = state
    .extension::<autumn_search::SearchClient>()
    .expect("SearchPlugin is installed");
```

```rust
// Ranked + paginated keyword search, as an ordinary `Page`.
let page: Page<SearchHit> = search.search::<Article>("rust web", &page_req).await?;

// Driven by a request's ListQuery + PageRequest, so search drops into an
// existing index endpoint with no second query-parameter vocabulary.
let page = search.search_list::<Article>("rust web", &list, &page_req).await?;

// Turn hits back into records; ranked order is re-applied for you.
// The loader returns `SearchResult<Vec<Article>>`; an `AutumnError` from a
// repository call converts with `?`, so this is the whole loader.
let page: Page<Article> = search
    .search_hydrated::<Article, _, _>("rust web", &page_req, |ids| async move {
        Ok(repo.find_all(&ids).await?)
    })
    .await?;

// Semantic search over the `embed` field.
let hits = search.similar::<Article>("how do I add auth?", 5).await?;
let neighbours = search.similar_to::<Article>(article.id, 5).await?;
```

### Query semantics (the cross-backend contract)

Query text is a **bag of words**, not a query language:

- every token must be present for a document to match (AND);
- operator-looking input (`OR`, `field:`, `*`, quotes) is never interpreted as
  syntax;
- a blank or punctuation-only query returns an **empty page having issued no
  query** — never a full scan.

That keeps results consistent across engines and makes a hostile query string
structurally incapable of widening the result set. The Postgres backend
implements it with `plainto_tsquery`, which is parameterized and cannot be
injected into.

> This is deliberately narrower than `#[repository(searchable)]`'s `search()`,
> which uses `websearch_to_tsquery` and *does* honour `OR` / quoted phrases /
> `-` negation. The in-core method is a Postgres-specific convenience; the
> plugin's contract has to hold for every engine.

Hits carry identity (`index`, `id`, `score`), never record contents — the index
never becomes a second, staler copy of your database that can leak columns.

---

## 5. Authorization

A search index is a second read path over the same rows, so it needs the same
posture as a normal query.

```rust
use autumn_search::{BoxFuture, SearchError, SearchFilter, SearchVisibility};
use autumn_web::authorization::PolicyContext;

struct ArticleVisibility;

impl SearchVisibility for ArticleVisibility {
    fn filter<'a>(
        &'a self,
        ctx: &'a PolicyContext,
        _index: &'a str,
    ) -> BoxFuture<'a, Result<SearchFilter, SearchError>> {
        Box::pin(async move {
            Ok(match ctx.user_id.as_deref() {
                Some(user) => SearchFilter::default().equals("author_id", user),
                // Fail closed, exactly like `Scope`'s default empty list.
                None => SearchFilter::default().allow_ids(Vec::<i64>::new()),
            })
        })
    }
}
```

Then query through the authorization-aware entry points:

```rust
let page = search.search_for::<Article>(&ctx, "rust web", &page_req).await?;
let hits = search.similar_for::<Article>(&ctx, "rust web", 5).await?;
```

Four properties make this enforceable rather than advisory:

1. `SearchFilter` is a **required argument** of the backend query methods, not
   a post-processing step, so page totals and neighbour counts are computed
   *after* the restriction and a backend cannot quietly skip it.
2. Filters **intersect** (`SearchFilter::intersect`), so a caller-supplied
   filter can only ever narrow what authorization allowed. Two incompatible
   constraints collapse to "match nothing", never to an arbitrary winner.
3. A visibility hook that returns an error **aborts** the search. There is no
   fall-back-to-unfiltered path.
4. Calling `search_for` with no hook registered is
   `SearchError::VisibilityUnavailable` — a wiring mistake surfaces instead of
   silently returning everything.

Because a `SearchFilter` is plain data, an out-of-scope engine (Meilisearch, a
vector store) receives the same tenant/visibility restriction. The ambient
tenant from `CURRENT_TENANT` is intersected into **every** query automatically,
and `similar_to` filters the *seed* read as well as the neighbour query — an
unfiltered seed read would be an inference channel, letting a caller rank their
own probe documents against a record they cannot see.

---

## 6. Backfill

For bootstrapping and after a schema change:

```bash
autumn search reindex                     # every registered index
autumn search reindex --index articles    # one index
autumn search reindex --purge             # clear each index first
```

The CLI compiles the application binary and runs it with
`AUTUMN_SEARCH_BACKFILL` set — the same "run the app, it knows its own wiring"
technique `autumn jobs manifest` uses. It has to be the app: the indexes,
backend, and embedder are registered at runtime by your own builder call, so a
standalone CLI cannot see them.

Programmatically, or from a job:

```rust
let report = search.backfill("articles", &BackfillOptions::default()).await?;
let reports = search.backfill_all(&BackfillOptions::default().purge(true)).await?;
```

The backfill walks the source with **keyset** pagination
(`WHERE <key> > $after ORDER BY <key>`), never `OFFSET`, so a live table's
concurrent writes cannot make it skip or repeat rows. It writes through the
exact same path as an incremental reindex, so bootstrapping and steady state
cannot disagree.

It stops only on an **empty** batch, never on a short one. `DocumentSource::scan`
returns *up to* `limit` documents, so a source that filters after reading —
soft-deleted rows, a tenant check, an empty embed field — legitimately returns
a short batch with rows still behind it. Stopping there would truncate the
rebuild and report success.

`purge` is off by default: emptying the index is the wrong trade for a routine
repair run. Turn it on when documents the source no longer produces would
otherwise survive forever.

---

## 7. Backends

| Backend | Keyword | Vector | Use for |
|---|---|---|---|
| `PostgresSearchStore` | `tsvector` + `setweight` + `ts_rank_cd` | `pgvector`, or a portable `double precision[]` fallback | production |
| `MemorySearchBackend` | in-process, weighted | in-process cosine | dev and tests — no Docker, no network |
| your `impl SearchBackend` | — | — | Meilisearch, Tantivy, a vector store |

### The Postgres index

Documents live in one framework-owned table, `autumn_search_documents`, keyed
`(index_name, record_id)` — not in each model's own `search_vector` column.
That buys three things the per-table column cannot:

- the index is **engine-agnostic**: swapping engines changes the backend, not
  every model's migration;
- it is **observable and repairable**: count, diff, and purge the index without
  touching the system of record;
- backfill and incremental reindex write through the **same** path.

Your model's own `#[searchable]` column and `#[repository(searchable)]`
`search()` are untouched.

Ranking passes the framework's own weight array to `ts_rank_cd`
(`{D, C, B, A}` = `{0.1, 0.2, 0.5, 1.0}`, the normalized form of
`weight_factor`'s `1/2/5/10`) rather than taking Postgres' default
`{0.1, 0.2, 0.4, 1.0}`. The two differ only at `B` — which is the weight a
body field usually carries, so leaving it defaulted would rank Postgres
differently from `MemorySearchBackend` on the most ordinary model there is,
and the two suites assert they implement the *same* contract.

### `pgvector`, and life without it

`ensure_index` probes for the `vector` extension. When it is installed **and**
`search.embedding_dimensions` is set, embeddings go to a `vector(N)` column
with an ivfflat index and k-NN uses the `<=>` cosine-distance operator.
Otherwise they go to a `double precision[]` column ranked by an
`autumn_search_cosine()` SQL function created by the same schema.

Same API, same ordering, different speed — so the plugin is deployable on a
managed Postgres without `pgvector`, and gets the fast path for free where it
exists. A failed `CREATE EXTENSION` degrades; it never aborts boot.

---

## 8. Embeddings

`autumn-search` orchestrates embeddings. It ships **no model, no inference
runtime, and no vendor SDK** — that is explicitly out of scope. Implement
`Embedder`:

```rust
use autumn_search::{BoxFuture, Embedder, SearchError, SearchResult};

struct MyEmbedder { /* an HTTP client, a local runtime, … */ }

impl Embedder for MyEmbedder {
    fn dimensions(&self) -> usize { 768 }

    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, SearchResult<Vec<Vec<f32>>>> {
        Box::pin(async move {
            // one provider round-trip per batch; one vector per input, in order
            todo!()
        })
    }
}
```

Two implementations ship, and neither is a model:

- **`NoEmbedder`** — the default. Refuses, so an app that never configured
  embeddings gets a typed `EmbedderUnavailable` at *query* time rather than
  meaningless vectors. Keyword indexing still works; a missing provider must
  not fail writes.
- **`HashingEmbedder`** — a deterministic, dependency-free hashing vectorizer.
  Real lexical embeddings for development and for tests that must not reach the
  network. It is honest about what it is: no semantics beyond token overlap.

`dimensions() == 0` means "no embeddings available": the client skips embedding
while indexing and errors on a semantic query.

---

## 9. Errors

`SearchError` is typed, and `into_autumn_error()` maps it to the right status:

| Variant | Status | Meaning |
|---|---|---|
| `VectorUnsupported`, `DimensionMismatch` | 400 | the caller asked for something this index cannot answer |
| `UnknownIndex`, `EmbedderUnavailable`, `VisibilityUnavailable`, `SourceUnavailable`, `TenantContextMissing`, `InvalidIndex`, `Unsupported`, `Embedding`, `Backend` | 500 | a configuration gap or a genuine failure |

Every message names the builder call that fixes it.

---

## Out of scope (deliberately)

- a bundled embedding model or inference runtime — see §8;
- faceting, typo tolerance, synonyms, and highlighting beyond what a backend
  offers natively;
- cross-model / federated ranking fusion — indexes are per-model;
- a full RAG pipeline (chunking, prompt assembly, LLM calls). This is the
  retrieval layer.
