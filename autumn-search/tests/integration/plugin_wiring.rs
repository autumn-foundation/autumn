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
    let config = SearchConfig::from_autumn_toml(toml).expect("parse");
    assert_eq!(config.queue, "indexing");
    assert_eq!(config.batch_size, 42);
    assert!(!config.enabled);
    assert_eq!(config.embedding_dimensions, Some(768));
}

#[test]
fn a_missing_search_section_yields_the_defaults() {
    let config = SearchConfig::from_autumn_toml("[server]\nport = 3000\n").expect("parse");
    assert_eq!(config, SearchConfig::default());
}

#[test]
fn an_unknown_search_key_is_rejected_rather_than_silently_ignored() {
    let toml = "[search]\nqueu = \"typo\"\n";
    assert!(SearchConfig::from_autumn_toml(toml).is_err());
}

#[test]
fn a_zero_batch_size_in_config_is_rejected() {
    let toml = "[search]\nbatch_size = 0\n";
    assert!(SearchConfig::from_autumn_toml(toml).is_err());
}
