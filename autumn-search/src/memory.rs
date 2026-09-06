//! In-process reference [`SearchBackend`].
//!
//! Two jobs:
//!
//! 1. **The executable specification.** Every behaviour in the backend
//!    contract — ranking, pagination totals, fail-closed blank queries,
//!    idempotent upserts, filter semantics, k-NN ordering — is implemented
//!    here in ~200 readable lines, so a new engine implementor has something
//!    to diff against.
//! 2. **Infra-free tests and local dev.** An app (or this crate's own suite)
//!    gets complete keyword *and* vector search with no Postgres, no Docker,
//!    and no network.
//!
//! It is not for production: everything lives in one process's memory and
//! disappears on restart.

use std::collections::HashMap;
use std::sync::RwLock;

use autumn_web::pagination::{Page, PageRequest};
use autumn_web::search::{IndexDefinition, weight_factor};

use crate::backend::{
    BackendCapabilities, BoxFuture, IndexedDocument, KeywordQuery, SearchBackend, SearchFilter,
    SearchHit, VectorQuery, empty_page,
};
use crate::embedding::cosine_similarity;
use crate::error::{SearchError, SearchResult};
use crate::text::{query_tokens, tokenize};

/// An in-memory [`SearchBackend`].
///
/// Cheap to clone-by-`Arc`; internally a `RwLock` over per-index document
/// maps.
#[derive(Debug, Default)]
pub struct MemorySearchBackend {
    /// Documents AND their write sequences under **one** lock.
    ///
    /// Deliberately not two: a conditional write has to compare a sequence and
    /// replace a document as a single step. With separate locks a backfill can
    /// read an old sequence, a reindex can then claim a newer one and store
    /// fresh content, and the backfill still overwrites it — the exact race
    /// the watermark exists to prevent, reintroduced by the bookkeeping meant
    /// to close it.
    store: RwLock<Store>,
}

/// An indexed document plus its field values, tokenized once.
///
/// `keyword_search` used to call [`tokenize`] on every field of every document
/// on every query — profiling a 5,000-document/~200-word-body corpus found
/// that re-tokenization (`score`'s own loop plus the `str::to_lowercase`
/// allocation inside [`tokenize`]) accounted for ~66% of the call's
/// instructions. A document's tokens don't change between searches, only
/// between writes, so they are computed once here, in [`MemorySearchBackend::write`],
/// and reused by every later `score` call until the document is rewritten.
#[derive(Debug, Clone)]
struct StoredDocument {
    indexed: IndexedDocument,
    /// Tokens of `indexed.document.fields[i].value`, aligned by index.
    field_tokens: Vec<Vec<String>>,
}

impl StoredDocument {
    fn new(indexed: IndexedDocument) -> Self {
        let field_tokens = indexed
            .document
            .fields
            .iter()
            .map(|field| tokenize(&field.value).collect())
            .collect();
        Self {
            indexed,
            field_tokens,
        }
    }
}

/// Documents plus the write bookkeeping that decides which write wins.
#[derive(Debug, Default)]
struct Store {
    documents: HashMap<String, HashMap<i64, StoredDocument>>,
    /// Monotonic counter behind the write watermark. A counter rather than a
    /// clock: the watermark only ever answers "was this written after that",
    /// which a counter answers exactly, with no resolution floor and nothing
    /// to skew — and it makes the interleaving deterministically testable.
    next_sequence: u64,
    /// Per index, the sequence each record was last **written or deleted** at.
    ///
    /// Deletes are recorded too, and outlive the document. A delete that only
    /// removed the value would leave no evidence it happened, so a stale
    /// backfill batch would see no newer sequence and reinsert the record —
    /// resurrecting something a user deleted.
    sequences: HashMap<String, HashMap<i64, u64>>,
}

impl MemorySearchBackend {
    /// Create an empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of documents currently held in `index`. Test/inspection helper.
    #[must_use]
    pub fn document_count(&self, index: &str) -> usize {
        self.store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .documents
            .get(index)
            .map_or(0, HashMap::len)
    }

