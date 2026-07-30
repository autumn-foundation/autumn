//! The [`Plugin`] that mounts the whole subsystem with one builder call.

use std::borrow::Cow;
use std::sync::Arc;

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::search::{IndexDefinition, SearchIndexed};

use crate::authz::SearchVisibility;
use crate::backend::SearchBackend;
use crate::client::{BackfillOptions, SearchClient, SearchClientBuilder};
use crate::config::SearchConfig;
use crate::embedding::Embedder;
use crate::jobs::search_job_infos;
use crate::postgres::PostgresSearchStore;
use crate::source::DocumentSource;

/// Environment variable that turns a normal boot into a one-shot backfill.
///
/// `autumn search reindex` runs the application binary with this set, which is
/// the same "run the app, it knows its own wiring" technique `autumn jobs
/// manifest` uses (`AUTUMN_DUMP_JOBS`). It has to be the app: the indexes,
/// backend and embedder are registered at runtime by the app's own builder
/// call, so a standalone CLI cannot see them.
///
/// Set it to an index name to rebuild one index, or to `all` for every index.
pub const BACKFILL_ENV: &str = "AUTUMN_SEARCH_BACKFILL";

/// Environment variable that makes the one-shot backfill purge first.
pub const BACKFILL_PURGE_ENV: &str = "AUTUMN_SEARCH_BACKFILL_PURGE";

/// Keyword + vector search for `autumn-web` applications.
///
/// ```rust,ignore
/// autumn_web::app()
///     .plugin(
///         SearchPlugin::new()
///             .postgres()                 // Postgres FTS + pgvector backend
///             .embedder(Arc::new(MyEmbedder))
///             .index::<Article>()         // one line per searchable model
///     )
///     .routes(routes![...])
///     .run()
///     .await;
/// ```
pub struct SearchPlugin {
    config: SearchConfig,
    backend: Option<Arc<dyn SearchBackend>>,
    embedder: Option<Arc<dyn Embedder>>,
    source: Option<Arc<dyn DocumentSource>>,
    visibility: Option<Arc<dyn SearchVisibility>>,
    indexes: Vec<IndexDefinition>,
    postgres: Option<Arc<PostgresSearchStore>>,
}

impl Default for SearchPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchPlugin {
    /// Start configuring the plugin.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: SearchConfig::default(),
            backend: None,
            embedder: None,
            source: None,
            visibility: None,
            indexes: Vec::new(),
            postgres: None,
        }
    }

    /// Replace the whole `[search]` configuration.
    #[must_use]
    pub fn config(mut self, config: SearchConfig) -> Self {
        self.config = config;
        self
    }

    /// Route the reindex/backfill jobs to a named queue.
    #[must_use]
    pub fn queue(mut self, queue: impl Into<String>) -> Self {
        self.config.queue = queue.into();
        self
    }

    /// Install the search engine.
    #[must_use]
    pub fn backend(mut self, backend: Arc<dyn SearchBackend>) -> Self {
        self.backend = Some(backend);
        self.postgres = None;
        self
    }

    /// Use the Postgres backend (in-core FTS + `pgvector`), reading records
    /// through the Postgres document source.
    ///
    /// One [`PostgresSearchStore`] serves as both; its connection pool is
    /// installed from `AppState` at startup, so there is nothing to pass in.
    #[must_use]
    pub fn postgres(mut self) -> Self {
        let store = Arc::new(PostgresSearchStore::new(self.config.embedding_dimensions));
        self.postgres = Some(Arc::clone(&store));
        self.backend = Some(store.clone() as Arc<dyn SearchBackend>);
        self.source = Some(store as Arc<dyn DocumentSource>);
        self
    }

    /// Install the embedding provider.
    #[must_use]
    pub fn embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Install the record source used by reindex and backfill.
    #[must_use]
    pub fn source(mut self, source: Arc<dyn DocumentSource>) -> Self {
        self.source = Some(source);
        self
    }

    /// Install the authorization hook backing the `*_for` query methods.
    #[must_use]
    pub fn visibility(mut self, visibility: Arc<dyn SearchVisibility>) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Register a searchable model's index.
    #[must_use]
    pub fn index<M: SearchIndexed>(mut self) -> Self {
        self.indexes.push(M::index_definition());
        self
    }

    /// Register an index definition directly.
    #[must_use]
    pub fn index_definition(mut self, definition: IndexDefinition) -> Self {
        self.indexes.push(definition);
        self
    }

    /// Whether the Postgres backend is in use.
    #[must_use]
    pub const fn uses_postgres(&self) -> bool {
        self.postgres.is_some()
    }

    /// The effective configuration.
    #[must_use]
    pub const fn search_config(&self) -> &SearchConfig {
        &self.config
    }

    /// Build the [`SearchClient`] this plugin would install.
    ///
    /// Exposed so tests (and an app that drives search outside a request) can
    /// obtain the same client the plugin registers.
    #[must_use]
    pub fn client(&self) -> SearchClient {
        self.client_builder().build()
    }

    fn client_builder(&self) -> SearchClientBuilder {
        let mut builder = SearchClient::builder().enabled(self.config.enabled);
        if let Some(backend) = &self.backend {
            builder = builder.backend(Arc::clone(backend));
        }
        if let Some(embedder) = &self.embedder {
            builder = builder.embedder(Arc::clone(embedder));
        }
        if let Some(source) = &self.source {
            builder = builder.source(Arc::clone(source));
        }
        if let Some(visibility) = &self.visibility {
            builder = builder.visibility(Arc::clone(visibility));
        }
        for definition in &self.indexes {
            builder = builder.index_definition(definition.clone());
        }
        builder
    }
}

