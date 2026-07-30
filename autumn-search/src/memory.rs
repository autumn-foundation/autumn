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
    BackendCapabilities, BoxFuture, IndexedDocument, KeywordQuery, SearchBackend, SearchHit,
    VectorQuery, empty_page,
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
            .map_or(0, |guard| guard.get(index).map_or(0, HashMap::len))
    }

    /// Names of every index that has been created.
    #[must_use]
    pub fn index_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .indexes
            .read()
            .map(|guard| guard.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Run `f` against the (created-on-demand) document map for `definition`.
    fn with_index<T>(
        &self,
        definition: &IndexDefinition,
        f: impl FnOnce(&mut HashMap<i64, IndexedDocument>) -> T,
    ) -> SearchResult<T> {
        let mut guard = self
            .indexes
            .write()
            .map_err(|_| SearchError::Backend("in-memory index lock poisoned".to_owned()))?;
        Ok(f(guard.entry(definition.name.to_owned()).or_default()))
    }

    /// Run `f` against a read view of the documents for `definition`.
    fn read_index<T>(
        &self,
        definition: &IndexDefinition,
        f: impl FnOnce(Option<&HashMap<i64, IndexedDocument>>) -> T,
    ) -> SearchResult<T> {
        let guard = self
            .indexes
            .read()
            .map_err(|_| SearchError::Backend("in-memory index lock poisoned".to_owned()))?;
        Ok(f(guard.get(definition.name)))
    }
}

/// Score `document` against `tokens`.
///
/// Returns `None` unless **every** query token appears somewhere in the
/// document (the AND contract documented on [`KeywordQuery`]). The score sums
/// `weight_factor(field) × occurrences`, so a match in a weight-`A` field
/// outranks the same match in a weight-`B` field.
fn score(document: &IndexedDocument, tokens: &[String]) -> Option<f32> {
    let mut total = 0.0_f32;
    let mut matched = vec![false; tokens.len()];

    for field in &document.document.fields {
        if field.value.is_empty() {
            continue;
        }
        let factor = weight_factor(field.weight);
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
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
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
        BackendCapabilities {
            keyword: true,
            vector: true,
            weighted_fields: true,
            embedding_readback: true,
        }
    }

    fn ensure_index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            definition
                .validate()
                .map_err(|e| SearchError::InvalidIndex(e.to_string()))?;
            self.with_index(definition, |_| ())?;
            Ok(())
        })
    }

    fn index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        documents: &'a [IndexedDocument],
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            definition
                .validate()
                .map_err(|e| SearchError::InvalidIndex(e.to_string()))?;
            self.with_index(definition, |index| {
                for document in documents {
                    // Keyed upsert: re-indexing the same record replaces it, so
                    // at-least-once delivery can never duplicate a document.
                    index.insert(document.id(), document.clone());
                }
            })
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
            })
        })
    }

    fn clear<'a>(&'a self, definition: &'a IndexDefinition) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move { self.with_index(definition, HashMap::clear) })
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
                    if let Some(score) = score(document, &tokens) {
                        hits.push(SearchHit::new(definition.name, document.id(), score));
                    }
                }
                hits
            })?;

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
                    // A width disagreement means the index was built with a
                    // different embedder — scoring it would silently return
                    // garbage, so surface it instead.
                    if embedding.len() != query.vector.len() {
                        return Some(SearchError::DimensionMismatch {
                            expected: embedding.len(),
                            actual: query.vector.len(),
                        });
                    }
                    if !query.filter.permits(&document.document) {
                        continue;
                    }
                    let score = cosine_similarity(embedding, &query.vector);
                    if query.min_score.is_some_and(|min| score < min) {
                        continue;
                    }
                    hits.push(SearchHit::new(definition.name, document.id(), score));
                }
                None
            })?;
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
    ) -> BoxFuture<'a, SearchResult<Option<Vec<f32>>>> {
        Box::pin(async move {
            self.read_index(definition, |index| {
                index
                    .and_then(|documents| documents.get(&id))
                    .and_then(|document| document.embedding.clone())
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::search::{SearchDocument, SearchIndexField};

    use crate::backend::SearchFilter;

    use super::*;

    const FIELDS: &[SearchIndexField] = &[
        SearchIndexField::new("title", 'A'),
        SearchIndexField::new("body", 'B'),
    ];

    fn definition() -> IndexDefinition {
        IndexDefinition::new("articles", "english", FIELDS, Some("body"))
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
        assert!(score(&document, &["rust".to_owned()]).is_some());
        assert!(score(&document, &["rust".to_owned(), "web".to_owned()]).is_some());
        assert!(
            score(&document, &["rust".to_owned(), "gardening".to_owned()]).is_none(),
            "a missing token must veto the match"
        );
    }

    #[test]
    fn scoring_weights_a_title_hit_above_a_body_hit() {
        let title_hit = score(&doc(1, "rust", "nothing"), &["rust".to_owned()]).expect("match");
        let body_hit = score(&doc(2, "nothing", "rust"), &["rust".to_owned()]).expect("match");
        assert!(title_hit > body_hit, "{title_hit} !> {body_hit}");
    }

    #[test]
    fn scoring_accumulates_repeated_occurrences() {
        let once = score(&doc(1, "rust", ""), &["rust".to_owned()]).expect("match");
        let twice = score(&doc(1, "rust rust", ""), &["rust".to_owned()]).expect("match");
        assert!(twice > once);
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
                .embedding(&definition, 42)
                .await
                .expect("read")
                .is_none()
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