    /// Names of every index that has been created.
    #[must_use]
    pub fn index_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .documents
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Run `f` against the whole store. Recovers from a poisoned lock rather
    /// than propagating, per CONTRIBUTING.md's contract: the guarded data is a
    /// plain map with no invariant a panicking writer could have left
    /// half-applied.
    fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> T) -> T {
        let mut guard = self
            .store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }

    /// Run `f` against the (created-on-demand) document map for `definition`.
    fn with_index<T>(
        &self,
        definition: &IndexDefinition,
        f: impl FnOnce(&mut HashMap<i64, StoredDocument>) -> T,
    ) -> T {
        self.with_store(|store| {
            f(store
                .documents
                .entry(definition.name.to_owned())
                .or_default())
        })
    }

    /// The current write sequence.
    fn sequence(&self) -> u64 {
        self.store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next_sequence
    }

    /// Record that `ids` were deleted, so a later conditional write can see
    /// that something newer happened to them.
    fn record_deletes(&self, definition: &IndexDefinition, ids: &[i64]) {
        self.with_store(|store| {
            let index = definition.name.to_owned();
            store.documents.entry(index.clone()).or_default();
            let sequences = store.sequences.entry(index.clone()).or_default();
            let mut next = store.next_sequence;
            for id in ids {
                next = next.saturating_add(1);
                sequences.insert(*id, next);
            }
            store.next_sequence = next;
            if let Some(documents) = store.documents.get_mut(&index) {
                for id in ids {
                    // Absent ids are a no-op: deletes are replayed.
                    documents.remove(id);
                }
            }
        });
    }

    /// Upsert `documents`, skipping any whose stored write is newer than
    /// `watermark`. The single write path behind both `index` and
    /// `index_unless_newer`, so the two cannot drift apart.
    fn write(
        &self,
        definition: &IndexDefinition,
        documents: &[IndexedDocument],
        watermark: Option<u64>,
    ) -> SearchResult<()> {
        definition
            .validate()
            .map_err(|e| SearchError::InvalidIndex(e.to_string()))?;

        // Tokenize before taking the lock. `StoredDocument::new` is pure
        // per-document work with no dependency on store state, and it is
        // considerably heavier than the compare-and-set bookkeeping below —
        // doing it inside `with_store` would hold the exclusive lock for the
        // whole batch's tokenization time, blocking every concurrent
        // keyword/vector/embedding read (all of which only need a read lock)
        // for that whole span. A watermark-superseded document (rare: only
        // when a concurrent write raced this batch) is tokenized and then
        // discarded below rather than skipped — the price of not knowing
        // which documents will be superseded until the lock is held.
        let prepared: Vec<StoredDocument> = documents
            .iter()
            .map(|document| StoredDocument::new(document.clone()))
            .collect();

        // ONE critical section for the whole compare-and-set. Checking the
        // sequence under one lock and replacing the document under another
        // would let a newer write slot in between the two, and the stale
        // batch would overwrite it while the bookkeeping claimed the newer
        // write had won.
        self.with_store(|store| {
            let index = definition.name.to_owned();
            store.documents.entry(index.clone()).or_default();
            let mut next = store.next_sequence;
            let mut applied: Vec<(i64, u64)> = Vec::with_capacity(documents.len());
            {
                let sequences = store.sequences.entry(index.clone()).or_default();
                for document in documents {
                    // A record touched after the watermark — written OR
                    // deleted — is left alone: that writer read the source
                    // more recently than this batch did, so applying this one
                    // would move the index backwards.
                    if let (Some(watermark), Some(touched)) =
                        (watermark, sequences.get(&document.id()))
                        && *touched > watermark
                    {
                        continue;
                    }
                    next = next.saturating_add(1);
                    applied.push((document.id(), next));
                }
                for (id, sequence) in &applied {
                    sequences.insert(*id, *sequence);
                }
            }
            store.next_sequence = next;

            let applied_ids: std::collections::HashSet<i64> =
                applied.iter().map(|(id, _)| *id).collect();
            if let Some(map) = store.documents.get_mut(&index) {
                for stored in prepared {
                    if !applied_ids.contains(&stored.indexed.id()) {
                        continue;
                    }
                    // Keyed upsert: re-indexing the same record replaces it, so
                    // at-least-once delivery can never duplicate a document.
                    map.insert(stored.indexed.id(), stored);
                }
            }
        });
        Ok(())
    }

    /// Run `f` against a read view of the documents for `definition`.
    fn read_index<T>(
        &self,
        definition: &IndexDefinition,
        f: impl FnOnce(Option<&HashMap<i64, StoredDocument>>) -> T,
    ) -> T {
        let guard = self
            .store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(guard.documents.get(definition.name))
    }
}

