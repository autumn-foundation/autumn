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
    /// Set once the app supplies configuration explicitly, so `build` does not
    /// then overwrite it from `autumn.toml`.
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
            config_explicit: false,
        }
    }

    /// Replace the whole `[search]` configuration.
    ///
    /// Setting it explicitly also stops [`Plugin::build`] from loading
    /// `[search]` out of `autumn.toml`.
    #[must_use]
    pub fn config(mut self, config: SearchConfig) -> Self {
        self.config = config;
        self.config_explicit = true;
        self
    }

    /// Route the reindex/backfill jobs to a named queue.
    ///
    /// Overrides **only** `[search] queue`; every other key still comes from
    /// `autumn.toml`. Suppressing the whole file here would mean an app that
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
    /// Before [`Plugin::build`] this does **not** include `autumn.toml` — the
    /// file is read at build time so a test or a caller can inspect the
    /// builder's own intent without touching the filesystem.
    #[must_use]
    pub fn search_config(&self) -> SearchConfig {
        let mut config = self.config.clone();
        if let Some(queue) = &self.queue_override {
            config.queue.clone_from(queue);
        }
        config
    }

    /// Build the [`SearchClient`] this plugin would install.
    ///
    /// Exposed so tests (and an app that drives search outside a request) can
    /// obtain the same client the plugin registers.
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
                self.search_config().embedding_dimensions,
            ))
        }))
    }

    fn client_builder(&self) -> SearchClientBuilder {
        let config = self.search_config();
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

/// Read `[search]` from the application's `autumn.toml`.
///
/// Resolution mirrors core's own config loader (`find_config_file_named` in
/// `autumn/src/config.rs`): the app's crate directory via
/// `AUTUMN_MANIFEST_DIR` first, then the process working directory. The two
/// differ whenever the binary is launched from somewhere other than the crate
/// root — `autumn search reindex --package app` from a workspace root, for
/// instance — and reading the wrong file (or none) would silently ignore
/// `enabled = false` or pick the wrong embedding mode.
///
/// `Ok(None)` means no file was found — an app with no `autumn.toml` is a
/// supported zero-config setup, so that is not an error. A file that exists
/// but has a malformed `[search]` **is**.
fn load_config_file() -> Result<Option<SearchConfig>, crate::config::SearchConfigError> {
    for path in config_file_candidates() {
        match std::fs::read_to_string(&path) {
            Ok(contents) => return SearchConfig::from_toml_str(&contents).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // A file that exists but cannot be read is a real problem — a
            // permissions mistake must not read as "zero-config".
            Err(error) => {
                return Err(crate::config::SearchConfigError::Invalid(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(None)
}

/// Candidate `autumn.toml` paths, in core's precedence order.
fn config_file_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::with_capacity(2);
    if let Ok(manifest_dir) = std::env::var("AUTUMN_MANIFEST_DIR") {
        candidates.push(std::path::PathBuf::from(manifest_dir).join("autumn.toml"));
    }
    candidates.push(std::path::PathBuf::from("autumn.toml"));
    candidates
}

impl Plugin for SearchPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("autumn-search")
    }

    fn build(mut self, app: AppBuilder) -> AppBuilder {
        // Declare the plugin-owned `[search]` table first, so a host app with
        // `server.strict_config = true` boots instead of failing on an
        // "unknown key" — and so every return path below carries it.
        let app = app.config_section("search");

        // Pick up `[search]` from `autumn.toml` unless the app configured the
        // plugin explicitly. Without this, `enabled = false` — documented as
        // the incident kill switch — would need a code change and a deploy,
        // which is exactly what a kill switch must not need.
        let mut config_error = None;
        if !self.config_explicit {
            match load_config_file() {
                Ok(Some(config)) => self.config = config,
                Ok(None) => {}
                // A malformed `[search]` must not silently fall back to
                // defaults: an unknown key there is how a typo'd kill switch
                // would go unnoticed. `Plugin::build` cannot return an error,
                // so surface it from the startup hook, which aborts boot.
                Err(error) => config_error = Some(error.to_string()),
            }
        }
        // Builder overrides land on TOP of the file, never instead of it.
        if let Some(queue) = self.queue_override.take() {
            self.config.queue = queue;
        }

        // Resolved (and memoized) here rather than in `postgres()`, so the
        // configuration it snapshots — `embedding_dimensions`, which gates the
        // pgvector fast path — reflects the file config loaded just above,
        // regardless of builder-call order. `client()` shares this instance:
        // only the one installed here receives the pool.
        let postgres = self.postgres_store().map(Arc::clone);

        let jobs = search_job_infos(&self.config.queue);
        let config = self.config.clone();
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
        // from `autumn.toml` — the incident kill switch defeated by an
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
    fn config_resolution_prefers_the_manifest_directory_over_the_cwd() {
        // `autumn search reindex --package app` runs the binary from the
        // workspace root, where the CWD holds a different `autumn.toml` (or
        // none). Core resolves through `AUTUMN_MANIFEST_DIR` first; the
        // plugin-owned section must resolve the same way or it reads a
        // different file than the rest of the config.
        let candidates = config_file_candidates();
        assert_eq!(
            candidates.last().map(|p| p.to_string_lossy().into_owned()),
            Some("autumn.toml".to_owned()),
            "the CWD is always the last resort"
        );
        // The manifest-dir candidate is present only when the env var is set,
        // and when present it must come FIRST.
        match std::env::var("AUTUMN_MANIFEST_DIR") {
            Ok(dir) => {
                assert_eq!(candidates.len(), 2);
                assert!(candidates[0].starts_with(&dir), "{candidates:?}");
            }
            Err(_) => assert_eq!(candidates.len(), 1),
        }
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
