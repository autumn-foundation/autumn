//! The one engine-agnostic query and indexing API.
//!
//! `SearchClient` is what an application actually touches: it owns the index
//! registry, the backend, the embedder, the document source, and the
//! visibility hook, and it composes them so the surface stays "one attribute
//! on the model + one call here".
//!
//! ```rust,ignore
//! // keyword, ranked + paginated
//! let page: Page<SearchHit> = search.search::<Article>("rust web", &page_req).await?;
//! // semantic "find similar"
//! let hits = search.similar::<Article>("how do I add auth?", 5).await?;
//! // authorization-aware
//! let page = search.search_for::<Article>(&ctx, "rust web", &page_req).await?;
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use autumn_web::authorization::PolicyContext;
use autumn_web::pagination::{ListQuery, Page, PageRequest};
use autumn_web::search::{IndexDefinition, SearchDocument, SearchIndexed};

use crate::authz::{SearchVisibility, ambient_tenant_filter};
use crate::backend::{
    IndexedDocument, KeywordQuery, SearchBackend, SearchFilter, SearchHit, TENANT_FILTER_KEY,
    VectorQuery, empty_page,
};
use crate::embedding::{Embedder, NoEmbedder};
use crate::error::{SearchError, SearchResult};
use crate::jobs::{ReindexArgs, ReindexOp};
use crate::source::DocumentSource;

/// How many rows a backfill reads per batch when nothing else says.
const DEFAULT_BACKFILL_BATCH: usize = 500;

// ── Backfill ────────────────────────────────────────────────────────────────

/// Knobs for a full reindex.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct BackfillOptions {
    /// Rows per batch. `0` is clamped to the default rather than looping
    /// forever on an empty scan.
    pub batch_size: usize,
    /// Clear the index before rebuilding it.
    ///
    /// Off by default: purging makes the index briefly empty, which is the
    /// wrong trade for a routine repair run. Turn it on for a schema change,
    /// where stale documents the source no longer produces would otherwise
    /// survive forever.
    pub purge: bool,
}

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BACKFILL_BATCH,
            purge: false,
        }
    }
}

impl BackfillOptions {
    /// Set the batch size.
    #[must_use]
    pub const fn batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size;
        self
    }

    /// Clear the index before rebuilding.
    #[must_use]
    pub const fn purge(mut self, purge: bool) -> Self {
        self.purge = purge;
        self
    }

    /// The batch size actually used, with `0` clamped to the default.
    #[must_use]
    pub const fn effective_batch_size(&self) -> usize {
        if self.batch_size == 0 {
            DEFAULT_BACKFILL_BATCH
        } else {
            self.batch_size
        }
    }
}

/// What a backfill did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackfillReport {
    /// The index that was rebuilt.
    pub index: String,
    /// Documents written.
    pub indexed: u64,
    /// Non-empty batches processed.
    pub batches: u64,
    /// Whether the index was cleared first.
    pub purged: bool,
}

// ── Client ──────────────────────────────────────────────────────────────────

/// The application-facing search API.
///
/// Cheap to clone (everything inside is an `Arc`); installed on `AppState` as
/// an extension by [`crate::SearchPlugin`].
#[derive(Clone)]
pub struct SearchClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    backend: Arc<dyn SearchBackend>,
    embedder: Arc<dyn Embedder>,
    source: Option<Arc<dyn DocumentSource>>,
    visibility: Option<Arc<dyn SearchVisibility>>,
    indexes: BTreeMap<String, IndexDefinition>,
    enabled: bool,
    batch_size: usize,
}

impl std::fmt::Debug for SearchClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchClient")
            .field("backend", &self.inner.backend.name())
            .field("indexes", &self.index_names())
            .field("embedding_dimensions", &self.inner.embedder.dimensions())
            .field("has_source", &self.inner.source.is_some())
            .field("has_visibility", &self.inner.visibility.is_some())
            .field("enabled", &self.inner.enabled)
            .finish()
    }
}

