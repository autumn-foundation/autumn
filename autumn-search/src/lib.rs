//! Keyword **and** vector search for `autumn-web` applications (issue #1191).
//!
//! Autumn already had full-text search *primitives* in core (#842): a
//! `#[searchable]` model attribute, a `tsvector` column, and a
//! `#[repository(searchable)]` `search()` method. What it did not have was a
//! **search subsystem** — a way to declare a model searchable, keep an index
//! in sync as records change, and query it through one engine-agnostic API,
//! let alone semantic / vector search for "find similar" and RAG retrieval.
//!
//! This crate is that subsystem: the autumn-shaped answer to Laravel Scout,
//! with vectors first-class rather than bolted on.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use autumn_search::{SearchPlugin, SearchSyncHooks};
//!
//! // 1. Mark the model searchable. `embed` nominates the field to embed.
//! #[autumn_web::model]
//! #[searchable(language = "english")]
//! pub struct Article {
//!     #[id] pub id: i64,
//!     #[searchable(weight = "A")] pub title: String,
//!     #[searchable(weight = "B", embed)] pub body: String,
//! }
//!
//! // 2. Keep the index in sync from the record lifecycle. `#[repository]`
//! //    takes a plain type NAME for `hooks`, so alias the generic first.
//! type ArticleSearchHooks = SearchSyncHooks<Article, NewArticle, UpdateArticle>;
//!
//! #[autumn_web::repository(Article, hooks = ArticleSearchHooks, commit_hooks = true)]
//! pub trait ArticleRepository {}
//!
//! // 3. Mount the plugin.
//! autumn_web::app()
//!     .plugin(
//!         SearchPlugin::new()
//!             .postgres()
//!             .embedder(Arc::new(MyEmbedder))
//!             .index::<Article>(),
//!     )
//!     .run()
//!     .await;
//!
//! // 4. Query it. The plugin installs the client as an `AppState` extension.
//! let search = state.extension::<SearchClient>().expect("SearchPlugin installed");
//! let page = search.search::<Article>("rust web framework", &page_req).await?;
//! let similar = search.similar::<Article>("how do I add auth?", 5).await?;
//! ```
//!
//! Configuration comes from `[search]`, which the plugin resolves itself
//! (base `autumn.toml` → `[profile.<name>.search]` → `autumn-<profile>.toml`
//! → `AUTUMN_SEARCH__*`, the same layering core applies) unless the app passes
//! `SearchPlugin::config(...)`.
//!
//! # How the pieces fit
//!
//! | Concern | Type | Notes |
//! | --- | --- | --- |
//! | Index shape | `autumn_web::search::IndexDefinition` | derived from `#[searchable]`, never hand-written |
//! | Record extraction | `autumn_web::search::SearchDocument` | derived from `#[searchable]` |
//! | Engine | [`SearchBackend`] | [`MemorySearchBackend`], [`PostgresSearchStore`], or yours |
//! | Embeddings | [`Embedder`] | no model or vendor ships here, by design |
//! | Index sync | [`SearchSyncHooks`] → [`ReindexArgs`] → `#[job]` | off-request and durable |
//! | Reading records | [`DocumentSource`] | drives both reindex and backfill |
//! | Authorization | [`SearchVisibility`] → [`SearchFilter`] | pushed *into* the engine query |
//! | Querying | [`SearchClient`] | `Page`/`ListQuery`-shaped results |
//!
//! # Design notes worth knowing
//!
//! **Reindex re-reads the row.** A reindex job carries `(index, id)`, not the
//! record. So create, update, delete, a replayed job, a lost event, and a row
//! changed by direct SQL all converge to the same index state, and
//! at-least-once delivery is safe by construction.
//!
//! **Filters are arguments, not afterthoughts.** [`SearchFilter`] is a
//! required parameter of the backend query methods, so an engine cannot
//! quietly return rows the caller may not see; filters *intersect*, so a
//! caller can only ever narrow what authorization allowed.
//!
//! **Query text is a bag of words.** Every token must match, and operator
//! syntax (`OR`, `field:`, `*`) is never interpreted. That keeps results
//! consistent across engines and makes a hostile query structurally incapable
//! of widening the result set.

// autumn-panic-gate: request-path crate — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

mod authz;
mod backend;
mod client;
mod config;
mod embedding;
mod error;
mod jobs;
mod memory;
mod plugin;
mod postgres;
mod source;
mod sync;
mod text;

pub use authz::{
    DenyAll, SearchVisibility, TenantVisibility, ambient_tenant_filter, current_tenant_filter,
};
pub use backend::{
    BackendCapabilities, BoxFuture, IndexedDocument, KeywordQuery, SearchBackend, SearchFilter,
    SearchHit, TENANT_FILTER_KEY, VectorQuery, empty_page,
};
pub use client::{BackfillOptions, BackfillReport, SearchClient, SearchClientBuilder};
pub use config::{SearchConfig, SearchConfigError};
pub use embedding::{Embedder, HashingEmbedder, NoEmbedder, cosine_similarity, normalize};
pub use error::{SearchError, SearchResult};
pub use jobs::{
    BACKFILL_JOB, BackfillArgs, REINDEX_JOB, ReindexArgs, ReindexOp, client_from_state,
    search_job_infos,
};
pub use memory::MemorySearchBackend;
pub use plugin::{
    BACKFILL_ENV, BACKFILL_PURGE_ENV, BackfillTarget, SearchPlugin, backfill_request_from_env,
};
pub use postgres::{DOCUMENTS_TABLE, PostgresSearchStore, VectorMode};
pub use source::{DocumentSource, MemoryDocumentSource};
pub use sync::{SearchSyncHooks, enqueue_reindex, enqueue_reindex_for, enqueue_unindex_for};
pub use text::{query_tokens, tokenize};

// Re-exported so an app can name the derived vocabulary without also depending
// on `autumn_web::search` directly.
pub use autumn_web::search::{
    IndexDefinition, SearchDocument, SearchFieldValue, SearchIndexField, SearchIndexed,
    SearchTextValue,
};

// Results are `Page`-shaped and queries are `PageRequest`/`ListQuery`-shaped,
// so re-export those too rather than making every call site reach into
// `autumn_web::pagination`.
pub use autumn_web::pagination::{ListQuery, Page, PageRequest};
