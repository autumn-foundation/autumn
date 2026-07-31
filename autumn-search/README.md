# autumn-search

Keyword **and** vector search for [autumn-web](https://crates.io/crates/autumn-web)
applications: mark a model searchable, get an index that stays in sync
automatically, and query it — by keyword or by semantic similarity — through
one engine-agnostic API.

The autumn-shaped answer to Laravel Scout, with vectors first-class rather than
bolted on.

## What you write

```rust
use std::sync::Arc;
use autumn_search::{SearchPlugin, SearchSyncHooks};

// 1. Mark the model searchable. `embed` nominates the field to embed.
#[autumn_web::model]
#[searchable(language = "english")]
pub struct Article {
    #[id] pub id: i64,
    #[searchable(weight = "A")] pub title: String,
    #[searchable(weight = "B", embed)] pub body: String,
}

// 2. Keep the index in sync from the record lifecycle. `#[repository]` takes a
//    plain type NAME for `hooks`, so alias the generic first.
type ArticleSearchHooks = SearchSyncHooks<Article, NewArticle, UpdateArticle>;

#[autumn_web::repository(Article, hooks = ArticleSearchHooks, commit_hooks = true)]
pub trait ArticleRepository {}

// 3. Mount the plugin.
autumn_web::app()
    .plugin(
        SearchPlugin::new()
            .postgres()
            .embedder(Arc::new(MyEmbedder))
            .index::<Article>(),
    )
    .run()
    .await;
```

## What you get

```rust
// The plugin installs the client as an `AppState` extension.
let search = state.extension::<autumn_search::SearchClient>().expect("SearchPlugin");

// ranked + paginated keyword search, as a `Page`
let page = search.search::<Article>("rust web framework", &page_req).await?;

// semantic "find similar" / RAG retrieval
let hits = search.similar::<Article>("how do I add auth?", 5).await?;
let neighbours = search.similar_to::<Article>(article.id, 5).await?;

// authorization-aware: the filter is pushed *into* the engine query
let page = search.search_for::<Article>(&ctx, "rust web", &page_req).await?;
```

Creating, updating, or deleting an `Article` enqueues a durable reindex job.
There is no hand-written index-sync code anywhere.

## Backfill

```bash
autumn search reindex                     # every registered index
autumn search reindex --index articles    # one index
autumn search reindex --purge             # clear first (after a schema change)
autumn search reindex --profile prod      # rebuild the prod profile's index
```

## Backends

| Backend | Keyword | Vector | Notes |
|---|---|---|---|
| `PostgresSearchStore` | `tsvector` + `ts_rank_cd` | `pgvector`, or a portable `double precision[]` fallback | The default. Reuses the in-core FTS from #842. |
| `MemorySearchBackend` | in-process, weighted | in-process cosine | Dev and tests; no Docker, no network. |
| yours | — | — | Implement `SearchBackend`. |

`SearchBackend` is plain data in, plain data out, so an external engine
(Meilisearch, Tantivy, a vector store) is a new `impl`, not a fork.

## Embeddings

`autumn-search` orchestrates embeddings; it ships **no model, runtime, or
vendor SDK**. Implement `Embedder` over whatever you use and install it with
`SearchPlugin::embedder(...)`. Two implementations ship, and neither is a
model:

- `NoEmbedder` — the default; refuses, so a missing provider is a typed error
  rather than meaningless vectors.
- `HashingEmbedder` — deterministic, dependency-free lexical vectors for dev
  and tests.

## Configuration

```toml
[search]
queue = "search"            # the #[job] queue reindex/backfill run on
batch_size = 500            # rows per backfill batch
enabled = true              # false ⇒ index writes are no-ops (incident switch)
embedding_dimensions = 768  # enables the pgvector fast path
```

The plugin reads this itself at boot, so `enabled = false` is a config change
rather than a deploy. Resolution goes through the same profile layering the
runtime uses — base `autumn.toml`, then `[profile.<name>.search]`, then
`autumn-<profile>.toml`, then `AUTUMN_SEARCH__*` env vars — so the kill switch
works per environment:

```toml
[profile.prod.search]
enabled = false
```

```bash
AUTUMN_SEARCH__ENABLED=false ./my-app   # or without touching a file at all
```

Pass `SearchPlugin::config(...)` (or any of the builder overrides) to configure
it in code instead.

## Documentation

See [`docs/guide/search.md`](https://github.com/autumn-foundation/autumn/blob/main/docs/guide/search.md)
for the full guide.

## License

MIT OR Apache-2.0