/// Score `document` against `tokens`, ranking with `definition`'s weights.
///
/// Returns `None` unless **every** query token appears somewhere in the
/// document (the AND contract documented on [`KeywordQuery`]). The score sums
/// `weight_factor(field) × occurrences`, so a match in a weight-`A` field
/// outranks the same match in a weight-`B` field.
///
/// The weight comes from [`IndexDefinition::weight_of`], never from the
/// document — the same rule the Postgres backend follows when it interpolates
/// a `setweight(...)` letter. A document is data: it can arrive from a
/// third-party [`DocumentSource`](crate::DocumentSource), a hand-built
/// [`SearchDocument`], or a stale row written before the model's weights
/// changed, and any of those could otherwise promote a `D` field to `A` and
/// reorder someone else's results. A field the index does not declare is
/// skipped entirely, so it contributes neither score nor a token match.
fn score(
    definition: &IndexDefinition,
    document: &IndexedDocument,
    field_tokens: &[Vec<String>],
    tokens: &[String],
) -> Option<f32> {
    let mut total = 0.0_f32;
    let mut matched = vec![false; tokens.len()];

    for (field, field_tokens) in document.document.fields.iter().zip(field_tokens) {
        if field.value.is_empty() {
            continue;
        }
        let Some(weight) = definition.weight_of(field.name) else {
            continue;
        };
        let factor = weight_factor(weight);
        for field_token in field_tokens {
            for (index, query_token) in tokens.iter().enumerate() {
                if field_token == query_token {
                    total += factor;
                    if let Some(slot) = matched.get_mut(index) {
                        *slot = true;
                    }
                }
            }
        }
    }

    if matched.iter().all(|m| *m) && total > 0.0 {
        Some(total)
    } else {
        None
    }
}

/// Sort hits into the canonical order: score descending, then id ascending so
/// ties are stable across calls (and so pagination never skips or repeats).
fn sort_hits(hits: &mut [SearchHit]) {
    // `total_cmp`, not `partial_cmp(..).unwrap_or(Equal)`: mapping NaN to
    // `Equal` yields a non-transitive comparator, and `sort_by` panics on one
    // ("user-provided comparison function does not correctly implement a total
    // order"). No in-tree producer emits NaN, but a third-party backend or a
    // hand-built `SearchHit` can.
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
}

/// Take the `request`-th page of `hits`, preserving the pre-slice total.
fn paginate(hits: Vec<SearchHit>, request: &PageRequest) -> Page<SearchHit> {
    let total = i64::try_from(hits.len()).unwrap_or(i64::MAX);
    let size = request.size() as usize;
    let offset = (request.page().saturating_sub(1) as usize).saturating_mul(size);
    let content = hits.into_iter().skip(offset).take(size).collect();
    Page::new(content, total, request)
}