/// Builder for [`SearchClient`].
#[derive(Default)]
pub struct SearchClientBuilder {
    backend: Option<Arc<dyn SearchBackend>>,
    embedder: Option<Arc<dyn Embedder>>,
    source: Option<Arc<dyn DocumentSource>>,
    visibility: Option<Arc<dyn SearchVisibility>>,
    indexes: BTreeMap<String, IndexDefinition>,
    enabled: bool,
    batch_size: usize,
}

impl SearchClientBuilder {
    /// Start a builder with no backend (defaults to in-memory) and no indexes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: true,
            batch_size: DEFAULT_BACKFILL_BATCH,
            ..Self::default()
        }
    }

    /// Install the engine.
    #[must_use]
    pub fn backend(mut self, backend: Arc<dyn SearchBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Install the embedding provider.
    #[must_use]
    pub fn embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Install the record source used by reindex and backfill.
    #[must_use]
    pub fn source(mut self, source: Arc<dyn DocumentSource>) -> Self {
        self.source = Some(source);
        self
    }

    /// Install the authorization hook used by the `*_for` entry points.
    #[must_use]
    pub fn visibility(mut self, visibility: Arc<dyn SearchVisibility>) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Register `M`'s index, derived from its `#[searchable]` attributes.
    #[must_use]
    pub fn index<M: SearchIndexed>(mut self) -> Self {
        let definition = M::index_definition();
        self.indexes.insert(definition.name.to_owned(), definition);
        self
    }

    /// Register an index definition directly, for a corpus that is not a
    /// `#[model]` — or to adjust a derived one (clearing `tenant_scoped` on a
    /// model whose `tenant_id` column is not a tenant, say).
    ///
    /// A genuinely non-model corpus also needs its own
    /// [`crate::DocumentSource`]: the shipped Postgres source projects
    /// `FROM "<index name>"` and expects an `id` column plus a real column per
    /// indexed field.
    #[must_use]
    pub fn index_definition(mut self, definition: IndexDefinition) -> Self {
        self.indexes.insert(definition.name.to_owned(), definition);
        self
    }

    /// Enable or disable the subsystem. When disabled, index writes are
    /// no-ops and queries return empty pages.
    #[must_use]
    pub const fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Default rows per backfill batch, used when a backfill request does not
    /// name its own. `0` is clamped to the built-in default.
    #[must_use]
    pub const fn batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = if batch_size == 0 {
            DEFAULT_BACKFILL_BATCH
        } else {
            batch_size
        };
        self
    }

    /// Finish the client, defaulting the backend to in-memory and the embedder
    /// to [`NoEmbedder`].
    #[must_use]
    pub fn build(self) -> SearchClient {
        SearchClient {
            inner: Arc::new(ClientInner {
                backend: self
                    .backend
                    .unwrap_or_else(|| Arc::new(crate::memory::MemorySearchBackend::new())),
                embedder: self.embedder.unwrap_or_else(|| Arc::new(NoEmbedder)),
                source: self.source,
                visibility: self.visibility,
                indexes: self.indexes,
                enabled: self.enabled,
                batch_size: self.batch_size,
            }),
        }
    }
}

impl SearchClient {
    /// Start building a client.
    #[must_use]
    pub fn builder() -> SearchClientBuilder {
        SearchClientBuilder::new()
    }

    /// The installed backend.
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn SearchBackend> {
        &self.inner.backend
    }

