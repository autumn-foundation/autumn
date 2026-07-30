//! Lifecycle-driven index sync.
//!
//! AC: "The index stays in sync with the record lifecycle: create/update/delete
//! to a searchable model **enqueues a reindex** (via `#[job]`, so indexing is
//! off-request and durable)."
//!
//! The design under test: a create/update/delete on a searchable model enqueues
//! `ReindexArgs { index, id, op }`. The job handler **re-reads the source row**
//! and converges — present ⇒ upsert, absent ⇒ delete. That single idempotent
//! operation is what makes at-least-once job delivery (and a lost delete event)
//! safe.

use std::sync::Arc;

use autumn_search::{
    MemoryDocumentSource, MemorySearchBackend, ReindexArgs, ReindexOp, SearchClient, SearchError,
};
use autumn_web::pagination::PageRequest;

use super::support::{Article, article};

async fn client_and_source(articles: &[Article]) -> (SearchClient, Arc<MemoryDocumentSource>) {
    let source = Arc::new(MemoryDocumentSource::new());
    for a in articles {
        source.upsert(a);
    }
    let client = SearchClient::builder()
        .backend(Arc::new(MemorySearchBackend::new()))
        .source(source.clone())
        .index::<Article>()
        .build();
    client.ensure_indexes().await.expect("ensure");
    (client, source)
}

fn corpus() -> Vec<Article> {
    vec![
        article(1, "Rust web frameworks", "A survey."),
        article(2, "Gardening", "Tomatoes."),
    ]
}

#[tokio::test]
async fn reindexing_pulls_the_current_row_into_the_index() {
    let (client, _source) = client_and_source(&corpus()).await;

    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("reindex");

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
async fn reindexing_is_idempotent_so_at_least_once_delivery_is_safe() {
    let (client, _source) = client_and_source(&corpus()).await;

    for _ in 0..3 {
        client
            .reindex(&ReindexArgs::upsert("search_articles", 1))
            .await
            .expect("reindex");
    }

    let page = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(page.content.len(), 1);
    assert_eq!(page.total_elements, 1);
}

#[tokio::test]
async fn reindexing_a_row_that_no_longer_exists_removes_it_from_the_index() {
    // Drift repair: a delete event that was lost, or a row removed by direct
    // SQL, converges to the same state as an explicit delete.
    let (client, source) = client_and_source(&corpus()).await;
    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("index it");

    source.remove(1);
    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("reindex after delete");

    let page = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert!(page.content.is_empty());
}

#[tokio::test]
async fn an_explicit_delete_op_never_re_reads_the_source() {
    // A delete must not race a re-created row back into the index, and must
    // work even when the source is unavailable.
    let (client, source) = client_and_source(&corpus()).await;
    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("index");

    client
        .reindex(&ReindexArgs::delete("search_articles", 1))
        .await
        .expect("delete");
    assert_eq!(
        source.fetch_calls_for(1),
        1,
        "the delete op must not re-read the row"
    );

    let page = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert!(page.content.is_empty());
}

#[tokio::test]
async fn a_record_update_changes_ranking_without_any_manual_reindex_call() {
    // The falsifiable success metric from the issue.
    let (client, source) = client_and_source(&corpus()).await;
    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("initial");

    source.upsert(&article(1, "Gardening weekly", "Tomatoes only."));
    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("lifecycle reindex");

    assert!(
        client
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .content
            .is_empty()
    );
    assert!(
        client
            .search::<Article>("tomatoes", &PageRequest::default())
            .await
            .expect("search")
            .content
            .iter()
            .any(|h| h.id == 1)
    );
}

#[tokio::test]
async fn reindexing_an_unknown_index_is_a_typed_error_not_a_silent_drop() {
    let (client, _source) = client_and_source(&corpus()).await;

    let err = client
        .reindex(&ReindexArgs::upsert("not_an_index", 1))
        .await
        .unwrap_err();
    assert!(matches!(err, SearchError::UnknownIndex(_)), "{err:?}");
}

#[tokio::test]
async fn reindex_args_round_trip_through_the_job_payload() {
    // The args cross a process boundary on the durable job queue.
    let args = ReindexArgs::delete("search_articles", 42);
    let json = serde_json::to_value(&args).expect("serialize");
    let back: ReindexArgs = serde_json::from_value(json).expect("deserialize");
    assert_eq!(back, args);
    assert_eq!(back.op, ReindexOp::Delete);
}

#[tokio::test]
async fn the_reindex_job_is_registered_with_a_stable_name() {
    // The job name is a wire contract: in-flight payloads outlive a deploy.
    let infos = autumn_search::search_job_infos("search");
    let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"autumn_search_reindex"), "{names:?}");
    assert!(names.contains(&"autumn_search_backfill"), "{names:?}");
    assert!(infos.iter().all(|i| i.queue == "search"));
}
