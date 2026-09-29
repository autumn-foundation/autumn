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

// ── A source that returns short batches ─────────────────────────────────────

/// A `DocumentSource` that reads a fixed window of `limit` rows and then drops
/// the ones that do not qualify — so it returns **up to** `limit` documents,
/// as the trait allows, and a short batch does not mean "no more rows".
///
/// This is not contrived: any source that filters after reading behaves this
/// way — soft-deleted rows, rows failing a tenant check, rows whose embed text
/// is empty. Here, even ids are the ones dropped.
struct FilteringSource {
    documents: Vec<autumn_web::search::SearchDocument>,
}

impl autumn_search::DocumentSource for FilteringSource {
    fn fetch<'a>(
        &'a self,
        _definition: &'a autumn_search::IndexDefinition,
        ids: &'a [i64],
    ) -> autumn_search::BoxFuture<
        'a,
        autumn_search::SearchResult<Vec<autumn_web::search::SearchDocument>>,
    > {
        Box::pin(async move {
            Ok(self
                .documents
                .iter()
                .filter(|document| ids.contains(&document.id) && document.id % 2 == 1)
                .cloned()
                .collect())
        })
    }

    fn scan<'a>(
        &'a self,
        _definition: &'a autumn_search::IndexDefinition,
        after: Option<i64>,
        limit: usize,
    ) -> autumn_search::BoxFuture<
        'a,
        autumn_search::SearchResult<Vec<autumn_web::search::SearchDocument>>,
    > {
        Box::pin(async move {
            let after = after.unwrap_or(0);
            Ok(self
                .documents
                .iter()
                .filter(|document| document.id > after)
                // Read the window first…
                .take(limit)
                // …then drop what does not qualify. The batch is short, but
                // rows remain beyond it.
                .filter(|document| document.id % 2 == 1)
                .cloned()
                .collect())
        })
    }
}

#[tokio::test]
async fn a_short_batch_does_not_end_the_backfill() {
    use autumn_web::search::SearchIndexed as _;

    // 30 rows, half of which the source filters out. With a batch size of 10,
    // every batch comes back short (5 of 10) while 20 more rows still remain.
    // Treating "short" as "exhausted" indexes 5 records and reports success —
    // a silently truncated rebuild, which is the worst kind.
    let source = Arc::new(FilteringSource {
        documents: (1..=30)
            .map(|id| article(id, &format!("Record {id}"), "rust web framework").search_document())
            .collect(),
    });
    let client = SearchClient::builder()
        .backend(Arc::new(MemorySearchBackend::new()))
        .source(source)
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");

    let report = client
        .backfill(
            "search_articles",
            &BackfillOptions::default().batch_size(10),
        )
        .await
        .expect("backfill");

    assert_eq!(
        report.indexed, 15,
        "every odd id in 1..=30 must be indexed, not just the first batch"
    );
    let page = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(page.total_elements, 15);
}

// ── Backfill versus a concurrent reindex ────────────────────────────────────

