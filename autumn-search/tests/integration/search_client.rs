//! `SearchClient` — the one engine-agnostic query API.
//!
//! AC: "One engine-agnostic query API returns ranked, paginated results
//! (reusing `Page`/`ListQuery`) for keyword search; and a vector/semantic
//! query (k-NN / similarity) for embedded fields."

use std::sync::Arc;

use autumn_search::{
    HashingEmbedder, MemorySearchBackend, NoEmbedder, SearchClient, SearchError, SearchFilter,
};
use autumn_web::pagination::{ListQuery, PageRequest, SortDir};

use super::support::{Article, TenantArticle, article, tenant_article, with_tenant};

async fn client(articles: &[Article]) -> SearchClient {
    let client = SearchClient::builder()
        .backend(Arc::new(MemorySearchBackend::new()))
        .embedder(Arc::new(HashingEmbedder::new(128)))
        .index::<Article>()
        .index::<TenantArticle>()
        .build();
    client.ensure_indexes().await.expect("ensure indexes");
    for a in articles {
        client.index_record(a).await.expect("index record");
    }
    client
}

/// A client seeded with tenant-scoped records.
async fn tenant_client(articles: &[TenantArticle]) -> SearchClient {
    let client = SearchClient::builder()
        .backend(Arc::new(MemorySearchBackend::new()))
        .embedder(Arc::new(HashingEmbedder::new(128)))
        .index::<TenantArticle>()
        .build();
    client.ensure_indexes().await.expect("ensure indexes");
    for a in articles {
        client.index_record(a).await.expect("index record");
    }
    client
}

fn corpus() -> Vec<Article> {
    vec![
        article(1, "Rust web frameworks", "A survey of the ecosystem."),
        article(2, "Getting started", "Autumn is a rust web framework."),
        article(3, "Gardening", "Tomatoes and basil."),
    ]
}

// ── Keyword ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_returns_a_ranked_page_of_hits() {
    let client = client(&corpus()).await;

    let page = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");

    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(page.total_elements, 2);
    assert!(page.content.iter().all(|h| h.index == "search_articles"));
}

#[tokio::test]
async fn search_accepts_a_list_query_so_it_composes_with_existing_endpoints() {
    // `ListQuery` carries the request's `filter[...]` keys; `PageRequest`
    // carries the paging. `search_list` reuses both exactly as a generated
    // `list()` endpoint does — search drops into an existing index handler
    // without a second query-parameter vocabulary.
    let client = tenant_client(&[
        tenant_article(1, "Rust at acme", "rust framework", "acme"),
        tenant_article(2, "Rust at globex", "rust framework", "globex"),
    ])
    .await;

    let list = ListQuery::new(None, SortDir::Asc, &[("tenant_id", "globex")]);
    let page = with_tenant("globex", async {
        client
            .search_list::<TenantArticle>("rust", &list, &PageRequest::default())
            .await
    })
    .await
    .expect("search_list");

    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(page.total_elements, 1);
    assert_eq!(page.page, 1);
}

#[tokio::test]
async fn search_list_paginates_through_the_page_request() {
    let client = client(&corpus()).await;

    let page = client
        .search_list::<Article>("rust", &ListQuery::default(), &PageRequest::new(2, 1))
        .await
        .expect("search_list");

    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(page.total_elements, 2);
    assert_eq!(page.page, 2);
    assert_eq!(page.size, 1);
}

#[tokio::test]
async fn a_list_query_filter_on_an_indexed_field_narrows_the_results() {
    let client = client(&corpus()).await;

    let list = ListQuery::new(None, SortDir::Asc, &[("title", "Getting started")]);
    let page = client
        .search_list::<Article>("rust", &list, &PageRequest::default())
        .await
        .expect("search_list");

    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );
}

