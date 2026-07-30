//! Full reindex / backfill.
//!
//! AC: "… and a **full reindex/backfill** command exists for bootstrapping or
//! schema changes."

use std::sync::Arc;

use autumn_search::{
    BackfillOptions, MemoryDocumentSource, MemorySearchBackend, SearchClient, SearchError,
};
use autumn_web::pagination::PageRequest;

use super::support::{Article, article};

async fn client_with(n: i64) -> (SearchClient, Arc<MemoryDocumentSource>) {
    let source = Arc::new(MemoryDocumentSource::new());
    for id in 1..=n {
        source.upsert(&article(id, &format!("Record {id}"), "rust web framework"));
    }
    let client = SearchClient::builder()
        .backend(Arc::new(MemorySearchBackend::new()))
        .source(source.clone())
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");
    (client, source)
}

#[tokio::test]
async fn backfill_indexes_every_row_in_batches() {
    let (client, _source) = client_with(25).await;

    let report = client
        .backfill(
            "search_articles",
            &BackfillOptions::default().batch_size(10),
        )
        .await
        .expect("backfill");

    assert_eq!(report.indexed, 25);
    assert_eq!(report.batches, 3, "25 rows at 10/batch is 3 batches");
    assert_eq!(report.index, "search_articles");

    let page = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(page.total_elements, 25);
}

#[tokio::test]
async fn backfill_is_idempotent() {
    let (client, _source) = client_with(5).await;

    client
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .expect("first");
    let second = client
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .expect("second");

    assert_eq!(second.indexed, 5);
    let page = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(
        page.total_elements, 5,
        "re-running must not duplicate documents"
    );
}

#[tokio::test]
async fn backfill_can_purge_stale_documents_first() {
    // Bootstrapping after a schema change: `purge` clears documents that the
    // source no longer produces, instead of leaving orphans behind forever.
    let (client, source) = client_with(3).await;
    client
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .expect("initial");

    source.remove(3);

    let without_purge = client
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .expect("no purge");
    assert_eq!(without_purge.indexed, 2);
    assert_eq!(
        client
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .total_elements,
        3,
        "without purge the orphan survives"
    );

    let purged = client
        .backfill("search_articles", &BackfillOptions::default().purge(true))
        .await
        .expect("purge");
    assert_eq!(purged.indexed, 2);
    assert!(purged.purged);
    assert_eq!(
        client
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .total_elements,
        2
    );
}

#[tokio::test]
async fn backfilling_all_indexes_reports_each_one() {
    let (client, _source) = client_with(4).await;

    let reports = client
        .backfill_all(&BackfillOptions::default())
        .await
        .expect("backfill all");

    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].index, "search_articles");
    assert_eq!(reports[0].indexed, 4);
}

#[tokio::test]
async fn backfilling_an_unknown_index_is_a_typed_error() {
    let (client, _source) = client_with(1).await;

    let err = client
        .backfill("nope", &BackfillOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, SearchError::UnknownIndex(_)), "{err:?}");
}

#[tokio::test]
async fn a_zero_batch_size_is_clamped_rather_than_looping_forever() {
    let (client, _source) = client_with(3).await;

    let report = client
        .backfill("search_articles", &BackfillOptions::default().batch_size(0))
        .await
        .expect("backfill");
    assert_eq!(report.indexed, 3);
}

#[tokio::test]
async fn backfill_without_a_document_source_is_a_typed_error() {
    let client = SearchClient::builder()
        .backend(Arc::new(MemorySearchBackend::new()))
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");

    let err = client
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, SearchError::SourceUnavailable), "{err:?}");
}