#[tokio::test]
async fn a_backfill_does_not_overwrite_a_reindex_that_ran_while_it_was_working() {
    // The window is real and wide: a backfill reads a batch, embeds it (a
    // provider round-trip), then writes. A mutation in between enqueues a
    // reindex job that reads and writes the NEW state — and the backfill's
    // in-flight batch would then put back what the source said minutes ago.
    // Nothing re-runs the reindex, so the index stays wrong until the next
    // mutation touches that record.
    use autumn_search::{IndexedDocument, SearchBackend as _};
    use autumn_web::search::SearchIndexed as _;

    let backend = Arc::new(MemorySearchBackend::new());
    let source = Arc::new(MemoryDocumentSource::new());
    source.upsert(&article(1, "Stale title", "rust web framework"));

    let client = SearchClient::builder()
        .backend(backend.clone())
        .source(source.clone())
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");

    let definition = Article::index_definition();

    // Simulate the interleaving directly: take the watermark the backfill
    // would take, then let a reindex land, then replay the backfill's write.
    let watermark = backend.write_watermark().await.expect("watermark");

    // …the mutation's reindex job writes the new state.
    let fresh = article(1, "Fresh title", "rust web framework");
    client.index_record(&fresh).await.expect("reindex");

    // …and now the backfill's stale batch arrives.
    let stale =
        IndexedDocument::new(article(1, "Stale title", "rust web framework").search_document());
    backend
        .index_unless_newer(&definition, &[stale], watermark.as_deref())
        .await
        .expect("backfill write");

    let page = client
        .search::<Article>("fresh", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(
        page.total_elements, 1,
        "the reindex's newer document must survive the backfill batch"
    );
    assert_eq!(
        client
            .search::<Article>("stale", &PageRequest::default())
            .await
            .expect("search")
            .total_elements,
        0,
        "the backfill must not have put the stale title back"
    );
}

#[tokio::test]
async fn a_backfill_still_writes_documents_nothing_else_touched() {
    // The guard must only skip records a newer writer claimed — everything
    // else still has to be indexed, or the watermark would turn every backfill
    // after the first into a no-op.
    let (client, _source) = client_with(5).await;
    let report = client
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .expect("backfill");
    assert_eq!(report.indexed, 5);

    let second = client
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .expect("second backfill");
    assert_eq!(
        second.indexed, 5,
        "a later backfill re-reads the source and rewrites every row"
    );
    assert_eq!(
        client
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .total_elements,
        5
    );
}

#[tokio::test]
async fn a_backfill_does_not_resurrect_a_record_deleted_while_it_was_working() {
    // The other half of the race, and the worse half. The watermark's UPDATE
    // guard only protects a document that still exists — if the reindex
    // DELETED it, the backfill's write finds nothing to conflict with and
    // inserts unconditionally. A record someone deleted becomes searchable
    // again, and stays that way until another mutation touches it.
    use autumn_search::{IndexedDocument, SearchBackend as _};
    use autumn_web::search::SearchIndexed as _;

    let backend = Arc::new(MemorySearchBackend::new());
    let client = SearchClient::builder()
        .backend(backend.clone())
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");

    let definition = Article::index_definition();
    let record = article(1, "Secret", "rust web framework");
    client.index_record(&record).await.expect("index");

    // The backfill scans and takes its watermark…
    let watermark = backend.write_watermark().await.expect("watermark");

    // …the record is deleted, and the reindex removes its document…
    client.remove::<Article>(1).await.expect("remove");
    assert_eq!(
        client
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .total_elements,
        0
    );

    // …and now the backfill's stale batch arrives.
    let stale = IndexedDocument::new(record.search_document());
    backend
        .index_unless_newer(&definition, &[stale], watermark.as_deref())
        .await
        .expect("backfill write");

    assert_eq!(
        client
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .total_elements,
        0,
        "a deleted record must not come back because a backfill was mid-flight"
    );
}

#[tokio::test]
async fn a_purge_clears_the_delete_record_so_the_rebuild_is_not_blocked() {
    // The tombstone must not outlive a deliberate reset: a purging backfill is
    // *supposed* to rewrite everything, so leaving delete records behind would
    // make it silently skip whatever had been deleted before.
    let backend = Arc::new(MemorySearchBackend::new());
    let source = Arc::new(MemoryDocumentSource::new());
    source.upsert(&article(1, "Rust", "rust web framework"));

    let client = SearchClient::builder()
        .backend(backend.clone())
        .source(source.clone())
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");

    client
        .index_record(&article(1, "Rust", "x"))
        .await
        .expect("index");
    client.remove::<Article>(1).await.expect("remove");

    let report = client
        .backfill("search_articles", &BackfillOptions::default().purge(true))
        .await
        .expect("backfill");
    assert_eq!(report.indexed, 1);
    assert_eq!(
        client
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .total_elements,
        1,
        "the source still has the row, so a purging rebuild must index it"
    );
}
