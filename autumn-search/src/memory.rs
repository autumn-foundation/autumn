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
    indexes: RwLock<HashMap<String, HashMap<i64, IndexedDocument>>>,
    /// Per-document write sequence, and the source of the write watermark.
    ///
    /// A monotonic counter rather than a clock: the watermark only ever has to
    /// answer "was this written after that", and a counter answers it exactly,
    /// with no resolution floor and nothing to skew. It is also what lets the
    /// backfill-versus-reindex ordering be tested deterministically without a
    /// database.
    writes: RwLock<WriteLog>,
}

/// The monotonic write counter plus, per index, the sequence each document was
/// last written at.
#[derive(Debug, Default)]
struct WriteLog {
    next: u64,
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
        self.indexes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(index)
            .map_or(0, HashMap::len)
    }

    /// Names of every index that has been created.
    #[must_use]
    pub fn index_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .indexes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Run `f` against the (created-on-demand) document map for `definition`.
    /// Recovers from a poisoned lock rather than propagating, per
    /// CONTRIBUTING.md's contract: the guarded data is a plain document map
    /// with no invariant a panicking writer could have left half-applied.
    fn with_index<T>(
        &self,
        definition: &IndexDefinition,
        f: impl FnOnce(&mut HashMap<i64, IndexedDocument>) -> T,
    ) -> T {
        let mut guard = self
            .indexes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(guard.entry(definition.name.to_owned()).or_default())
    }

    /// The current write sequence.
    fn sequence(&self) -> u64 {
        self.writes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next
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

        let mut writes = self
            .writes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut applied: Vec<(i64, u64)> = Vec::with_capacity(documents.len());
        {
            let WriteLog { next, sequences } = &mut *writes;
            let seen = sequences.entry(definition.name.to_owned()).or_default();
            for document in documents {
                // A document written after the watermark is left alone: the
                // writer that produced it read the source more recently than
                // this batch did, so overwriting it would move the index
                // backwards.
                if let (Some(watermark), Some(written)) = (watermark, seen.get(&document.id()))
                    && *written > watermark
                {
                    continue;
                }
                *next += 1;
                applied.push((document.id(), *next));
            }
            for (id, sequence) in &applied {
                seen.insert(*id, *sequence);
            }
        }
        drop(writes);

        let applied_ids: std::collections::HashSet<i64> =
            applied.iter().map(|(id, _)| *id).collect();
        self.with_index(definition, |index| {
            for document in documents {
                if !applied_ids.contains(&document.id()) {
                    continue;
                }
                // Keyed upsert: re-indexing the same record replaces it, so
                // at-least-once delivery can never duplicate a document.
                index.insert(document.id(), document.clone());
            }
        });
        Ok(())
    }

    /// Run `f` against a read view of the documents for `definition`.
    fn read_index<T>(
        &self,
        definition: &IndexDefinition,
        f: impl FnOnce(Option<&HashMap<i64, IndexedDocument>>) -> T,
    ) -> T {
        let guard = self
            .indexes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(guard.get(definition.name))
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
    tokens: &[String],
) -> Option<f32> {
    let mut total = 0.0_f32;
    let mut matched = vec![false; tokens.len()];

    for field in &document.document.fields {
        if field.value.is_empty() {
            continue;
        }
        let Some(weight) = definition.weight_of(field.name) else {
            continue;
        };
        let factor = weight_factor(weight);
        for field_token in tokenize(&field.value) {
            for (index, query_token) in tokens.iter().enumerate() {
                if field_token == *query_token {
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
            self.with_index(definition, |index| {
                for id in ids {
                    // Absent ids are a no-op: deletes are replayed.
                    index.remove(id);
                }
            });
            Ok(())
        })
    }

    fn clear<'a>(&'a self, definition: &'a IndexDefinition) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            self.with_index(definition, HashMap::clear);
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
                for document in index.into_iter().flat_map(HashMap::values) {
                    if !query.filter.permits(&document.document) {
                        continue;
                    }
                    if let Some(score) = score(definition, document, &tokens) {
                        hits.push(SearchHit::new(definition.name, document.id(), score));
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
                for document in index.into_iter().flat_map(HashMap::values) {
                    let Some(embedding) = &document.embedding else {
                        continue;
                    };
                    // Filter FIRST: a document the caller cannot see must not
                    // influence the outcome at all. Checking the width before
                    // the filter would let one tenant's differently-sized
                    // embedding abort every other tenant's vector search, and
                    // leak that document's width in the error.
                    if !query.filter.permits(&document.document) {
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
                    hits.push(SearchHit::new(definition.name, document.id(), score));
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
                    .filter(|document| filter.permits(&document.document))
                    .and_then(|document| document.embedding.clone())
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

    #[test]
    fn scoring_requires_every_query_token() {
        let document = doc(1, "Rust web", "frameworks");
        assert!(score(&definition(), &document, &["rust".to_owned()]).is_some());
        assert!(
            score(
                &definition(),
                &document,
                &["rust".to_owned(), "web".to_owned()]
            )
            .is_some()
        );
        assert!(
            score(
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
        let title_hit = score(
            &definition(),
            &doc(1, "rust", "nothing"),
            &["rust".to_owned()],
        )
        .expect("match");
        let body_hit = score(
            &definition(),
            &doc(2, "nothing", "rust"),
            &["rust".to_owned()],
        )
        .expect("match");
        assert!(title_hit > body_hit, "{title_hit} !> {body_hit}");
    }

    #[test]
    fn scoring_accumulates_repeated_occurrences() {
        let once = score(&definition(), &doc(1, "rust", ""), &["rust".to_owned()]).expect("match");
        let twice = score(
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
            score(&definition(), &honest, &["rust".to_owned()]).expect("title match");
        let inflated_score =
            score(&definition(), &inflated, &["rust".to_owned()]).expect("body match");
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
            score(&definition(), &smuggled, &["rust".to_owned()]).is_none(),
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