impl SearchBackend for MemorySearchBackend {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
            .with_vector(true)
            .with_weighted_fields(true)
            .with_embedding_readback(true)
            .with_conditional_index(true)
    }

    fn ensure_index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            definition
                .validate()
                .map_err(|e| SearchError::InvalidIndex(e.to_string()))?;
            self.with_index(definition, |_| ());
            Ok(())
        })
    }

    fn index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        documents: &'a [IndexedDocument],
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move { self.write(definition, documents, None) })
    }

    fn write_watermark(&self) -> BoxFuture<'_, SearchResult<Option<String>>> {
        Box::pin(async move { Ok(Some(self.sequence().to_string())) })
    }

    fn index_unless_newer<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        documents: &'a [IndexedDocument],
        watermark: Option<&'a str>,
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            self.write(
                definition,
                documents,
                watermark.and_then(|w| w.parse().ok()),
            )
        })
    }

    fn delete<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        ids: &'a [i64],
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            // Recorded in the sequence log, not just removed: the delete has
            // to outlive the document, or a backfill batch that scanned this
            // record before the delete would find no newer sequence and put it
            // straight back.
            self.record_deletes(definition, ids);
            Ok(())
        })
    }

    fn clear<'a>(&'a self, definition: &'a IndexDefinition) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            // A purge is a deliberate reset of the whole index, so the
            // sequence log goes with it — the backfill that follows is
            // *supposed* to rewrite everything.
            self.with_store(|store| {
                store
                    .documents
                    .entry(definition.name.to_owned())
                    .or_default()
                    .clear();
                store.sequences.remove(definition.name);
            });
            Ok(())
        })
    }

    fn keyword_search<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        query: &'a KeywordQuery,
    ) -> BoxFuture<'a, SearchResult<Page<SearchHit>>> {
        Box::pin(async move {
            // Fail closed on both "nothing to search for" and "nothing is
            // visible" — neither may degrade into a full listing.
            let Some(tokens) = query_tokens(&query.text) else {
                return Ok(empty_page(&query.page));
            };
            if query.filter.matches_nothing() {
                return Ok(empty_page(&query.page));
            }

            let mut hits = self.read_index(definition, |index| {
                let mut hits = Vec::new();
                for stored in index.into_iter().flat_map(HashMap::values) {
                    if !query.filter.permits(&stored.indexed.document) {
                        continue;
                    }
                    if let Some(score) =
                        score(definition, &stored.indexed, &stored.field_tokens, &tokens)
                    {
                        hits.push(SearchHit::new(definition.name, stored.indexed.id(), score));
                    }
                }
                hits
            });

            sort_hits(&mut hits);
            Ok(paginate(hits, &query.page))
        })
    }

    fn vector_search<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        query: &'a VectorQuery,
    ) -> BoxFuture<'a, SearchResult<Vec<SearchHit>>> {
        Box::pin(async move {
            if !definition.supports_vector_search() {
                return Err(SearchError::VectorUnsupported {
                    index: definition.name.to_owned(),
                });
            }
            if query.filter.matches_nothing() || query.limit == 0 {
                return Ok(Vec::new());
            }

            let mut hits = Vec::new();
            let mismatch = self.read_index(definition, |index| {
                for stored in index.into_iter().flat_map(HashMap::values) {
                    let Some(embedding) = &stored.indexed.embedding else {
                        continue;
                    };
                    // Filter FIRST: a document the caller cannot see must not
                    // influence the outcome at all. Checking the width before
                    // the filter would let one tenant's differently-sized
                    // embedding abort every other tenant's vector search, and
                    // leak that document's width in the error.
                    if !query.filter.permits(&stored.indexed.document) {
                        continue;
                    }
                    // A width disagreement means the index was built with a
                    // different embedder — scoring it would silently return
                    // garbage, so surface it instead.
                    if embedding.len() != query.vector.len() {
                        return Some(SearchError::DimensionMismatch {
                            expected: embedding.len(),
                            actual: query.vector.len(),
                        });
                    }
                    let score = cosine_similarity(embedding, &query.vector);
                    if query.min_score.is_some_and(|min| score < min) {
                        continue;
                    }
                    hits.push(SearchHit::new(definition.name, stored.indexed.id(), score));
                }
                None
            });
            if let Some(error) = mismatch {
                return Err(error);
            }

            sort_hits(&mut hits);
            hits.truncate(query.limit);
            Ok(hits)
        })
    }

    fn embedding<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        id: i64,
        filter: &'a SearchFilter,
    ) -> BoxFuture<'a, SearchResult<Option<Vec<f32>>>> {
        Box::pin(async move {
            Ok(self.read_index(definition, |index| {
                index
                    .and_then(|documents| documents.get(&id))
                    // A record the filter excludes reads back as absent, so a
                    // "more like this" seed cannot be a record the caller
                    // cannot see.
                    .filter(|stored| filter.permits(&stored.indexed.document))
                    .and_then(|stored| stored.indexed.embedding.clone())
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::search::{SearchDocument, SearchIndexField};

    use super::*;

    const FIELDS: &[SearchIndexField] = &[
        SearchIndexField::new("title", 'A'),
        SearchIndexField::new("body", 'B'),
    ];

    fn definition() -> IndexDefinition {
        IndexDefinition::new("articles", "english", FIELDS, Some("body"), false)
    }

    fn doc(id: i64, title: &str, body: &str) -> IndexedDocument {
        IndexedDocument::new(
            SearchDocument::new("articles", id)
                .with_field("title", 'A', title)
                .with_field("body", 'B', body),
        )
    }

    /// `score` now takes the document's pre-tokenized fields alongside the
    /// document itself (computed once at write time in real use); tests
    /// derive them the same way `StoredDocument::new` does.
    fn score_doc(
        definition: &IndexDefinition,
        document: &IndexedDocument,
        tokens: &[String],
    ) -> Option<f32> {
        let stored = StoredDocument::new(document.clone());
        score(definition, &stored.indexed, &stored.field_tokens, tokens)
    }

    #[test]
    fn scoring_requires_every_query_token() {
        let document = doc(1, "Rust web", "frameworks");
        assert!(score_doc(&definition(), &document, &["rust".to_owned()]).is_some());
        assert!(
            score_doc(
                &definition(),
                &document,
                &["rust".to_owned(), "web".to_owned()]
            )
            .is_some()
        );
        assert!(
            score_doc(
                &definition(),
                &document,
                &["rust".to_owned(), "gardening".to_owned()]
            )
            .is_none(),
            "a missing token must veto the match"
        );
    }

    #[test]
    fn scoring_weights_a_title_hit_above_a_body_hit() {
        let title_hit = score_doc(
            &definition(),
            &doc(1, "rust", "nothing"),
            &["rust".to_owned()],
        )
        .expect("match");
        let body_hit = score_doc(
            &definition(),
            &doc(2, "nothing", "rust"),
            &["rust".to_owned()],
        )
        .expect("match");
        assert!(title_hit > body_hit, "{title_hit} !> {body_hit}");
    }

    #[test]
    fn scoring_accumulates_repeated_occurrences() {
        let once =
            score_doc(&definition(), &doc(1, "rust", ""), &["rust".to_owned()]).expect("match");
        let twice = score_doc(
            &definition(),
            &doc(1, "rust rust", ""),
            &["rust".to_owned()],
        )
        .expect("match");
        assert!(twice > once);
    }

    #[test]
    fn scoring_uses_the_definitions_weight_not_the_documents() {
        // A document that claims weight `A` for the index's weight-`B` `body`
        // field must not outrank a real `A` hit. The stored weight is data —
        // it can come from a third-party source or a row written before the
        // model changed — so ranking reads the definition instead.
        let honest = doc(1, "rust", "");
        let inflated = IndexedDocument::new(
            SearchDocument::new("articles", 2)
                .with_field("title", 'A', "")
                .with_field("body", 'A', "rust"),
        );
        let honest_score =
            score_doc(&definition(), &honest, &["rust".to_owned()]).expect("title match");
        let inflated_score =
            score_doc(&definition(), &inflated, &["rust".to_owned()]).expect("body match");
        assert!(
            honest_score > inflated_score,
            "a document-declared weight must not promote a body hit: \
             {honest_score} !> {inflated_score}"
        );
    }

    #[test]
    fn scoring_ignores_fields_the_index_does_not_declare() {
        // An undeclared field contributes neither score nor a token match, so
        // it cannot satisfy the AND contract on its own.
        let smuggled = IndexedDocument::new(
            SearchDocument::new("articles", 1)
                .with_field("title", 'A', "")
                .with_field("body", 'B', "")
                .with_field("internal_notes", 'A', "rust"),
        );
        assert!(
            score_doc(&definition(), &smuggled, &["rust".to_owned()]).is_none(),
            "a field outside the index definition must not be searchable"
        );
    }

    #[test]
    fn sorting_breaks_score_ties_by_ascending_id() {
        let mut hits = vec![
            SearchHit::new("articles", 3, 1.0),
            SearchHit::new("articles", 1, 1.0),
            SearchHit::new("articles", 2, 5.0),
        ];
        sort_hits(&mut hits);
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2, 1, 3]);
    }

    #[test]
    fn paginating_preserves_the_pre_slice_total() {
        let hits: Vec<SearchHit> = (1..=5).map(|id| SearchHit::new("a", id, 1.0)).collect();
        let page = paginate(hits, &PageRequest::new(2, 2));
        assert_eq!(
            page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(page.total_elements, 5);
    }

    #[test]
    fn paginating_past_the_end_is_empty_not_wrapped() {
        let hits: Vec<SearchHit> = (1..=3).map(|id| SearchHit::new("a", id, 1.0)).collect();
        let page = paginate(hits, &PageRequest::new(9, 2));
        assert!(page.content.is_empty());
        assert_eq!(page.total_elements, 3);
    }

    #[tokio::test]
    async fn clearing_one_index_leaves_the_others_alone() {
        let backend = MemorySearchBackend::new();
        let articles = definition();
        let mut notes = definition();
        notes.name = "notes";

        backend.ensure_index(&articles).await.expect("ensure");
        backend.ensure_index(&notes).await.expect("ensure");
        backend
            .index(&articles, &[doc(1, "a", "b")])
            .await
            .expect("index");
        backend
            .index(&notes, &[doc(1, "a", "b")])
            .await
            .expect("index");

        backend.clear(&articles).await.expect("clear");
        assert_eq!(backend.document_count("articles"), 0);
        assert_eq!(backend.document_count("notes"), 1);
        assert_eq!(backend.index_names(), vec!["articles", "notes"]);
    }

    #[tokio::test]
    async fn a_zero_limit_vector_query_does_no_work() {
        let backend = MemorySearchBackend::new();
        let definition = definition();
        backend.ensure_index(&definition).await.expect("ensure");
        let hits = backend
            .vector_search(&definition, &VectorQuery::new(vec![1.0], 0))
            .await
            .expect("vector search");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn embedding_readback_returns_none_for_an_unknown_record() {
        let backend = MemorySearchBackend::new();
        let definition = definition();
        backend.ensure_index(&definition).await.expect("ensure");
        assert!(
            backend
                .embedding(&definition, 42, &SearchFilter::default())
                .await
                .expect("read")
                .is_none()
        );
    }

    #[tokio::test]
    async fn embedding_readback_honours_the_filter() {
        // "Find records like this one" must not become an oracle: reading the
        // seed record's vector is a query, and a record the filter excludes
        // must read back as absent.
        let backend = MemorySearchBackend::new();
        let definition = definition();
        backend.ensure_index(&definition).await.expect("ensure");
        backend
            .index(
                &definition,
                &[IndexedDocument::new(
                    SearchDocument::new("articles", 1)
                        .with_field("title", 'A', "secret")
                        .with_tenant("globex"),
                )
                .with_embedding(vec![1.0, 0.0])],
            )
            .await
            .expect("index");

        assert!(
            backend
                .embedding(&definition, 1, &SearchFilter::default().tenant("acme"))
                .await
                .expect("read")
                .is_none(),
            "another tenant's embedding must not be readable"
        );
        assert!(
            backend
                .embedding(&definition, 1, &SearchFilter::default().tenant("globex"))
                .await
                .expect("read")
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_impossible_filter_short_circuits_both_query_paths() {
        let backend = MemorySearchBackend::new();
        let definition = definition();
        backend.ensure_index(&definition).await.expect("ensure");
        backend
            .index(
                &definition,
                &[doc(1, "rust", "rust").with_embedding(vec![1.0])],
            )
            .await
            .expect("index");

        let impossible = SearchFilter::default().allow_ids(Vec::<i64>::new());
        let page = backend
            .keyword_search(
                &definition,
                &KeywordQuery::new("rust", PageRequest::default()).filter(impossible.clone()),
            )
            .await
            .expect("keyword");
        assert!(page.content.is_empty());

        let hits = backend
            .vector_search(
                &definition,
                &VectorQuery::new(vec![1.0], 10).filter(impossible),
            )
            .await
            .expect("vector");
        assert!(hits.is_empty());
    }
}
