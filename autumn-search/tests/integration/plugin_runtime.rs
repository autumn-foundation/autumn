//! The plugin's **runtime** wiring, driven through `autumn-web`'s in-process
//! test client.
//!
//! AC 8 names the in-process test client specifically, and for a reason: the
//! other suites drive `SearchClient` directly, which proves the *engine* works
//! but proves nothing about the seam an application actually touches — the
//! startup hook, the installed extension, and the `#[job]` handlers. `TestApp`
//! runs a plugin's startup hooks and `TestClient` records and executes
//! enqueued jobs, so this suite closes exactly that gap.
//!
//! Everything here asserts behaviour rather than shape: each test would fail
//! if the production wiring it names were deleted.
//!
//! `TestApp::build` installs the **process-global** job client, and the
//! enqueue helpers resolve through it. Every test here therefore serializes on
//! `job::global_job_runtime_test_lock()` and clears the client first —
//! including the ones that never enqueue, because otherwise *their* `build()`
//! would swap the client out from under a test that is mid-assertion. Same
//! convention as `autumn/tests/integration/job_recorder_integration.rs`.

use std::sync::Arc;

use autumn_search::{
    BackfillOptions, MemoryDocumentSource, MemorySearchBackend, REINDEX_JOB, ReindexArgs,
    SearchClient, SearchConfig, SearchPlugin,
};
use autumn_web::job;
use autumn_web::pagination::PageRequest;
use autumn_web::test::TestApp;

use super::support::{Article, article};

/// Build a test app with the plugin installed over an in-memory backend and a
/// document source seeded with `articles`.
fn app_with(
    articles: &[Article],
    config: SearchConfig,
) -> (autumn_web::test::TestClient, Arc<MemoryDocumentSource>) {
    let source = Arc::new(MemoryDocumentSource::new());
    for record in articles {
        source.upsert(record);
    }
    let client = TestApp::new()
        .plugin(
            SearchPlugin::new()
                .config(config)
                .backend(Arc::new(MemorySearchBackend::new()))
                .source(source.clone())
                .index::<Article>(),
        )
        .build();
    (client, source)
}

fn corpus() -> Vec<Article> {
    vec![
        article(1, "Rust web frameworks", "A survey."),
        article(2, "Gardening", "Tomatoes."),
    ]
}

/// The `SearchClient` the plugin's startup hook installed on `AppState`.
fn installed_client(test: &autumn_web::test::TestClient) -> Arc<SearchClient> {
    test.state()
        .extension::<SearchClient>()
        .expect("the plugin's startup hook must install a SearchClient extension")
}

#[tokio::test]
async fn the_startup_hook_installs_a_client_whose_indexes_are_ready() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Not "does the builder hold an index" — does the *app* come up with a
    // client that has already created its storage.
    let (test, _source) = app_with(&corpus(), SearchConfig::default());
    let search = installed_client(&test);

    assert_eq!(search.index_names(), vec!["search_articles"]);

    // `ensure_indexes` ran during startup, so indexing works with no further
    // setup — if the hook had skipped it, an unknown-index error would surface
    // here rather than at boot.
    search
        .index_record(&article(1, "Rust web frameworks", "A survey."))
        .await
        .expect("index");
    let page = search
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );
}

#[tokio::test]
async fn a_lifecycle_reindex_flows_through_the_job_queue_into_the_index() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // The whole of AC3 in one test: an enqueue (as `SearchSyncHooks` performs
    // it from `after_*_commit`) is recorded on the queue, and *running the
    // registered job handler* — not calling the client directly — is what puts
    // the record in the index.
    let (test, _source) = app_with(&corpus(), SearchConfig::default());
    let search = installed_client(&test);

    assert!(
        search
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .content
            .is_empty(),
        "nothing is indexed before the job runs"
    );

    autumn_search::enqueue_reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("enqueue");
    test.assert_job_enqueued_with(
        REINDEX_JOB,
        serde_json::json!({"index": "search_articles", "id": 1, "op": "upsert"}),
    );

    let performed = test.perform_enqueued_jobs().await;
    performed.assert_all_succeeded();

    let page = search
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1],
        "running the registered job handler must index the record"
    );
}