    /// Whether the subsystem is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled
    }

    /// Configured default rows per backfill batch (`[search] batch_size`).
    #[must_use]
    pub fn default_batch_size(&self) -> usize {
        self.inner.batch_size
    }

    /// Width of the vectors the installed [`Embedder`] produces.
    ///
    /// `0` means no usable embedder is installed (the default
    /// [`NoEmbedder`](crate::NoEmbedder)), so nothing vector-shaped will run.
    /// Exposed so a host can check it against the width its backend was
    /// configured for **before** serving traffic, rather than discovering the
    /// disagreement as an empty result page.
    #[must_use]
    pub fn embedding_dimensions(&self) -> usize {
        self.inner.embedder.dimensions()
    }

    /// Registered index names, sorted.
    #[must_use]
    pub fn index_names(&self) -> Vec<String> {
        self.inner.indexes.keys().cloned().collect()
    }

    /// The definition registered under `name`.
    #[must_use]
    pub fn index_definition(&self, name: &str) -> Option<&IndexDefinition> {
        self.inner.indexes.get(name)
    }

    fn definition(&self, name: &str) -> SearchResult<&IndexDefinition> {
        self.inner
            .indexes
            .get(name)
            .ok_or_else(|| SearchError::UnknownIndex(name.to_owned()))
    }

    /// Create or migrate storage for every registered index.
    ///
    /// Idempotent; run from the plugin's startup hook.
    ///
    /// # Errors
    ///
    /// Propagates the backend's failure, and rejects an index definition that
    /// fails [`IndexDefinition::validate`].
    pub async fn ensure_indexes(&self) -> SearchResult<()> {
        for definition in self.inner.indexes.values() {
            self.inner.backend.ensure_index(definition).await?;
        }
        Ok(())
    }

    // ── Indexing ────────────────────────────────────────────────────────────

    /// Index (or re-index) one record.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`] if the model's index is not registered,
    /// plus any embedding or backend failure.
    pub async fn index_record<M: SearchIndexed>(&self, record: &M) -> SearchResult<()> {
        self.index_documents(M::SEARCH_INDEX, vec![record.search_document()])
            .await
    }

    /// Index a batch of already-extracted documents.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], plus any embedding or backend failure.
    pub async fn index_documents(
        &self,
        index: &str,
        documents: Vec<SearchDocument>,
    ) -> SearchResult<()> {
        if !self.inner.enabled || documents.is_empty() {
            return Ok(());
        }
        let definition = self.definition(index)?;
        let prepared = self.embed_documents(documents).await?;
        self.inner.backend.index(definition, &prepared).await
    }

    /// Remove one record from its index.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], plus any backend failure.
    pub async fn remove<M: SearchIndexed>(&self, id: i64) -> SearchResult<()> {
        self.remove_ids(M::SEARCH_INDEX, &[id]).await
    }

    /// Remove records by id.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], plus any backend failure.
    pub async fn remove_ids(&self, index: &str, ids: &[i64]) -> SearchResult<()> {
        if !self.inner.enabled || ids.is_empty() {
            return Ok(());
        }
        let definition = self.definition(index)?;
        self.inner.backend.delete(definition, ids).await
    }

    /// Attach embeddings to `documents` that declare `embed_text`.
    ///
    /// A zero-dimension embedder (the default [`NoEmbedder`]) means "no
    /// embeddings configured": documents are indexed for keyword search and
    /// simply carry no vector. Semantic queries then fail loudly at query
    /// time, which is the right place to notice a missing provider — an app
    /// that only wants keyword search should not have its writes fail.
    async fn embed_documents(
        &self,
        documents: Vec<SearchDocument>,
    ) -> SearchResult<Vec<IndexedDocument>> {
        if self.inner.embedder.dimensions() == 0 {
            return Ok(documents.into_iter().map(IndexedDocument::new).collect());
        }

        // One provider round-trip per batch, not per document.
        let mut positions = Vec::new();
        let mut texts = Vec::new();
        for (position, document) in documents.iter().enumerate() {
            if let Some(text) = &document.embed_text {
                positions.push(position);
                texts.push(text.clone());
            }
        }

        let mut prepared: Vec<IndexedDocument> =
            documents.into_iter().map(IndexedDocument::new).collect();
        if texts.is_empty() {
            return Ok(prepared);
        }

        let vectors = self.inner.embedder.embed(&texts).await?;
        if vectors.len() != positions.len() {
            return Err(SearchError::Embedding(format!(
                "embedder returned {} vectors for {} inputs",
                vectors.len(),
                positions.len()
            )));
        }
        for (position, vector) in positions.into_iter().zip(vectors) {
            if let Some(document) = prepared.get_mut(position) {
                document.embedding = Some(vector);
            }
        }
        Ok(prepared)
    }

    // ── Keyword queries ─────────────────────────────────────────────────────

    /// Ranked, paginated keyword search.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], plus any backend failure.
    pub async fn search<M: SearchIndexed>(
        &self,
        query: &str,
        request: &PageRequest,
    ) -> SearchResult<Page<SearchHit>> {
        self.search_filtered::<M>(query, request, SearchFilter::default())
            .await
    }

    /// Keyword search with an explicit filter.
    ///
    /// The ambient tenant restriction is intersected in automatically, so a
    /// caller-supplied filter can only narrow.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], plus any backend failure.
    pub async fn search_filtered<M: SearchIndexed>(
        &self,
        query: &str,
        request: &PageRequest,
        filter: SearchFilter,
    ) -> SearchResult<Page<SearchHit>> {
        if !self.inner.enabled {
            return Ok(empty_page(request));
        }
        let definition = self.definition(M::SEARCH_INDEX)?;
        let filter = ambient_tenant_filter(definition)?.intersect(filter);
        let keyword = KeywordQuery::new(query, *request).filter(filter);
        self.inner
            .backend
            .keyword_search(definition, &keyword)
            .await
    }

    /// Keyword search driven by a request's [`ListQuery`] and [`PageRequest`],
    /// so search drops into an existing index endpoint without a second
    /// query-parameter vocabulary.
    ///
    /// `filter[...]` keys that name an indexed field (or the reserved
    /// `tenant_id`) become exact-match predicates; unknown keys are **ignored**,
    /// exactly as the generated `list()` ignores non-allowlisted keys. Sorting
    /// is ignored: search results are relevance-ordered by definition.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], plus any backend failure.
    pub async fn search_list<M: SearchIndexed>(
        &self,
        query: &str,
        list: &ListQuery,
        request: &PageRequest,
    ) -> SearchResult<Page<SearchHit>> {
        // Checked before resolving the index so a disabled client behaves the
        // same here as in `search` (an empty page), rather than surfacing an
        // `UnknownIndex` for an index it was never going to query.
        if !self.inner.enabled {
            return Ok(empty_page(request));
        }
        let definition = self.definition(M::SEARCH_INDEX)?;
        let filter = filter_from_list_query(definition, list);
        self.search_filtered::<M>(query, request, filter).await
    }

    /// Authorization-aware keyword search.
    ///
    /// # Errors
    ///
    /// [`SearchError::VisibilityUnavailable`] when no [`SearchVisibility`] is
    /// registered — an authorization-aware search never silently runs
    /// unfiltered — plus whatever the hook or the backend returns.
    pub async fn search_for<M: SearchIndexed>(
        &self,
        ctx: &PolicyContext,
        query: &str,
        request: &PageRequest,
    ) -> SearchResult<Page<SearchHit>> {
        // Checked before the hook: with search off there is nothing to
        // authorize, and a missing or failing `SearchVisibility` must not turn
        // the documented empty page into an error. Otherwise the kill switch
        // would be ineffective on exactly the endpoints that use it.
        if !self.inner.enabled {
            return Ok(empty_page(request));
        }
        let filter = self.visibility_filter(ctx, M::SEARCH_INDEX).await?;
        self.search_filtered::<M>(query, request, filter).await
    }

    /// Keyword search whose hits are turned back into records by `loader`.
    ///
    /// The loader receives the ranked ids and may return them in any order, or
    /// omit records that vanished between the search and the load. The client
    /// re-applies the ranked order and drops the gaps.
    ///
    /// # The loader is a hydrator, not an authorization boundary
    ///
    /// Authorize with [`Self::search_for`] and a registered
    /// [`SearchVisibility`], which restricts the query itself. Dropping
    /// records in the loader instead leaves a **count oracle**: the caller
    /// still learns how many records matched. To blunt that, every record the
    /// loader omits is also subtracted from `total_elements` — but the totals
    /// then only approximate the current page, so this is damage limitation,
    /// not a substitute for filtering the query.
    ///
    /// # Errors
    ///
    /// Whatever `search` or `loader` returns.
    pub async fn search_hydrated<M, F, Fut>(
        &self,
        query: &str,
        request: &PageRequest,
        loader: F,
    ) -> SearchResult<Page<M>>
    where
        M: SearchIndexed,
        F: FnOnce(Vec<i64>) -> Fut + Send,
        Fut: Future<Output = SearchResult<Vec<M>>> + Send,
    {
        let hits = self.search::<M>(query, request).await?;
        self.hydrate(hits, loader).await
    }

    /// Hydrate an already-fetched page of hits. See [`Self::search_hydrated`].
    ///
    /// # Errors
    ///
    /// Whatever `loader` returns.
    pub async fn hydrate<M, F, Fut>(
        &self,
        hits: Page<SearchHit>,
        loader: F,
    ) -> SearchResult<Page<M>>
    where
        M: SearchIndexed,
        F: FnOnce(Vec<i64>) -> Fut + Send,
        Fut: Future<Output = SearchResult<Vec<M>>> + Send,
    {
        let order: Vec<i64> = hits.content.iter().map(|hit| hit.id).collect();
        let request = PageRequest::new(hits.page, hits.size);
        let records = loader(order.clone()).await?;

        // Keyed by id, so a loader that returns duplicates cannot produce a
        // page longer than the ranked set — and the ranked order wins over
        // whatever order the loader used.
        let mut by_id: BTreeMap<i64, M> = records
            .into_iter()
            .map(|record| (record.search_id(), record))
            .collect();
        let content: Vec<M> = order.iter().filter_map(|id| by_id.remove(id)).collect();

        // Subtract what the loader could not produce, so the pager does not
        // advertise records the caller never received.
        let dropped = u64::try_from(order.len().saturating_sub(content.len())).unwrap_or(0);
        let total = i64::try_from(hits.total_elements.saturating_sub(dropped)).unwrap_or(i64::MAX);

        Ok(Page::new(content, total, &request))
    }

    // ── Vector queries ──────────────────────────────────────────────────────

    /// The `limit` records most semantically similar to `text`.
    ///
    /// # Errors
    ///
    /// [`SearchError::EmbedderUnavailable`] when no embedding provider is
    /// installed, [`SearchError::VectorUnsupported`] when the model declares
    /// no `#[searchable(embed)]` field, plus any backend failure.
    pub async fn similar<M: SearchIndexed>(
        &self,
        text: &str,
        limit: usize,
    ) -> SearchResult<Vec<SearchHit>> {
        self.similar_filtered::<M>(text, limit, SearchFilter::default())
            .await
    }

    /// [`Self::similar`] with an explicit filter.
    ///
    /// # Errors
    ///
    /// As [`Self::similar`].
    pub async fn similar_filtered<M: SearchIndexed>(
        &self,
        text: &str,
        limit: usize,
        filter: SearchFilter,
    ) -> SearchResult<Vec<SearchHit>> {
        if !self.inner.enabled {
            return Ok(Vec::new());
        }
        if self.inner.embedder.dimensions() == 0 {
            return Err(SearchError::EmbedderUnavailable);
        }
        let vector = self
            .inner
            .embedder
            .embed(std::slice::from_ref(&text.to_owned()))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                SearchError::Embedding("embedder returned no vector for the query".to_owned())
            })?;
        self.similar_to_vector::<M>(vector, limit, filter).await
    }

    /// Authorization-aware semantic search.
    ///
    /// # Errors
    ///
    /// [`SearchError::VisibilityUnavailable`] when no hook is registered, plus
    /// whatever [`Self::similar`] returns.
    pub async fn similar_for<M: SearchIndexed>(
        &self,
        ctx: &PolicyContext,
        text: &str,
        limit: usize,
    ) -> SearchResult<Vec<SearchHit>> {
        if !self.inner.enabled {
            return Ok(Vec::new());
        }
        let filter = self.visibility_filter(ctx, M::SEARCH_INDEX).await?;
        self.similar_filtered::<M>(text, limit, filter).await
    }

    /// Neighbours of an already-indexed record ("more like this").
    ///
    /// Reads the record's stored embedding rather than re-embedding it, and
    /// excludes the record itself from its own neighbour list.
    ///
    /// The **seed read is filtered too**. `id` is caller-supplied, so an
    /// unfiltered read would be an inference channel: the caller could rank
    /// their own probe documents against a record they are not allowed to see,
    /// and learn both that it exists and roughly what it says. A seed the
    /// filter excludes reads back as absent, so the result is an empty
    /// neighbour list — exactly as if the record were not indexed.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], [`SearchError::VectorUnsupported`],
    /// [`SearchError::TenantContextMissing`], plus any backend failure. A
    /// record with no stored embedding yields an empty neighbour list rather
    /// than an error.
    pub async fn similar_to<M: SearchIndexed>(
        &self,
        id: i64,
        limit: usize,
    ) -> SearchResult<Vec<SearchHit>> {
        self.similar_to_filtered::<M>(id, limit, SearchFilter::default())
            .await
    }

    /// Authorization-aware "more like this".
    ///
    /// The registered [`SearchVisibility`] gates **both** the seed read and
    /// the neighbour query, so this is not a back door around `search_for`.
    ///
    /// # Errors
    ///
    /// [`SearchError::VisibilityUnavailable`] when no hook is registered, plus
    /// whatever [`Self::similar_to`] returns.
    pub async fn similar_to_for<M: SearchIndexed>(
        &self,
        ctx: &PolicyContext,
        id: i64,
        limit: usize,
    ) -> SearchResult<Vec<SearchHit>> {
        if !self.inner.enabled {
            return Ok(Vec::new());
        }
        let filter = self.visibility_filter(ctx, M::SEARCH_INDEX).await?;
        self.similar_to_filtered::<M>(id, limit, filter).await
    }

    /// [`Self::similar_to`] with an explicit filter applied to the seed read
    /// and the neighbour query alike.
    ///
    /// # Errors
    ///
    /// As [`Self::similar_to`].
    pub async fn similar_to_filtered<M: SearchIndexed>(
        &self,
        id: i64,
        limit: usize,
        filter: SearchFilter,
    ) -> SearchResult<Vec<SearchHit>> {
        if !self.inner.enabled {
            return Ok(Vec::new());
        }
        let definition = self.definition(M::SEARCH_INDEX)?;
        let seed_filter = ambient_tenant_filter(definition)?.intersect(filter.clone());
        let Some(vector) = self
            .inner
            .backend
            .embedding(definition, id, &seed_filter)
            .await?
        else {
            return Ok(Vec::new());
        };
        self.similar_to_vector::<M>(vector, limit, filter.exclude_ids([id]))
            .await
    }

    /// k-NN search against a caller-supplied vector.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], [`SearchError::VectorUnsupported`],
    /// [`SearchError::DimensionMismatch`], plus any backend failure.
    pub async fn similar_to_vector<M: SearchIndexed>(
        &self,
        vector: Vec<f32>,
        limit: usize,
        filter: SearchFilter,
    ) -> SearchResult<Vec<SearchHit>> {
        if !self.inner.enabled {
            return Ok(Vec::new());
        }
        let definition = self.definition(M::SEARCH_INDEX)?;
        let filter = ambient_tenant_filter(definition)?.intersect(filter);
        let query = VectorQuery::new(vector, limit).filter(filter);
        self.inner.backend.vector_search(definition, &query).await
    }

    async fn visibility_filter(
        &self,
        ctx: &PolicyContext,
        index: &str,
    ) -> SearchResult<SearchFilter> {
        let visibility = self
            .inner
            .visibility
            .as_ref()
            .ok_or(SearchError::VisibilityUnavailable)?;
        visibility.filter(ctx, index).await
    }

    // ── Reindex & backfill ──────────────────────────────────────────────────

    /// Apply one reindex instruction.
    ///
    /// This is the whole of index sync, and **both** operations do the same
    /// thing: re-read the row and make the index agree. Present ⇒ upsert;
    /// absent (hard-deleted, or soft-deleted and therefore filtered out by the
    /// source) ⇒ remove the document.
    ///
    /// Convergence is why the delete op re-reads too, rather than deleting
    /// unconditionally. Consider a record deleted and then re-created under the
    /// same primary key: a delete job that retries, or simply arrives after the
    /// re-create's upsert already ran, would otherwise remove a live record and
    /// leave it missing until some later mutation. Because both ops are
    /// "converge this id", **any** order and any duplication of instructions
    /// reaches the same state — which is what makes at-least-once delivery and
    /// the `(index, id)` dedup key safe.
    ///
    /// [`ReindexOp`] survives as intent, and as the one place the two paths
    /// differ: with no [`crate::DocumentSource`] installed there is nothing to
    /// re-read, so a delete still removes the document (it needs no source)
    /// while an upsert reports [`SearchError::SourceUnavailable`].
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], [`SearchError::SourceUnavailable`] for an
    /// upsert with no document source, plus any backend failure.
    pub async fn reindex(&self, args: &ReindexArgs) -> SearchResult<()> {
        // Checked FIRST: with search disabled, an in-flight job for an index
        // the app no longer registers must no-op, not fail five times and
        // dead-letter.
        if !self.inner.enabled {
            return Ok(());
        }
        let definition = self.definition(&args.index)?;

        let Some(source) = self.inner.source.as_ref() else {
            return match args.op {
                // A delete needs no source: "not in the index" is reachable
                // without knowing anything about the row.
                ReindexOp::Delete => self.inner.backend.delete(definition, &[args.id]).await,
                ReindexOp::Upsert => Err(SearchError::SourceUnavailable),
            };
        };

        let documents = source.fetch(definition, &[args.id]).await?;
        if documents.is_empty() {
            // The row is gone. Converge by removing the document — this is how
            // a lost delete event, a soft delete, or a row removed by direct
            // SQL repairs itself.
            return self.inner.backend.delete(definition, &[args.id]).await;
        }
        let prepared = self.embed_documents(documents).await?;
        self.inner.backend.index(definition, &prepared).await
    }

    /// Rebuild one index from its document source.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], [`SearchError::SourceUnavailable`], plus
    /// any backend failure.
    pub async fn backfill(
        &self,
        index: &str,
        options: &BackfillOptions,
    ) -> SearchResult<BackfillReport> {
        if !self.inner.enabled {
            // The incident kill switch must stop writes, and a `purge`
            // backfill is the most destructive write there is.
            return Ok(BackfillReport {
                index: index.to_owned(),
                indexed: 0,
                batches: 0,
                purged: false,
            });
        }
        let definition = self.definition(index)?;
        let source = self
            .inner
            .source
            .as_ref()
            .ok_or(SearchError::SourceUnavailable)?;

        if options.purge {
            self.inner.backend.clear(definition).await?;
        }

        let batch_size = options.effective_batch_size();
        let mut report = BackfillReport {
            index: index.to_owned(),
            indexed: 0,
            batches: 0,
            purged: options.purge,
        };
        let mut after: Option<i64> = None;

        // Taken ONCE, before the first batch, and passed to every write.
        //
        // A backfill reads a batch, embeds it (a provider round-trip — slow),
        // then writes. A mutation inside that window enqueues a reindex job
        // that reads and writes the *new* state, and the backfill's in-flight
        // batch would then overwrite it with what the source said minutes ago.
        // Nothing re-runs the reindex, so the index stays wrong until the next
        // mutation, and the same ordering resurrects a record deleted mid-run.
        //
        // The watermark makes the rule "a backfill never overwrites a document
        // written after the backfill began" — newer always wins, and the two
        // writers converge without knowing about each other. A backend that
        // does not report `conditional_index` ignores it and keeps
        // last-writer-wins.
        let watermark = self.inner.backend.write_watermark().await?;
        if watermark.is_none() && !self.inner.backend.capabilities().conditional_index {
            tracing::debug!(
                backend = self.inner.backend.name(),
                index = %index,
                "backend has no write watermark; a concurrent reindex may be overwritten by this \
                 backfill"
            );
        }

        loop {
            let documents = source.scan(definition, after, batch_size).await?;
            if documents.is_empty() {
                break;
            }
            // Keyset cursor: the scan is ordered by ascending id, so the last
            // row of the batch is where the next one resumes. Never `OFFSET`,
            // which would skip rows as concurrent writes shift the window.
            after = documents.last().map(|document| document.id);
            let count = documents.len() as u64;

            let prepared = self.embed_documents(documents).await?;
            self.inner
                .backend
                .index_unless_newer(definition, &prepared, watermark.as_deref())
                .await?;

            report.indexed += count;
            report.batches += 1;

            // No early exit on a short batch. `DocumentSource::scan` returns
            // **up to** `limit` documents, so a short-but-non-empty batch does
            // not mean the source is exhausted — a custom source that filters
            // after reading, or reads a fixed page and drops soft-deleted
            // rows, legitimately returns fewer. Stopping there would silently
            // truncate the rebuild at that batch and report success. An empty
            // batch is the only end-of-source signal, at the cost of one extra
            // scan per backfill.
        }

        tracing::info!(
            index = %report.index,
            indexed = report.indexed,
            batches = report.batches,
            purged = report.purged,
            "search backfill complete"
        );
        Ok(report)
    }

    /// Rebuild every registered index.
    ///
    /// # Errors
    ///
    /// As [`Self::backfill`], for the first index that fails.
    pub async fn backfill_all(
        &self,
        options: &BackfillOptions,
    ) -> SearchResult<Vec<BackfillReport>> {
        let mut reports = Vec::new();
        for index in self.index_names() {
            reports.push(self.backfill(&index, options).await?);
        }
        Ok(reports)
    }
}

