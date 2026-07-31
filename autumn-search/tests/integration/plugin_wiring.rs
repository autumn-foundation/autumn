//! `SearchPlugin` wiring.
//!
//! AC: "A new plugin crate (`autumn-search`) registers via the `Plugin` seam
//! (its own config + startup wiring), mounted with one builder call like the
//! other plugins."

use std::sync::Arc;

use autumn_search::{
    HashingEmbedder, MemorySearchBackend, SearchClient, SearchConfig, SearchPlugin,
};

use super::support::Article;

#[test]
fn the_plugin_mounts_with_one_builder_call() {
    let app = autumn_web::app().plugin(
        SearchPlugin::new()
            .backend(Arc::new(MemorySearchBackend::new()))
            .embedder(Arc::new(HashingEmbedder::new(64)))
            .index::<Article>(),
    );

    assert!(app.has_plugin("autumn-search"));
}

#[test]
fn the_plugin_declares_its_own_config_section() {
    // Without this a host app running `server.strict_config = true` would fail
    // to boot with `unknown key "search"`.
    let app = autumn_web::app().plugin(SearchPlugin::new().index::<Article>());
    assert!(app.has_config_section("search"));
}

#[test]
fn registering_the_plugin_twice_is_a_no_op() {
    let app = autumn_web::app()
        .plugin(SearchPlugin::new().index::<Article>())
        .plugin(SearchPlugin::new().index::<Article>());
    assert!(app.has_plugin("autumn-search"));
}

#[test]
fn the_plugin_registers_its_reindex_and_backfill_jobs_on_the_configured_queue() {
    let infos = autumn_search::search_job_infos("indexing");
    assert_eq!(infos.len(), 2);
    assert!(infos.iter().all(|i| i.queue == "indexing"));
}

#[test]
fn the_client_honours_a_postgres_request() {
    // `postgres()` only records the intent — the store is created at build
    // time. If `client()` did not resolve it, this would silently hand back an
    // isolated in-memory index instead of the configured persistent backend.
    let plugin = SearchPlugin::new().postgres().index::<Article>();
    assert!(plugin.uses_postgres());
    assert_eq!(plugin.client().backend().name(), "postgres");

    // And an explicit backend still wins, without leaving the Postgres store
    // behind as a pool-less document source.
    let plugin = SearchPlugin::new()
        .postgres()
        .backend(Arc::new(MemorySearchBackend::new()))
        .index::<Article>();
    assert!(!plugin.uses_postgres());
    assert_eq!(plugin.client().backend().name(), "memory");
}

#[tokio::test]
async fn the_plugin_exposes_a_ready_to_use_client() {
    let plugin = SearchPlugin::new()
        .backend(Arc::new(MemorySearchBackend::new()))
        .embedder(Arc::new(HashingEmbedder::new(32)))
        .index::<Article>();

    let client: SearchClient = plugin.client();
    client.ensure_indexes().await.expect("ensure");
    assert!(client.index_definition("search_articles").is_some());
    assert!(client.index_definition("missing").is_none());
    assert_eq!(client.index_names(), vec!["search_articles"]);
}

// ── Config ──────────────────────────────────────────────────────────────────

#[test]
fn config_defaults_are_conservative() {
    let config = SearchConfig::default();
    assert_eq!(config.queue, "search");
    assert_eq!(config.batch_size, 500);
    assert!(config.enabled);
    assert!(config.embedding_dimensions.is_none());
}

#[test]
fn config_parses_the_search_section_of_autumn_toml() {
    let toml = r#"
        [search]
        queue = "indexing"
        batch_size = 42
        enabled = false
        embedding_dimensions = 768
    "#;
    let config = SearchConfig::from_toml_str(toml).expect("parse");
    assert_eq!(config.queue, "indexing");
    assert_eq!(config.batch_size, 42);
    assert!(!config.enabled);
    assert_eq!(config.embedding_dimensions, Some(768));
}

#[test]
fn a_missing_search_section_yields_the_defaults() {
    let config = SearchConfig::from_toml_str("[server]\nport = 3000\n").expect("parse");
    assert_eq!(config, SearchConfig::default());
}

#[test]
fn an_unknown_search_key_is_rejected_rather_than_silently_ignored() {
    let toml = "[search]\nqueu = \"typo\"\n";
    assert!(SearchConfig::from_toml_str(toml).is_err());
}

#[test]
fn a_zero_batch_size_in_config_is_rejected() {
    let toml = "[search]\nbatch_size = 0\n";
    assert!(SearchConfig::from_toml_str(toml).is_err());
}

// ── The client taken before the plugin is mounted ───────────────────────────

#[tokio::test]
async fn every_client_the_plugin_hands_out_shares_one_index() {
    // The documented outside-request path forces `client()` to be called
    // *before* the plugin is moved into `app.plugin(...)`. If each call minted
    // its own default `MemorySearchBackend`, the retained handle and the one
    // the startup hook installs would address separate indexes: the app would
    // write through one and serve requests from the other, and every search
    // would come back empty with nothing in the logs.
    let plugin = SearchPlugin::new()
        .embedder(Arc::new(HashingEmbedder::new(32)))
        .index::<Article>();

    let retained = plugin.client();
    let installed = plugin.client();
    retained.ensure_indexes().await.expect("ensure");

    retained
        .index_record(&super::support::article(1, "Rust web", "a survey"))
        .await
        .expect("index through the retained handle");

    let page = installed
        .search::<Article>("rust", &autumn_search::PageRequest::default())
        .await
        .expect("search through the installed handle");
    assert_eq!(
        page.content.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![1],
        "both handles must address the same index"
    );
}

#[test]
fn a_client_taken_before_build_still_honours_the_resolved_config() {
    // `enabled` is resolved from the app's config, which happens at build
    // time — but a handle taken before then must not keep serving searches
    // that the resolved section turned off. Resolution is memoized on the
    // plugin, so both sides see one answer.
    //
    // Driven through `config(...)` here because a test process has no
    // `autumn.toml`; the point is that `client()` reads the same resolved
    // value `Plugin::build` does, not where that value came from.
    let disabled = SearchConfig::from_toml_str("[search]\nenabled = false\n").expect("parse");
    let plugin = SearchPlugin::new().config(disabled).index::<Article>();

    assert!(
        !plugin.client().is_enabled(),
        "a pre-build handle must not bypass the kill switch"
    );

    let app = autumn_web::app().plugin(plugin);
    assert!(app.has_plugin("autumn-search"));
}