#[tokio::test]
async fn a_delete_instruction_removes_the_record_through_the_same_job() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let (test, source) = app_with(&corpus(), SearchConfig::default());
    let search = installed_client(&test);

    autumn_search::enqueue_reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("enqueue");
    test.perform_enqueued_jobs().await.assert_all_succeeded();

    // A real delete removes the row first; the hook then enqueues the
    // instruction. Both ops re-read, so the *row's* absence is what removes
    // the document — which is precisely what stops a stale delete from
    // evicting a record that has since been recreated.
    source.remove(1);
    autumn_search::enqueue_reindex(&ReindexArgs::delete("search_articles", 1))
        .await
        .expect("enqueue");
    test.assert_job_enqueued_with(
        REINDEX_JOB,
        serde_json::json!({"index": "search_articles", "id": 1, "op": "delete"}),
    );
    test.perform_enqueued_jobs().await.assert_all_succeeded();

    assert!(
        search
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .content
            .is_empty()
    );
}

#[tokio::test]
async fn a_reindex_for_a_row_that_is_gone_converges_via_the_job_handler() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Drift repair, end to end: the source loses the row, the ordinary upsert
    // job runs, and the document goes away.
    let (test, source) = app_with(&corpus(), SearchConfig::default());
    let search = installed_client(&test);

    autumn_search::enqueue_reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("enqueue");
    test.perform_enqueued_jobs().await.assert_all_succeeded();
    assert_eq!(
        search
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .content
            .len(),
        1
    );

    source.remove(1);
    autumn_search::enqueue_reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("enqueue");
    test.perform_enqueued_jobs().await.assert_all_succeeded();

    assert!(
        search
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .content
            .is_empty()
    );
}

#[tokio::test]
async fn a_malformed_job_payload_fails_the_job_rather_than_being_ignored() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let (test, _source) = app_with(&corpus(), SearchConfig::default());

    autumn_web::job::enqueue(REINDEX_JOB, serde_json::json!({"nonsense": true}))
        .await
        .expect("enqueue");
    let performed = test.perform_enqueued_jobs().await;

    let failures = performed.failures();
    assert_eq!(failures.len(), 1, "a bad payload must fail, not no-op");
    assert!(
        failures[0]
            .1
            .to_string()
            .contains("invalid reindex payload"),
        "{:?}",
        failures[0].1
    );
}

// ── The kill switch ─────────────────────────────────────────────────────────

#[tokio::test]
async fn disabling_search_in_config_makes_index_writes_and_queries_no_ops() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let disabled = SearchConfig::from_toml_str("[search]\nenabled = false\n").expect("parse");
    let (test, _source) = app_with(&corpus(), disabled);
    let search = installed_client(&test);

    assert!(!search.is_enabled());

    search
        .index_record(&article(1, "Rust", "a rust framework"))
        .await
        .expect("index write must no-op, not error");
    let page = search
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert!(page.content.is_empty());
    assert_eq!(page.total_elements, 0);
}

#[tokio::test]
async fn disabling_search_stops_a_backfill_including_a_purging_one() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // The most destructive write there is. A queued backfill job that outlives
    // the decision to turn search off must not still empty the index.
    let (test, _source) = app_with(&corpus(), SearchConfig::default());
    let enabled = installed_client(&test);
    enabled
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .expect("backfill");
    assert_eq!(
        enabled
            .search::<Article>("rust", &PageRequest::default())
            .await
            .expect("search")
            .total_elements,
        1
    );

    let disabled = SearchConfig::from_toml_str("[search]\nenabled = false\n").expect("parse");
    let (test, _source) = app_with(&corpus(), disabled);
    let search = installed_client(&test);

    let report = search
        .backfill("search_articles", &BackfillOptions::default().purge(true))
        .await
        .expect("backfill must no-op, not error");
    assert_eq!(report.indexed, 0);
    assert!(!report.purged, "a disabled client must not clear the index");
}

#[tokio::test]
async fn a_disabled_client_no_ops_an_in_flight_job_for_an_unregistered_index() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Ordering matters: checking the index registry first would make this
    // `UnknownIndex`, so the job would retry to exhaustion and dead-letter
    // instead of quietly doing nothing.
    let disabled = SearchConfig::from_toml_str("[search]\nenabled = false\n").expect("parse");
    let (test, _source) = app_with(&corpus(), disabled);
    let search = installed_client(&test);

    search
        .reindex(&ReindexArgs::upsert("an_index_that_is_gone", 1))
        .await
        .expect("a disabled client must no-op rather than fail");
}

