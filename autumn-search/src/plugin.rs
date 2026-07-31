//! The [`Plugin`] that mounts the whole subsystem with one builder call.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

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

/// Line the plugin prints to stdout the moment a one-shot backfill begins.
///
/// Without it `autumn search reindex` cannot distinguish "the backfill is
/// running and will take a while" from "this app never installed
/// `SearchPlugin`, so nothing consumed [`BACKFILL_ENV`] and it is now serving
/// HTTP forever" — and the CLI would appear to hang until interrupted. The CLI
/// waits a bounded time for this line and only then waits indefinitely.
pub const BACKFILL_STARTED_MARKER: &str = "autumn-search: backfill starting";

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
    /// Whether `postgres()` was requested. The store itself is built in
    /// `Plugin::build`, after the configuration is final.
    use_postgres: bool,
    /// A `queue(...)` builder override, applied ON TOP of the file config
    /// rather than instead of it.
    queue_override: Option<String>,
    /// The Postgres store, created on first need and shared by `client()` and
    /// `Plugin::build`.
    ///
    /// Memoized rather than built in `postgres()` so the configuration it
    /// snapshots (`embedding_dimensions`, which gates the pgvector fast path)
    /// is as late as possible — and rather than built twice, because only the
    /// instance `build` installs ever receives the connection pool. A second
    /// one would be permanently pool-less.
    postgres_store: OnceLock<Arc<PostgresSearchStore>>,
    /// The in-memory backend used when the app installs none, created once.
    ///
    /// Memoized for the same reason `postgres_store` is: `client()` and
    /// `Plugin::build` each assemble a client, and a fresh
    /// `MemorySearchBackend` per call would give them **separate indexes** —
    /// an app following the documented outside-request path would write to one
    /// and serve requests from the other.
    default_backend: OnceLock<Arc<dyn SearchBackend>>,
    /// The `[search]` section as finally resolved, memoized so `client()` and
    /// `Plugin::build` cannot disagree about it.
    ///
    /// `Err` is kept rather than discarded: `Plugin::build` cannot return an
    /// error, so a malformed section has to be surfaced from the startup hook.
    resolved_config: OnceLock<Result<SearchConfig, String>>,
    /// Set once the app supplies configuration explicitly, so `build` does not
    /// then overwrite it from the app's config files.
    config_explicit: bool,
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
            use_postgres: false,
            queue_override: None,
            postgres_store: OnceLock::new(),
            default_backend: OnceLock::new(),
            resolved_config: OnceLock::new(),
            config_explicit: false,
        }
    }

    /// Replace the whole `[search]` configuration.
    ///
    /// Setting it explicitly also stops [`Plugin::build`] from resolving
    /// `[search]` out of the app's config files and environment.
    #[must_use]
    pub fn config(mut self, config: SearchConfig) -> Self {
        self.config = config;
        self.config_explicit = true;
        self
    }

    /// Route the reindex/backfill jobs to a named queue.
    ///
    /// Overrides **only** `[search] queue`; every other key still comes from
    /// the resolved config. Suppressing the whole file here would mean an app that
    /// picks a queue in code silently loses `enabled = false` — i.e. the
    /// documented incident kill switch would stop working because of an
    /// unrelated builder call.
    #[must_use]
    pub fn queue(mut self, queue: impl Into<String>) -> Self {
        self.queue_override = Some(queue.into());
        self
    }

    /// Install the search engine.
    ///
    /// Overrides a previous [`Self::postgres`], **including** the document
    /// source it installed: leaving that behind would ship a
    /// [`PostgresSearchStore`] whose pool the startup hook no longer installs,
    /// so every reindex and backfill would fail at runtime with "the search
    /// store has no database pool".
    #[must_use]
    pub fn backend(mut self, backend: Arc<dyn SearchBackend>) -> Self {
        self.backend = Some(backend);
        if std::mem::take(&mut self.use_postgres) {
            self.source = None;
            self.postgres_store = OnceLock::new();
        }
        self
    }

    /// Use the Postgres backend (in-core FTS + `pgvector`), reading records
    /// through the Postgres document source.
    ///
    /// One [`PostgresSearchStore`] serves as both; its connection pool is
    /// installed from `AppState` at startup, so there is nothing to pass in.
    /// Builder order does not matter: the store is created in
    /// [`Plugin::build`], once the configuration is final, so
    /// `.postgres().config(cfg)` and `.config(cfg).postgres()` behave
    /// identically.
    #[must_use]
    pub const fn postgres(mut self) -> Self {
        self.use_postgres = true;
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

    /// Register an index definition directly — for a corpus that is not a
    /// `#[model]`, or to adjust a derived one. See
    /// [`SearchClientBuilder::index_definition`](crate::SearchClientBuilder::index_definition).
    #[must_use]
    pub fn index_definition(mut self, definition: IndexDefinition) -> Self {
        self.indexes.push(definition);
        self
    }

    /// Whether the Postgres backend is in use.
    #[must_use]
    pub const fn uses_postgres(&self) -> bool {
        self.use_postgres
    }

    /// The configuration as it stands, with any builder overrides applied.
    ///
    /// Before [`Plugin::build`] this does **not** include the app's config
    /// files — they are resolved at build time, so a test or a caller can
    /// inspect the builder's own intent without touching the filesystem.
    #[must_use]
    pub fn search_config(&self) -> SearchConfig {
        let mut config = self.config.clone();
        if let Some(queue) = &self.queue_override {
            config.queue.clone_from(queue);
        }
        config
    }

    /// The `[search]` section as the plugin will actually use it: builder
    /// intent, then the app's config files and environment (unless
    /// [`Self::config`] was called), then the [`Self::queue`] override.
    ///
    /// Resolved **once** and memoized, so a client taken before
    /// [`Plugin::build`] and the one `build` installs cannot disagree — in
    /// particular, an app that keeps a pre-build handle must not keep serving
    /// searches after the resolved config said `enabled = false`.
    fn resolved_config(&self) -> &Result<SearchConfig, String> {
        self.resolved_config.get_or_init(|| {
            if self.config_explicit {
                return Ok(self.search_config());
            }
            SearchConfig::resolve()
                .map(|mut config| {
                    // Builder overrides land on TOP of the file, never instead
                    // of it.
                    if let Some(queue) = &self.queue_override {
                        config.queue.clone_from(queue);
                    }
                    config
                })
                .map_err(|error| error.to_string())
        })
    }

    /// [`Self::resolved_config`], falling back to builder intent when
    /// resolution failed. The error itself aborts boot from the startup hook;
    /// this keeps every other path total.
    fn effective_config(&self) -> SearchConfig {
        self.resolved_config()
            .as_ref()
            .map_or_else(|_| self.search_config(), Clone::clone)
    }

    /// Build the [`SearchClient`] this plugin would install.
    ///
    /// Exposed so tests (and an app that drives search outside a request) can
    /// obtain the same client the plugin registers. Every shared part — the
    /// backend, the document source, the resolved configuration — is memoized
    /// on the plugin, so this client and the one [`Plugin::build`] installs
    /// address the same index and honour the same `[search]` section.
    ///
    /// It does reflect the builder *as configured so far*: calls made after
    /// this one land on the plugin, not on an already-built client.
    #[must_use]
    pub fn client(&self) -> SearchClient {
        self.client_builder().build()
    }

    /// The Postgres store this plugin will install, created once.
    ///
    /// `None` unless [`Self::postgres`] was requested.
    fn postgres_store(&self) -> Option<&Arc<PostgresSearchStore>> {
        if !self.use_postgres {
            return None;
        }
        Some(self.postgres_store.get_or_init(|| {
            Arc::new(PostgresSearchStore::new(
                self.effective_config().embedding_dimensions,
            ))
        }))
    }

    /// The shared in-memory backend used when the app installs none.
    fn default_backend(&self) -> &Arc<dyn SearchBackend> {
        self.default_backend
            .get_or_init(|| Arc::new(crate::MemorySearchBackend::new()) as Arc<dyn SearchBackend>)
    }

    fn client_builder(&self) -> SearchClientBuilder {
        let config = self.effective_config();
        let mut builder = SearchClient::builder()
            .enabled(config.enabled)
            .batch_size(config.batch_size);
        // `postgres()` only records the intent, so resolve the store here —
        // otherwise `SearchPlugin::new().postgres().client()` would silently
        // hand back an isolated in-memory index instead of the configured
        // persistent backend.
        if let Some(store) = self.postgres_store() {
            builder = builder
                .backend(Arc::clone(store) as Arc<dyn SearchBackend>)
                .source(Arc::clone(store) as Arc<dyn DocumentSource>);
        }
        if let Some(backend) = &self.backend {
            builder = builder.backend(Arc::clone(backend));
        } else if self.postgres_store().is_none() {
            // No backend configured: fall back to the in-memory one, but the
            // SAME instance every time. Letting `SearchClientBuilder` mint its
            // own default per call would give `client()` and `Plugin::build`
            // separate indexes — an app that indexed through the pre-build
            // handle would then serve requests from an empty one.
            builder = builder.backend(Arc::clone(self.default_backend()));
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
#[non_exhaustive]
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

        // `[search]` comes from the app's config unless the plugin was
        // configured explicitly in code. Without that, `enabled = false` —
        // documented as the incident kill switch — would need a code change
        // and a deploy, which is exactly what a kill switch must not need.
        //
        // Read through the memoized `resolved_config`, NOT resolved afresh:
        // an app that took a `client()` handle before mounting the plugin has
        // already pinned this value, and resolving twice would let the two
        // disagree. A malformed `[search]` is kept as an error rather than
        // falling back to defaults — an unknown key there is how a typo'd kill
        // switch goes unnoticed — and since `Plugin::build` cannot return one,
        // it is surfaced from the startup hook, which aborts boot.
        let config_error = self.resolved_config().as_ref().err().cloned();
        let config = self.effective_config();

        // Resolved (and memoized) here rather than in `postgres()`, so the
        // configuration it snapshots — `embedding_dimensions`, which gates the
        // pgvector fast path — reflects the resolved config regardless of
        // builder-call order. `client()` shares this instance: only the one
        // installed here receives the pool.
        let postgres = self.postgres_store().map(Arc::clone);

        let jobs = search_job_infos(&config.queue);
        // The client is fully assembled here: every part is an `Arc` supplied
        // by the app's own builder call. Only the Postgres pool has to wait
        // for `AppState`, and the store takes it lazily — so the startup hook
        // stays a few lines instead of re-running the whole assembly.
        let client = self.client();

        app.jobs(jobs).on_startup(move |state| {
            let client = client.clone();
            let postgres = postgres.clone();
            let config = config.clone();
            let config_error = config_error.clone();
            async move {
                if let Some(message) = config_error {
                    return Err(autumn_web::AutumnError::internal_server_error_msg(format!(
                        "autumn-search: {message}"
                    )));
                }
                // The declared width and the installed embedder's width must
                // agree before a single document is written. They are set in
                // two different places — `[search] embedding_dimensions` in
                // config, `Embedder::dimensions()` in code — and when they
                // disagree the failure is silent: every write succeeds, the
                // vector column rejects the wrong-width value, and semantic
                // search simply returns nothing. Boot loudly instead.
                //
                // `0` means no usable embedder (the default `NoEmbedder`), for
                // which nothing vector-shaped runs at all — declaring a width
                // ahead of installing a provider is a legitimate ordering.
                let embedder_width = client.embedding_dimensions();
                if let Some(configured) = config.embedding_dimensions
                    && embedder_width != 0
                    && embedder_width != configured
                {
                    return Err(autumn_web::AutumnError::internal_server_error_msg(format!(
                        "autumn-search: `search.embedding_dimensions = {configured}` does not \
                         match the installed embedder, which produces {embedder_width}-dimension \
                         vectors; set them to the same value (and re-run `autumn search reindex \
                         --purge` if the width really changed)"
                    )));
                }

                if let Some(store) = &postgres {
                    let pool = state.pool().cloned().ok_or_else(|| {
                        autumn_web::AutumnError::internal_server_error_msg(
                            "SearchPlugin::postgres() needs a database pool; configure \
                             `database.primary_url` or install a different backend",
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
    // Announced BEFORE the work starts: this is the CLI's proof that the
    // plugin exists and consumed the request. Flushed explicitly — stdout is
    // line-buffered when attached to a terminal but block-buffered through a
    // pipe, which is exactly how the CLI reads it.
    println!(
        "{BACKFILL_STARTED_MARKER} {}",
        target.name().unwrap_or("all")
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());

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
    fn a_queue_override_does_not_suppress_the_rest_of_the_file_config() {
        // The trap: if `queue(...)` marked the whole config explicit, an app
        // that picks a queue in code would silently lose `enabled = false`
        // from the resolved config — the incident kill switch defeated by an
        // unrelated builder call.
        let plugin = SearchPlugin::new().queue("indexing");
        assert!(
            !plugin.config_explicit,
            "a queue override must leave the file config in play"
        );

        // `config(...)` IS a whole-config replacement, and does suppress it.
        let plugin = SearchPlugin::new().config(SearchConfig::default());
        assert!(plugin.config_explicit);
    }

    #[test]
    fn an_explicit_config_still_honours_a_later_queue_override() {
        let config = SearchConfig::from_toml_str("[search]\nbatch_size = 7\n").expect("parse");
        let plugin = SearchPlugin::new().config(config).queue("indexing");
        let effective = plugin.search_config();
        assert_eq!(effective.queue, "indexing");
        assert_eq!(effective.batch_size, 7, "the rest of the config survives");
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
