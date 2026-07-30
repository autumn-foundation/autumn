//! `MemorySearchBackend`: the reference `SearchBackend` implementation.
//!
//! Every behaviour asserted here is part of the **backend contract** — the
//! Postgres backend (and any external engine added later) must match it. That
//! is what makes `SearchBackend` a real seam rather than a Postgres-shaped
//! hole: the same suite runs against the Postgres backend in
//! `tests/postgres_backend.rs`.

use autumn_search::{
    IndexedDocument, KeywordQuery, MemorySearchBackend, SearchBackend as _, SearchError,
    SearchFilter, VectorQuery,
};
use autumn_web::pagination::PageRequest;
use autumn_web::search::SearchIndexed as _;

use super::support::{Article, article, seeded_backend, tenant_article};

fn corpus() -> Vec<Article> {
    vec![
        // "rust" in the title (weight A) — must outrank a body-only match.
        article(1, "Rust web frameworks", "A survey of the ecosystem."),
        // "rust" in the body only (weight B).
        article(2, "Getting started", "Autumn is a rust web framework."),
        // No match at all.
        article(3, "Gardening", "Tomatoes and basil."),
    ]
}

const fn page(number: u32, size: u32) -> PageRequest {
    PageRequest::new(number, size)
}

#[tokio::test]
async fn keyword_search_ranks_a_title_match_above_a_body_match() {
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    let results = backend
        .keyword_search(&def, &KeywordQuery::new("rust", page(1, 10)))
        .await
        .expect("keyword search");

    assert_eq!(
        results.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1, 2],
        "the weight-A title match must rank first"
    );
    assert_eq!(results.total_elements, 2);
    assert!(
        results.content[0].score > results.content[1].score,
        "scores must be strictly ordered, got {:?}",
        results.content
    );
}

#[tokio::test]
async fn keyword_search_paginates_with_a_stable_total() {
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    let first = backend
        .keyword_search(&def, &KeywordQuery::new("rust", page(1, 1)))
        .await
        .expect("page 1");
    let second = backend
        .keyword_search(&def, &KeywordQuery::new("rust", page(2, 1)))
        .await
        .expect("page 2");

    assert_eq!(
        first.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        second.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );
    // `total` is the size of the whole result set, not the page.
    assert_eq!(first.total_elements, 2);
    assert_eq!(second.total_elements, 2);

    let past_the_end = backend
        .keyword_search(&def, &KeywordQuery::new("rust", page(9, 1)))
        .await
        .expect("page 9");
    assert!(past_the_end.content.is_empty());
    assert_eq!(past_the_end.total_elements, 2);
}

#[tokio::test]
async fn an_empty_query_returns_an_empty_page_not_every_record() {
    // Fail closed: a blank search box must never degrade into "list
    // everything", which is both a scan and a disclosure hazard.
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    for blank in ["", "   ", "\t\n"] {
        let results = backend
            .keyword_search(&def, &KeywordQuery::new(blank, page(1, 10)))
            .await
            .expect("blank query");
        assert!(results.content.is_empty(), "blank query {blank:?} matched");
        assert_eq!(results.total_elements, 0);
    }
}

#[tokio::test]
async fn query_operators_are_matched_literally_not_interpreted() {
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    // None of these may error or widen the result set.
    for hostile in [
        "rust OR gardening",
        "title:rust",
        "rust*",
        "\"; DROP TABLE search_articles; --",
    ] {
        let results = backend
            .keyword_search(&def, &KeywordQuery::new(hostile, page(1, 10)))
            .await
            .expect("hostile query must not error");
        assert!(
            !results.content.iter().any(|h| h.id == 3),
            "query {hostile:?} must not match the unrelated record"
        );
    }
}