/// Translate a request's `filter[...]` keys into a [`SearchFilter`].
///
/// Only keys naming an indexed field (or the reserved `tenant_id`) are
/// applied; anything else is ignored, matching how the generated `list()`
/// treats a non-allowlisted filter key. That is what lets one query string
/// drive both endpoints.
fn filter_from_list_query(definition: &IndexDefinition, list: &ListQuery) -> SearchFilter {
    let mut filter = SearchFilter::default();
    for (key, value) in list.filters() {
        if key == TENANT_FILTER_KEY {
            filter = filter.tenant(value);
        } else if definition.field_names().any(|field| field == key) {
            filter = filter.equals(key, value);
        }
    }
    filter
}

#[cfg(test)]
mod tests {
    use autumn_web::pagination::SortDir;
    use autumn_web::search::SearchIndexField;

    use super::*;

    const FIELDS: &[SearchIndexField] = &[
        SearchIndexField::new("title", 'A'),
        SearchIndexField::new("body", 'B'),
    ];

    fn definition() -> IndexDefinition {
        IndexDefinition::new("articles", "english", FIELDS, Some("body"), false)
    }

    #[test]
    fn list_query_filters_map_onto_indexed_fields() {
        let list = ListQuery::new(None, SortDir::Asc, &[("title", "Hello")]);
        let filter = filter_from_list_query(&definition(), &list);
        assert_eq!(
            filter.equals.get("title").map(String::as_str),
            Some("Hello")
        );
    }