#[tokio::test]
async fn a_list_query_filter_on_an_unknown_key_is_ignored_like_list() {
    // `list()` ignores non-allowlisted filter keys rather than erroring;
    // search matches that posture so a shared query string works on both.
    let client = client(&corpus()).await;

    let list = ListQuery::new(None, SortDir::Asc, &[("nope", "whatever")]);
    let page = client
        .search_list::<Article>("rust", &list, &PageRequest::default())
        .await
        .expect("search_list");

    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn hydrating_a_page_preserves_rank_order_and_pagination_metadata() {
    // The client returns hits; turning them into records is the app's loader,
    // and the loader must not be allowed to reorder or lose the ranking.
    let client = client(&corpus()).await;

    let page = client
        .search_hydrated::<Article, _, _>("rust", &PageRequest::default(), |ids| async move {
            // Deliberately return them in the *wrong* order.
            let mut records: Vec<Article> = ids
                .iter()
                .rev()
                .map(|id| article(*id, "title", "body"))
                .collect();
            records.sort_by_key(|a| a.id);
            Ok(records)
        })
        .await
        .expect("hydrated search");

    assert_eq!(
        page.content.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![1, 2],
        "hydration must re-apply the ranked order"
    );
    assert_eq!(page.total_elements, 2);
}

#[tokio::test]
async fn hydration_drops_records_the_loader_could_not_return() {
    // A hit whose record vanished between the search and the load must not
    // become a hole or a panic — it is simply absent, and it is subtracted
    // from the total so the pager cannot advertise a record the caller never
    // received. (That total is damage limitation, not authorization: filtering
    // in the loader still leaks a count. `search_for` is the authorization
    // path.)
    let client = client(&corpus()).await;

    let page = client
        .search_hydrated::<Article, _, _>("rust", &PageRequest::default(), |ids| async move {
            Ok(ids
                .iter()
                .filter(|id| **id == 2)
                .map(|id| article(*id, "t", "b"))
                .collect::<Vec<_>>())
        })
        .await
        .expect("hydrated search");

    assert_eq!(
        page.content.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        page.total_elements, 1,
        "a record the loader could not produce is not counted"
    );
}

// ── Vector / semantic ───────────────────────────────────────────────────────

#[tokio::test]
async fn similar_embeds_the_query_text_and_returns_nearest_neighbours() {
    let client = client(&[
        article(1, "Rust", "rust web framework for building apis"),
        article(2, "Rust again", "a rust web framework"),
        article(3, "Garden", "tomato basil gardening in summer"),
    ])
    .await;

    let hits = client
        .similar::<Article>("rust web framework", 2)
        .await
        .expect("similar");

    assert_eq!(hits.len(), 2);
    assert!(
        hits.iter().all(|h| h.id != 3),
        "the unrelated record must not be a nearest neighbour: {hits:?}"
    );
}

#[tokio::test]
async fn similar_to_an_existing_record_finds_its_neighbours() {
    let client = client(&[
        article(1, "Rust", "rust web framework for building apis"),
        article(2, "Rust again", "a rust web framework"),
        article(3, "Garden", "tomato basil gardening in summer"),
    ])
    .await;

    let hits = client
        .similar_to::<Article>(1, 5)
        .await
        .expect("similar_to");

    assert!(
        !hits.iter().any(|h| h.id == 1),
        "a record is not its own neighbour: {hits:?}"
    );
    assert_eq!(hits.first().map(|h| h.id), Some(2));
}

#[tokio::test]
async fn vector_search_without_an_embedder_is_a_typed_error_not_a_wrong_answer() {
    let client = SearchClient::builder()
        .backend(Arc::new(MemorySearchBackend::new()))
        .embedder(Arc::new(NoEmbedder))
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");

    let err = client.similar::<Article>("anything", 3).await.unwrap_err();
    assert!(matches!(err, SearchError::EmbedderUnavailable), "{err:?}");
}

// ── Registration & lifecycle ────────────────────────────────────────────────

#[tokio::test]
async fn querying_an_unregistered_index_is_a_typed_error() {
    let client = SearchClient::builder()
        .backend(Arc::new(MemorySearchBackend::new()))
        .build();

    let err = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, SearchError::UnknownIndex(ref n) if n == "search_articles"),
        "{err:?}"
    );
}

#[tokio::test]
async fn removing_a_record_takes_it_out_of_the_index() {
    let client = client(&corpus()).await;

    client.remove::<Article>(1).await.expect("remove");

    let page = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );
}

#[tokio::test]
async fn a_record_update_changes_recall_with_no_manual_reindex_call() {
    // This is the issue's falsifiable success metric, at the client level:
    // re-indexing the mutated record (which the lifecycle does automatically)
    // must change what the query returns.
    let client = client(&corpus()).await;

    let mut record = article(1, "Rust web frameworks", "A survey of the ecosystem.");
    record.title = "Gardening weekly".to_owned();
    record.body = "Tomatoes only.".to_owned();
    client.index_record(&record).await.expect("reindex");

    let rust = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(
        rust.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );

    let tomatoes = client
        .search::<Article>("tomatoes", &PageRequest::default())
        .await
        .expect("search");
    assert!(tomatoes.content.iter().any(|h| h.id == 1));
}

#[tokio::test]
async fn an_explicit_filter_is_passed_through_to_the_backend() {
    let client = tenant_client(&[
        tenant_article(1, "Rust at acme", "internal", "acme"),
        tenant_article(2, "Rust at globex", "internal", "globex"),
    ])
    .await;

    let page = with_tenant("acme", async {
        client
            .search_filtered::<TenantArticle>(
                "rust",
                &PageRequest::default(),
                SearchFilter::default().tenant("acme"),
            )
            .await
    })
    .await
    .expect("search");

    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );
}