#[tokio::test]
async fn indexing_is_idempotent_and_updates_replace_the_previous_document() {
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    // Re-index the same record twice — the index must hold one document.
    let doc =
        IndexedDocument::new(article(1, "Rust web frameworks", "A survey.").search_document());
    backend
        .index(&def, std::slice::from_ref(&doc))
        .await
        .expect("index");
    backend.index(&def, &[doc]).await.expect("re-index");

    let results = backend
        .keyword_search(&def, &KeywordQuery::new("rust", page(1, 10)))
        .await
        .expect("search");
    assert_eq!(results.content.iter().filter(|h| h.id == 1).count(), 1);

    // Now mutate the record so it no longer mentions "rust": the index must
    // reflect the new text, not the old one.
    let updated =
        IndexedDocument::new(article(1, "Gardening weekly", "Tomatoes only.").search_document());
    backend.index(&def, &[updated]).await.expect("update");

    let after = backend
        .keyword_search(&def, &KeywordQuery::new("rust", page(1, 10)))
        .await
        .expect("search");
    assert_eq!(
        after.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );

    let recall = backend
        .keyword_search(&def, &KeywordQuery::new("tomatoes", page(1, 10)))
        .await
        .expect("search");
    assert!(recall.content.iter().any(|h| h.id == 1));
}

#[tokio::test]
async fn deleting_removes_the_document_and_is_a_no_op_when_absent() {
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    backend.delete(&def, &[1]).await.expect("delete");
    let results = backend
        .keyword_search(&def, &KeywordQuery::new("rust", page(1, 10)))
        .await
        .expect("search");
    assert_eq!(
        results.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );

    // Deleting an id that is not indexed converges rather than erroring —
    // at-least-once job delivery replays deletes.
    backend.delete(&def, &[1]).await.expect("repeat delete");
    backend.delete(&def, &[999]).await.expect("unknown delete");
}

#[tokio::test]
async fn clear_empties_only_the_named_index() {
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    assert_eq!(backend.document_count(def.name), 3);
    backend.clear(&def).await.expect("clear");
    assert_eq!(backend.document_count(def.name), 0);

    let results = backend
        .keyword_search(&def, &KeywordQuery::new("rust", page(1, 10)))
        .await
        .expect("search");
    assert!(results.content.is_empty());
}

#[tokio::test]
async fn ensure_index_is_idempotent_and_rejects_an_invalid_definition() {
    let backend = MemorySearchBackend::new();
    let def = Article::index_definition();
    backend.ensure_index(&def).await.expect("first");
    backend.ensure_index(&def).await.expect("second");

    let mut bad = def.clone();
    bad.name = "articles\"; DROP TABLE users; --";
    let err = backend.ensure_index(&bad).await.unwrap_err();
    assert!(matches!(err, SearchError::InvalidIndex(_)), "{err:?}");
}

// ── Filtering (the authorization seam every backend must honour) ────────────

#[tokio::test]
async fn a_tenant_filter_never_returns_another_tenants_records() {
    let backend = seeded_backend(&[
        tenant_article(1, "Rust at acme", "internal", "acme"),
        tenant_article(2, "Rust at globex", "internal", "globex"),
        article(3, "Rust in public", "no tenant"),
    ])
    .await;
    let def = Article::index_definition();

    let query =
        KeywordQuery::new("rust", page(1, 10)).filter(SearchFilter::default().tenant("acme"));
    let results = backend.keyword_search(&def, &query).await.expect("search");

    assert_eq!(
        results.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        results.total_elements, 1,
        "total must reflect the filtered set"
    );
}

#[tokio::test]
async fn an_allowed_ids_filter_restricts_the_result_set() {
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    let query =
        KeywordQuery::new("rust", page(1, 10)).filter(SearchFilter::default().allow_ids([2_i64]));
    let results = backend.keyword_search(&def, &query).await.expect("search");
    assert_eq!(
        results.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );

    // An *empty* allowlist means "nothing is visible", not "no filter".
    let none = KeywordQuery::new("rust", page(1, 10))
        .filter(SearchFilter::default().allow_ids(Vec::<i64>::new()));
    let results = backend.keyword_search(&def, &none).await.expect("search");
    assert!(results.content.is_empty());
    assert_eq!(results.total_elements, 0);
}