/// What a one-shot backfill boot was asked to rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackfillTarget {
    /// Every registered index (`AUTUMN_SEARCH_BACKFILL=all`).
    AllIndexes,
    /// One named index.
    Index(String),
}

impl BackfillTarget {
    /// The index name, or `None` for every index.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::AllIndexes => None,
            Self::Index(name) => Some(name),
        }
    }
}

/// The backfill requested through the environment, if any.
#[must_use]
pub fn backfill_request_from_env(
    config: &SearchConfig,
) -> Option<(BackfillTarget, BackfillOptions)> {
    let raw = std::env::var(BACKFILL_ENV).ok()?;
    let target = parse_backfill_target(&raw)?;
    let purge_raw = std::env::var(BACKFILL_PURGE_ENV).ok();
    Some((
        target,
        BackfillOptions::default()
            .batch_size(config.batch_size)
            .purge(parse_purge_flag(purge_raw.as_deref())),
    ))
}

/// Parse [`BACKFILL_ENV`]. `None` means "no backfill requested".
fn parse_backfill_target(raw: &str) -> Option<BackfillTarget> {
    let target = raw.trim();
    if target.is_empty() {
        return None;
    }
    if target.eq_ignore_ascii_case("all") {
        Some(BackfillTarget::AllIndexes)
    } else {
        Some(BackfillTarget::Index(target.to_owned()))
    }
}

/// Purge is opt-in: only an explicitly affirmative value enables it, so a
/// stray `AUTUMN_SEARCH_BACKFILL_PURGE=0` never empties an index.
fn parse_purge_flag(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes"))
}

impl Plugin for SearchPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("autumn-search")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        // Declare the plugin-owned `[search]` table first, so a host app with
        // `server.strict_config = true` boots instead of failing on an
        // "unknown key" — and so every return path below carries it.
        let app = app.config_section("search");

        let jobs = search_job_infos(&self.config.queue);
        let config = self.config.clone();
        let postgres = self.postgres.clone();
        // The client is fully assembled here: every part is an `Arc` supplied
        // by the app's own builder call. Only the Postgres pool has to wait
        // for `AppState`, and the store takes it lazily — so the startup hook
        // stays a few lines instead of re-running the whole assembly.
        let client = self.client();

        app.jobs(jobs).on_startup(move |state| {
            let client = client.clone();
            let postgres = postgres.clone();
            let config = config.clone();
            async move {
                if let Some(store) = &postgres {
                    let pool = state.pool().cloned().ok_or_else(|| {
                        autumn_web::AutumnError::internal_server_error_msg(
                            "SearchPlugin::postgres() needs a database pool; configure                              `database.primary_url` or install a different backend",
                        )
                    })?;
                    store.install_pool(pool);
                }

                client.ensure_indexes().await.map_err(|error| {
                    autumn_web::AutumnError::internal_server_error_msg(format!(
                        "search index setup failed: {error}"
                    ))
                })?;

                state.insert_extension(client.clone());

                // A one-shot backfill boot (`autumn search reindex`) rebuilds
                // and exits rather than going on to serve traffic.
                if let Some((target, options)) = backfill_request_from_env(&config) {
                    run_backfill_and_exit(&client, &target, &options).await;
                }
                Ok(())
            }
        })
    }
}

