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

use crate::authz::{SearchVisibility, current_tenant_filter};
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
}

impl SearchClientBuilder {
    /// Start a builder with no backend (defaults to in-memory) and no indexes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: true,
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

    /// Register an index definition directly (for a non-`#[model]` corpus).
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
        let filter = current_tenant_filter().intersect(filter);
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
        let filter = self.visibility_filter(ctx, M::SEARCH_INDEX).await?;
        self.search_filtered::<M>(query, request, filter).await
    }

    /// Keyword search whose hits are turned back into records by `loader`.
    ///
    /// The loader receives the ranked ids and may return them in any order, or
    /// omit records it cannot produce (deleted between search and load, or
    /// filtered by its own authorization). The client re-applies the ranked
    /// order and drops the gaps, and preserves the pre-hydration
    /// `total_elements` so the pager stays consistent.
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
        let records = loader(order.clone()).await?;

        let mut by_id: BTreeMap<i64, M> = records
            .into_iter()
            .map(|record| (record.search_id(), record))
            .collect();
        let content: Vec<M> = order.iter().filter_map(|id| by_id.remove(id)).collect();

        Ok(Page {
            content,
            page: hits.page,
            size: hits.size,
            total_elements: hits.total_elements,
            total_pages: hits.total_pages,
            has_next: hits.has_next,
            has_previous: hits.has_previous,
        })
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
        let filter = self.visibility_filter(ctx, M::SEARCH_INDEX).await?;
        self.similar_filtered::<M>(text, limit, filter).await
    }

    /// Neighbours of an already-indexed record ("more like this").
    ///
    /// Reads the record's stored embedding rather than re-embedding it, and
    /// excludes the record itself from its own neighbour list.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], [`SearchError::VectorUnsupported`], plus
    /// any backend failure. A record with no stored embedding yields an empty
    /// neighbour list rather than an error.
    pub async fn similar_to<M: SearchIndexed>(
        &self,
        id: i64,
        limit: usize,
    ) -> SearchResult<Vec<SearchHit>> {
        if !self.inner.enabled {
            return Ok(Vec::new());
        }
        let definition = self.definition(M::SEARCH_INDEX)?;
        let Some(vector) = self.inner.backend.embedding(definition, id).await? else {
            return Ok(Vec::new());
        };
        self.similar_to_vector::<M>(vector, limit, SearchFilter::default().exclude_ids([id]))
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
        let filter = current_tenant_filter().intersect(filter);
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
    /// This is the whole of index sync: an **upsert** re-reads the row and
    /// writes it (or deletes the document when the row is gone), and a
    /// **delete** removes the document without touching the source. Both are
    /// idempotent, so at-least-once job delivery is safe, and both converge to
    /// the same state whichever order they arrive in.
    ///
    /// # Errors
    ///
    /// [`SearchError::UnknownIndex`], [`SearchError::SourceUnavailable`] for
    /// an upsert with no document source, plus any backend failure.
    pub async fn reindex(&self, args: &ReindexArgs) -> SearchResult<()> {
        let definition = self.definition(&args.index)?;
        if !self.inner.enabled {
            return Ok(());
        }

        if args.op == ReindexOp::Delete {
            return self.inner.backend.delete(definition, &[args.id]).await;
        }

        let source = self
            .inner
            .source
            .as_ref()
            .ok_or(SearchError::SourceUnavailable)?;
        let documents = source.fetch(definition, &[args.id]).await?;
        if documents.is_empty() {
            // The row is gone. Converge by removing the document — this is how
            // a lost delete event, or a row removed by direct SQL, repairs
            // itself.
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
            self.inner.backend.index(definition, &prepared).await?;

            report.indexed += count;
            report.batches += 1;

            if usize::try_from(count).unwrap_or(usize::MAX) < batch_size {
                break;
            }
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
        IndexDefinition::new("articles", "english", FIELDS, Some("body"))
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
