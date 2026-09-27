//! Third-party `SearchBackend` and `Embedder` implementations.
//!
//! AC 5 requires the trait be "shaped so an external engine … can be added
//! without a breaking change", and AC 6 requires embedding to be "a trait the
//! app implements". Both claims are only checkable from *outside* the crate:
//! the shipped implementations are privileged (they can name private items and
//! construct `#[non_exhaustive]` types by literal), so they cannot prove that
//! a downstream crate could do the same.
//!
//! This module is that downstream crate. It compiles against the public API
//! only, and every restriction that would block a real implementor shows up
//! here as a compile error.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_search::{
    BackendCapabilities, BoxFuture, Embedder, IndexDefinition, IndexedDocument, KeywordQuery, Page,
    PageRequest, SearchBackend, SearchClient, SearchError, SearchHit, SearchResult,
};

use super::support::{Article, article};

// ── A keyword-only engine ───────────────────────────────────────────────────

/// The minimum viable third-party backend: keyword search over an in-memory
/// list, no vectors at all.
///
/// It implements only the required methods. Everything vector-shaped falls
/// through to the trait's defaults, which is exactly the "a keyword-only
/// engine is a complete implementation" claim.
#[derive(Default)]
struct ToyBackend {
    documents: std::sync::Mutex<Vec<IndexedDocument>>,
    ensure_calls: AtomicUsize,
}

impl SearchBackend for ToyBackend {
    fn name(&self) -> &'static str {
        "toy"
    }

    fn ensure_index<'a>(
        &'a self,
        _definition: &'a IndexDefinition,
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            self.ensure_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn index<'a>(
        &'a self,
        _definition: &'a IndexDefinition,
        documents: &'a [IndexedDocument],
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            {
                let mut store = self.documents.lock().expect("lock");
                for document in documents {
                    store.retain(|existing| existing.id() != document.id());
                    store.push(document.clone());
                }
            }
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        _definition: &'a IndexDefinition,
        ids: &'a [i64],
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            {
                self.documents
                    .lock()
                    .expect("lock")
                    .retain(|document| !ids.contains(&document.id()));
            }
            Ok(())
        })
    }

    fn clear<'a>(&'a self, _definition: &'a IndexDefinition) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            {
                self.documents.lock().expect("lock").clear();
            }
            Ok(())
        })
    }

    fn keyword_search<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        query: &'a KeywordQuery,
    ) -> BoxFuture<'a, SearchResult<Page<SearchHit>>> {
        Box::pin(async move {
            // `tokenize` and `SearchFilter::permits` are public, so a
            // third-party backend does not have to reinvent either the query
            // contract or the authorization semantics.
            let tokens: Vec<String> = autumn_search::tokenize(&query.text).collect();
            let hits: Vec<SearchHit> = {
                let store = self.documents.lock().expect("lock");
                store
                    .iter()
                    .filter(|document| query.filter.permits(&document.document))
                    .filter(|document| {
                        let text = document.document.text().to_lowercase();
                        !tokens.is_empty() && tokens.iter().all(|token| text.contains(token))
                    })
                    .map(|document| SearchHit::new(definition.name, document.id(), 1.0))
                    .collect()
            };
            let total = i64::try_from(hits.len()).unwrap_or(i64::MAX);
            Ok(Page::new(hits, total, &query.page))
        })
    }
}

#[tokio::test]
async fn a_keyword_only_backend_is_a_complete_implementation() {
    let backend = Arc::new(ToyBackend::default());
    let client = SearchClient::builder()
        .backend(backend.clone())
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");
    assert_eq!(backend.ensure_calls.load(Ordering::SeqCst), 1);

    for record in [
        article(1, "Rust web frameworks", "A survey."),
        article(2, "Gardening", "Tomatoes."),
    ] {
        client.index_record(&record).await.expect("index");
    }

    let page = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );
}

#[tokio::test]
async fn a_backend_that_declines_vectors_reports_unsupported_rather_than_wrong_answers() {
    let client = SearchClient::builder()
        .backend(Arc::new(ToyBackend::default()))
        .embedder(Arc::new(DoubleEmbedder))
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");

    let error = client
        .similar::<Article>("rust", 3)
        .await
        .expect_err("the toy backend has no vector support");
    assert!(
        matches!(error, SearchError::Unsupported { backend, operation }
            if backend == "toy" && operation == "vector search"),
        "{error:?}"
    );
}

#[test]
fn a_third_party_backend_can_declare_its_capabilities() {
    // `BackendCapabilities` is `#[non_exhaustive]`, so a downstream crate
    // cannot build it with a struct literal. Without the builders it could
    // only ever return the keyword-only default and could never advertise
    // vector support — this asserts the builders make that possible from out
    // here, not just inside the crate.
    let caps = BackendCapabilities::default()
        .with_vector(true)
        .with_embedding_readback(true);
    assert!(caps.keyword);
    assert!(caps.vector);
    assert!(caps.embedding_readback);
    assert!(!caps.weighted_fields);

    assert!(!ToyBackend::default().capabilities().vector);
}

// ── An app-supplied embedder ────────────────────────────────────────────────

/// An embedder an application could plausibly write: two dimensions derived
/// from the text, no vendor, no model.
struct DoubleEmbedder;

impl Embedder for DoubleEmbedder {
    fn dimensions(&self) -> usize {
        2
    }

    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, SearchResult<Vec<Vec<f32>>>> {
        Box::pin(async move {
            Ok(texts
                .iter()
                .map(|text| {
                    let count = |f: fn(&char) -> bool| {
                        u16::try_from(text.chars().filter(f).count()).unwrap_or(u16::MAX)
                    };
                    let mut vector = vec![
                        f32::from(count(char::is_ascii_alphabetic)),
                        f32::from(count(char::is_ascii_digit)),
                    ];
                    autumn_search::normalize(&mut vector);
                    vector
                })
                .collect())
        })
    }
}

/// An embedder that misbehaves by returning the wrong number of vectors.
struct ShortEmbedder;

impl Embedder for ShortEmbedder {
    fn dimensions(&self) -> usize {
        2
    }

    fn embed<'a>(&'a self, _texts: &'a [String]) -> BoxFuture<'a, SearchResult<Vec<Vec<f32>>>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
}

#[tokio::test]
async fn an_app_supplied_embedder_drives_semantic_search() {
    let client = SearchClient::builder()
        .backend(Arc::new(autumn_search::MemorySearchBackend::new()))
        .embedder(Arc::new(DoubleEmbedder))
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");

    for record in [
        article(1, "Letters", "abcdefghij"),
        article(2, "Digits", "1234567890"),
    ] {
        client.index_record(&record).await.expect("index");
    }

    let hits = client
        .similar::<Article>("abcdefgh", 1)
        .await
        .expect("similar");
    assert_eq!(
        hits.first().map(|hit| hit.id),
        Some(1),
        "the app's own embedder decides what 'similar' means: {hits:?}"
    );
}

#[tokio::test]
async fn a_misbehaving_embedder_is_reported_not_silently_mis_assigned() {
    // Returning fewer vectors than inputs would otherwise attach the wrong
    // embedding to a document, which is far worse than an error.
    let client = SearchClient::builder()
        .backend(Arc::new(autumn_search::MemorySearchBackend::new()))
        .embedder(Arc::new(ShortEmbedder))
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");

    let error = client
        .index_record(&article(1, "Rust", "a rust framework"))
        .await
        .expect_err("a vector-count mismatch must surface");
    assert!(
        matches!(error, SearchError::Embedding(ref message) if message.contains("0 vectors")),
        "{error:?}"
    );
}