#[tokio::test]
async fn an_excluded_ids_filter_removes_specific_records() {
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    let query =
        KeywordQuery::new("rust", page(1, 10)).filter(SearchFilter::default().exclude_ids([1_i64]));
    let results = backend.keyword_search(&def, &query).await.expect("search");
    assert_eq!(
        results.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );
}

// ── Vector search ───────────────────────────────────────────────────────────

fn vector_doc(id: i64, embedding: [f32; 3]) -> IndexedDocument {
    IndexedDocument::new(article(id, "t", "b").search_document()).with_embedding(embedding.to_vec())
}

#[tokio::test]
async fn vector_search_returns_the_nearest_neighbours_by_cosine_similarity() {
    let backend = MemorySearchBackend::new();
    let def = Article::index_definition();
    backend.ensure_index(&def).await.expect("ensure");
    backend
        .index(
            &def,
            &[
                vector_doc(1, [1.0, 0.0, 0.0]),
                vector_doc(2, [0.9, 0.1, 0.0]),
                vector_doc(3, [0.0, 1.0, 0.0]),
            ],
        )
        .await
        .expect("index");

    let hits = backend
        .vector_search(&def, &VectorQuery::new(vec![1.0, 0.0, 0.0], 2))
        .await
        .expect("vector search");

    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 2]);
    assert!(hits[0].score > hits[1].score);
    assert!(
        (hits[0].score - 1.0).abs() < 1e-5,
        "score {}",
        hits[0].score
    );
}

#[tokio::test]
async fn vector_search_honours_the_filter_and_the_minimum_score() {
    let backend = MemorySearchBackend::new();
    let def = Article::index_definition();
    backend.ensure_index(&def).await.expect("ensure");
    backend
        .index(
            &def,
            &[
                vector_doc(1, [1.0, 0.0, 0.0]),
                vector_doc(2, [0.0, 1.0, 0.0]),
            ],
        )
        .await
        .expect("index");

    let filtered = backend
        .vector_search(
            &def,
            &VectorQuery::new(vec![1.0, 0.0, 0.0], 10)
                .filter(SearchFilter::default().allow_ids([2_i64])),
        )
        .await
        .expect("filtered");
    assert_eq!(filtered.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);

    let thresholded = backend
        .vector_search(
            &def,
            &VectorQuery::new(vec![1.0, 0.0, 0.0], 10).min_score(0.5),
        )
        .await
        .expect("thresholded");
    assert_eq!(
        thresholded.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );
}

#[tokio::test]
async fn documents_without_an_embedding_are_invisible_to_vector_search() {
    let backend = seeded_backend(&corpus()).await;
    let def = Article::index_definition();

    let hits = backend
        .vector_search(&def, &VectorQuery::new(vec![1.0, 0.0, 0.0], 10))
        .await
        .expect("vector search");
    assert!(hits.is_empty());
}

#[tokio::test]
async fn a_dimension_mismatch_is_rejected_rather_than_silently_scored() {
    let backend = MemorySearchBackend::new();
    let def = Article::index_definition();
    backend.ensure_index(&def).await.expect("ensure");
    backend
        .index(&def, &[vector_doc(1, [1.0, 0.0, 0.0])])
        .await
        .expect("index");

    let err = backend
        .vector_search(&def, &VectorQuery::new(vec![1.0, 0.0], 10))
        .await
        .unwrap_err();
    assert!(
        matches!(err, SearchError::DimensionMismatch { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn vector_search_on_an_index_without_an_embed_field_is_a_typed_error() {
    let backend = MemorySearchBackend::new();
    let mut def = Article::index_definition();
    def.embed_field = None;
    backend.ensure_index(&def).await.expect("ensure");

    let err = backend
        .vector_search(&def, &VectorQuery::new(vec![1.0], 1))
        .await
        .unwrap_err();
    assert!(
        matches!(err, SearchError::VectorUnsupported { .. }),
        "{err:?}"
    );
}