    #[test]
    fn the_reserved_tenant_key_becomes_a_tenant_restriction() {
        let list = ListQuery::new(None, SortDir::Asc, &[("tenant_id", "acme")]);
        let filter = filter_from_list_query(&definition(), &list);
        assert_eq!(filter.tenant_id.as_deref(), Some("acme"));
        assert!(filter.equals.is_empty());
    }

    #[test]
    fn an_unknown_filter_key_is_ignored_like_list() {
        let list = ListQuery::new(None, SortDir::Asc, &[("nope", "x")]);
        let filter = filter_from_list_query(&definition(), &list);
        assert_eq!(filter, SearchFilter::default());
    }

    #[test]
    fn backfill_options_clamp_a_zero_batch_size() {
        assert_eq!(
            BackfillOptions::default()
                .batch_size(0)
                .effective_batch_size(),
            DEFAULT_BACKFILL_BATCH
        );
        assert_eq!(
            BackfillOptions::default()
                .batch_size(7)
                .effective_batch_size(),
            7
        );
    }

    #[test]
    fn a_client_defaults_to_the_memory_backend_and_no_embedder() {
        let client = SearchClient::builder().build();
        assert_eq!(client.backend().name(), "memory");
        assert!(client.is_enabled());
        assert!(client.index_names().is_empty());
    }
}