// ── Configuration reaches the runtime ───────────────────────────────────────

#[tokio::test]
async fn the_configured_batch_size_reaches_the_client() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let config = SearchConfig::from_toml_str("[search]\nbatch_size = 7\n").expect("parse");
    let (test, _source) = app_with(&corpus(), config);
    assert_eq!(installed_client(&test).default_batch_size(), 7);
}

#[tokio::test]
async fn the_configured_queue_reaches_the_registered_jobs() {
    let config = SearchConfig::from_toml_str("[search]\nqueue = \"indexing\"\n").expect("parse");
    let infos = autumn_search::search_job_infos(&config.queue);
    assert!(infos.iter().all(|info| info.queue == "indexing"));
    assert_eq!(infos.len(), 2);
}

// ── The kill switch beats the authorization hook ────────────────────────────

#[tokio::test]
async fn disabled_search_returns_an_empty_page_even_with_no_visibility_hook() {
    // Ordering matters: consulting `SearchVisibility` first would turn the
    // documented empty page into `VisibilityUnavailable`, so the kill switch
    // would be ineffective on precisely the authorization-aware endpoints an
    // app is most likely to have.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let disabled = SearchConfig::from_toml_str("[search]\nenabled = false\n").expect("parse");
    let (test, _source) = app_with(&corpus(), disabled);
    let search = installed_client(&test);
    let ctx = super::support::policy_context(Some("alice")).await;

    let page = search
        .search_for::<Article>(&ctx, "rust", &PageRequest::default())
        .await
        .expect("a disabled search must not demand a visibility hook");
    assert!(page.content.is_empty());
    assert_eq!(page.total_elements, 0);

    // The two semantic entry points have the same ordering.
    assert!(
        search
            .similar_for::<Article>(&ctx, "rust", 5)
            .await
            .expect("similar_for")
            .is_empty()
    );
    assert!(
        search
            .similar_to_for::<Article>(&ctx, 1, 5)
            .await
            .expect("similar_to_for")
            .is_empty()
    );
}

// ── Declared width vs installed embedder ────────────────────────────────────

/// Build an app whose declared `embedding_dimensions` may disagree with the
/// installed embedder.
fn app_with_widths(
    declared: Option<usize>,
    embedder: Arc<dyn autumn_search::Embedder>,
) -> autumn_web::test::TestClient {
    let mut config = SearchConfig::default();
    config.embedding_dimensions = declared;
    TestApp::new()
        .plugin(
            SearchPlugin::new()
                .config(config)
                .backend(Arc::new(MemorySearchBackend::new()))
                .embedder(embedder)
                .index::<Article>(),
        )
        .build()
}

#[tokio::test]
#[should_panic(expected = "does not match the installed embedder")]
async fn a_declared_width_that_disagrees_with_the_embedder_fails_at_boot() {
    // The two are set in different places — `[search] embedding_dimensions` in
    // config, `Embedder::dimensions()` in code — and the failure mode is
    // silent: every index write reports success, the vector column rejects the
    // wrong-width value, and semantic search just returns nothing forever. A
    // refused boot is the only version of this an operator can act on.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let _ = app_with_widths(Some(128), Arc::new(autumn_search::HashingEmbedder::new(64)));
}

#[tokio::test]
async fn a_matching_width_boots_and_a_declared_width_without_an_embedder_is_allowed() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let test = app_with_widths(Some(64), Arc::new(autumn_search::HashingEmbedder::new(64)));
    assert_eq!(installed_client(&test).embedding_dimensions(), 64);

    // Declaring a width before installing a provider is a legitimate ordering:
    // `NoEmbedder` reports `0`, nothing vector-shaped runs, and the declared
    // width is simply what the backend will use once a provider arrives.
    let test = app_with_widths(Some(768), Arc::new(autumn_search::NoEmbedder));
    assert_eq!(installed_client(&test).embedding_dimensions(), 0);
}