/// Run the requested backfill, print a summary, and exit the process.
///
/// This is the body of `autumn search reindex`: the CLI re-executes the
/// application binary with [`BACKFILL_ENV`] set, because only the app knows
/// which indexes, backend, and embedder are registered. Exiting rather than
/// returning is deliberate — the process was started to rebuild an index, not
/// to serve requests, and a non-zero exit is what makes the CLI fail loudly.
async fn run_backfill_and_exit(
    client: &SearchClient,
    target: &BackfillTarget,
    options: &BackfillOptions,
) -> ! {
    let result = match target.name() {
        Some(index) => client.backfill(index, options).await.map(|r| vec![r]),
        None => client.backfill_all(options).await,
    };
    match result {
        Ok(reports) => {
            for report in &reports {
                println!(
                    "autumn-search: reindexed {} ({} documents in {} batches{})",
                    report.index,
                    report.indexed,
                    report.batches,
                    if report.purged { ", purged first" } else { "" }
                );
            }
            if reports.is_empty() {
                println!("autumn-search: no indexes are registered; nothing to reindex");
            }
            std::process::exit(0)
        }
        Err(error) => {
            eprintln!("autumn-search: reindex failed: {error}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugin_name_is_the_crate_name() {
        assert_eq!(SearchPlugin::new().name(), "autumn-search");
    }

    #[test]
    fn the_queue_override_reaches_the_registered_jobs() {
        let plugin = SearchPlugin::new().queue("indexing");
        assert_eq!(plugin.search_config().queue, "indexing");
        let infos = search_job_infos(&plugin.search_config().queue);
        assert!(infos.iter().all(|info| info.queue == "indexing"));
    }

    #[test]
    fn an_explicit_backend_turns_off_the_postgres_default() {
        let plugin = SearchPlugin::new()
            .postgres()
            .backend(Arc::new(crate::memory::MemorySearchBackend::new()));
        assert!(!plugin.uses_postgres());
    }

    #[test]
    fn an_unset_or_blank_target_means_a_normal_boot() {
        // `backfill_request_from_env` reads the environment, which is
        // process-global and therefore not safe to mutate from a test; assert
        // on the parsing rule it implements instead.
        assert!(parse_backfill_target("").is_none());
        assert!(parse_backfill_target("   ").is_none());
    }

    #[test]
    fn all_means_every_index_and_a_name_means_one() {
        assert_eq!(
            parse_backfill_target("all"),
            Some(BackfillTarget::AllIndexes)
        );
        assert_eq!(
            parse_backfill_target("ALL"),
            Some(BackfillTarget::AllIndexes)
        );
        assert_eq!(
            parse_backfill_target("  articles  "),
            Some(BackfillTarget::Index("articles".to_owned()))
        );
        assert_eq!(BackfillTarget::AllIndexes.name(), None);
        assert_eq!(
            BackfillTarget::Index("articles".to_owned()).name(),
            Some("articles")
        );
    }

    #[test]
    fn purge_is_opt_in_and_only_for_affirmative_values() {
        assert!(parse_purge_flag(Some("1")));
        assert!(parse_purge_flag(Some("true")));
        assert!(parse_purge_flag(Some(" yes ")));
        assert!(!parse_purge_flag(Some("0")));
        assert!(!parse_purge_flag(Some("no")));
        assert!(!parse_purge_flag(Some("")));
        assert!(!parse_purge_flag(None));
    }
}
