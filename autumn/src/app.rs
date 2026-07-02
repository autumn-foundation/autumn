//! Application builder -- the entry point for configuring and running
//! an Autumn server.
//!
//! Every Autumn application follows the same pattern:
//!
//! 1. Call [`app()`] to create an [`AppBuilder`].
//! 2. Register routes with [`.routes()`](AppBuilder::routes).
//! 3. Call [`.run()`](AppBuilder::run) to start serving.
//!
//! # Example
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//!
//! #[get("/hello")]
//! async fn hello() -> &'static str { "Hello!" }
//!
//! #[autumn_web::main]
//! async fn main() {
//!     autumn_web::app()
//!         .routes(routes![hello])
//!         .run()
//!         .await;
//! }
//! ```

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::FutureExt as _;
use tracing::Instrument as _;

use crate::config::{AutumnConfig, ConfigLoader};
#[cfg(feature = "maud")]
use crate::error_pages::{ErrorPageRenderer, SharedRenderer};
use crate::middleware::exception_filter::ExceptionFilter;
#[cfg(feature = "db")]
use crate::migrate;
use crate::route::Route;
use crate::state::AppState;

/// Create a new [`AppBuilder`].
///
/// This is the primary entry point for constructing an Autumn application.
/// Chain [`.routes()`](AppBuilder::routes) calls to register handlers, then
/// call [`.run()`](AppBuilder::run) to start the server.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/")]
/// async fn index() -> &'static str { "hi" }
///
/// #[autumn_web::main]
/// async fn main() {
///     autumn_web::app()
///         .routes(routes![index])
///         .run()
///         .await;
/// }
/// ```
#[must_use]
pub fn app() -> AppBuilder {
    AppBuilder {
        routes: Vec::new(),
        api_versions: Vec::new(),
        route_sources: Vec::new(),
        current_plugin: None,
        tasks: Vec::new(),
        one_off_tasks: Vec::new(),
        jobs: Vec::new(),
        listeners: Vec::new(),
        static_metas: Vec::new(),
        exception_filters: Vec::new(),
        scoped_groups: Vec::new(),
        merge_routers: Vec::new(),
        nest_routers: Vec::new(),
        custom_layers: Vec::new(),
        static_gate_layers: Vec::new(),
        startup_hooks: Vec::new(),
        state_initializers: Vec::new(),
        shutdown_hooks: Vec::new(),
        extensions: HashMap::new(),
        registered_plugins: HashSet::new(),
        #[cfg(feature = "maud")]
        error_page_renderer: None,
        #[cfg(feature = "db")]
        migrations: Vec::new(),
        config_loader_factory: None,
        #[cfg(feature = "db")]
        pool_provider_factory: None,
        #[cfg(feature = "db")]
        shard_provider_factory: None,
        #[cfg(feature = "db")]
        shard_router: None,
        #[cfg(feature = "db")]
        directory_shard_router: false,
        telemetry_provider: None,
        session_store: None,
        #[cfg(feature = "ws")]
        channels_backend: None,
        #[cfg(feature = "storage")]
        blob_store: None,
        cache_backend: None,
        #[cfg(feature = "reporting")]
        error_reporters: Vec::new(),
        #[cfg(feature = "openapi")]
        openapi: None,
        #[cfg(feature = "mcp")]
        mcp: None,
        audit_logger: None,
        #[cfg(feature = "i18n")]
        i18n_bundle: None,
        #[cfg(feature = "i18n")]
        i18n_auto_load: false,
        #[cfg(feature = "embed-assets")]
        embedded_static: None,
        #[cfg(all(feature = "embed-assets", feature = "i18n"))]
        embedded_locales: None,
        policy_registrations: Vec::new(),
        #[cfg(feature = "mail")]
        mail_delivery_queue_factory: None,
        #[cfg(feature = "mail")]
        suppression_store: None,
        #[cfg(feature = "mail")]
        mount_unsubscribe_endpoint: false,
        #[cfg(feature = "mail")]
        mail_previews: Vec::new(),
        declared_routes: Vec::new(),
        idempotency_enabled: false,
        #[cfg(feature = "mail")]
        mail_interceptor: None,
        job_interceptor: None,
        #[cfg(feature = "db")]
        db_interceptor: None,
        #[cfg(feature = "ws")]
        channels_interceptor: None,
        #[cfg(feature = "oauth2")]
        http_interceptor: None,
        seo_sources: Vec::new(),
        metrics_sources: Vec::new(),
        health_indicators: Vec::new(),
        #[cfg(feature = "inbound-mail")]
        inbound_mail_router: None,
    }
}

type StartupHookFuture = Pin<Box<dyn Future<Output = crate::AutumnResult<()>> + Send>>;
type StartupHook = Box<dyn Fn(AppState) -> StartupHookFuture + Send + Sync>;
type StateInitializer = Box<dyn FnOnce(&AppState) + Send>;
type ShutdownHookFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type ShutdownHook = Box<dyn Fn() -> ShutdownHookFuture + Send + Sync>;

// ── Tier-1 subsystem factories ────────────────────────────────
//
// `ConfigLoader` and `DatabasePoolProvider` use RPIT (`-> impl Future + Send`)
// in their trait methods, so `Box<dyn Trait>` is not dyn-compatible. We store
// boxed factory closures that capture the concrete impl at the call site and
// erase its future type via `Pin<Box<dyn Future>>`. `TelemetryProvider`'s
// `init` is sync, so it's stored as a normal `Box<dyn>`.
type ConfigLoaderFactory = Box<
    dyn FnOnce() -> Pin<
            Box<dyn Future<Output = Result<AutumnConfig, crate::config::ConfigError>> + Send>,
        > + Send,
>;
#[cfg(feature = "db")]
type PoolProviderFactory = Box<
    dyn FnOnce(
            crate::config::DatabaseConfig,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<Option<crate::db::DatabaseTopology>, crate::db::PoolError>,
                    > + Send,
            >,
        > + Send,
>;
/// Captured [`DatabasePoolProvider::create_shard_topology`] calls: builds
/// one topology per configured shard, in declaration order.
#[cfg(feature = "db")]
type ShardProviderFactory = Box<
    dyn FnOnce(
            crate::config::DatabaseConfig,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Vec<crate::db::DatabaseTopology>, crate::db::PoolError>>
                    + Send,
            >,
        > + Send,
>;

/// Closure that registers a policy or scope on the runtime
/// [`PolicyRegistry`](crate::authorization::PolicyRegistry).
type PolicyRegistration = Box<dyn FnOnce(&crate::authorization::PolicyRegistry) + Send>;

/// Represents an API version registration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ApiVersion {
    /// The version name (e.g. "v1", "v2").
    pub version: String,
    /// When this version was deprecated.
    pub deprecated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When this version was sunsetted.
    pub sunset_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A wrapper for registered API versions in the app state.
#[derive(Clone, Debug)]
pub struct RegisteredApiVersions(pub Vec<ApiVersion>);

/// Builder for configuring and launching an Autumn application.
///
/// Created by [`app()`]. Collect routes with [`.routes()`](Self::routes),
/// then call [`.run()`](Self::run) to start the HTTP server.
///
/// The builder follows the **builder pattern**: each method consumes `self`
/// and returns a new `AppBuilder`, allowing chained calls.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/a")]
/// async fn route_a() -> &'static str { "a" }
///
/// #[get("/b")]
/// async fn route_b() -> &'static str { "b" }
///
/// #[autumn_web::main]
/// async fn main() {
///     autumn_web::app()
///         .routes(routes![route_a])
///         .routes(routes![route_b])
///         .run()
///         .await;
/// }
/// ```
#[allow(clippy::struct_excessive_bools)]
pub struct AppBuilder {
    pub(crate) routes: Vec<Route>,
    /// Registered API versions.
    pub api_versions: Vec<ApiVersion>,
    /// Parallel to `routes`: registration origin for each route.
    route_sources: Vec<crate::route_listing::RouteSource>,
    /// Non-None while a plugin's `build()` is executing; routes and scoped
    /// groups added during that window are attributed to this plugin.
    current_plugin: Option<String>,
    tasks: Vec<crate::task::TaskInfo>,
    one_off_tasks: Vec<crate::task::OneOffTaskInfo>,
    pub(crate) jobs: Vec<crate::job::JobInfo>,
    /// Registered event listeners; durable ones are synthesized into jobs at
    /// build time and the rest dispatch synchronously via the event registry.
    pub(crate) listeners: Vec<crate::events::ListenerInfo>,
    pub(crate) static_metas: Vec<crate::static_gen::StaticRouteMeta>,
    pub(crate) exception_filters: Vec<Arc<dyn ExceptionFilter>>,
    pub(crate) scoped_groups: Vec<ScopedGroup>,
    pub(crate) merge_routers: Vec<axum::Router<AppState>>,
    pub(crate) nest_routers: Vec<(String, axum::Router<AppState>)>,
    /// Custom Tower layers registered via [`AppBuilder::layer`], applied
    /// inside `RequestIdLayer` on ingress so they observe the request ID.
    pub(crate) custom_layers: Vec<CustomLayerRegistration>,
    /// Pre-static gate layers registered via [`AppBuilder::static_gate`],
    /// applied outermost (outside session and before the static cache lookup)
    /// so they can auth-gate / redirect requests before a cached SSG/ISG page
    /// is served.
    pub(crate) static_gate_layers: Vec<CustomLayerRegistration>,
    pub(crate) startup_hooks: Vec<StartupHook>,
    pub(crate) state_initializers: Vec<StateInitializer>,
    pub(crate) shutdown_hooks: Vec<ShutdownHook>,
    pub(crate) extensions: HashMap<TypeId, Box<dyn Any + Send>>,
    /// Plugin names that have already been applied, for duplicate detection.
    pub(crate) registered_plugins: HashSet<String>,
    /// Custom error page renderer (overrides built-in pages).
    #[cfg(feature = "maud")]
    error_page_renderer: Option<SharedRenderer>,
    /// Embedded Diesel migrations, registered via `.migrations()`.
    #[cfg(feature = "db")]
    migrations: Vec<migrate::EmbeddedMigrations>,
    /// Custom config loader (tier-1 subsystem replacement). When `None`, the
    /// default [`TomlEnvConfigLoader`](crate::config::TomlEnvConfigLoader) runs.
    config_loader_factory: Option<ConfigLoaderFactory>,
    /// Custom DB pool provider (tier-1 subsystem replacement). When `None`,
    /// the default [`DieselDeadpoolPoolProvider`](crate::db::DieselDeadpoolPoolProvider) runs.
    #[cfg(feature = "db")]
    pool_provider_factory: Option<PoolProviderFactory>,
    /// Companion to `pool_provider_factory` for `[[database.shards]]`
    /// topologies; captured from the same provider in `with_pool_provider`.
    #[cfg(feature = "db")]
    shard_provider_factory: Option<ShardProviderFactory>,
    /// Custom shard routing strategy. When `None` and shards are
    /// configured, the default [`HashShardRouter`](crate::sharding::HashShardRouter)
    /// is used.
    #[cfg(feature = "db")]
    shard_router: Option<Arc<dyn crate::sharding::ShardRouter>>,
    /// Builder opt-in for the control-DB [`DirectoryShardRouter`](crate::sharding::DirectoryShardRouter),
    /// applied to `config.database.directory_shard_router` at build time.
    #[cfg(feature = "db")]
    directory_shard_router: bool,
    /// Custom telemetry provider (tier-1 subsystem replacement). When `None`,
    /// the default [`TracingOtlpTelemetryProvider`](crate::telemetry::TracingOtlpTelemetryProvider) runs.
    telemetry_provider: Option<Box<dyn crate::telemetry::TelemetryProvider>>,
    /// Custom session store (tier-1 subsystem replacement). When `Some`,
    /// `apply_session_layer` skips the config-driven `memory`/`redis` selection
    /// and uses this store directly.
    session_store: Option<Arc<dyn crate::session::BoxedSessionStore>>,
    /// Custom channel backend (tier-1 subsystem replacement). When `Some`,
    /// `AppState` skips config-driven `in_process`/`redis` channel selection.
    #[cfg(feature = "ws")]
    channels_backend: Option<Arc<dyn crate::channels::ChannelsBackend>>,
    /// Custom blob store installed via
    /// [`AppBuilder::with_blob_store`]. When `Some`, `preflight_storage`
    /// is skipped and this store is installed directly onto `AppState`.
    #[cfg(feature = "storage")]
    blob_store: Option<crate::storage::SharedBlobStore>,
    /// Shared cache backend installed via [`AppBuilder::with_cache_backend`].
    /// When `Some`, installed onto `AppState` as `shared_cache` before startup
    /// hooks run.
    cache_backend: Option<Arc<dyn crate::cache::Cache>>,
    /// Error reporters registered via [`AppBuilder::with_error_reporter`].
    /// Installed onto `AppState` so the
    /// [`ReportingLayer`](crate::reporting::ReportingLayer) delivers panic and
    /// 5xx [`ErrorEvent`](crate::reporting::ErrorEvent)s to each. Empty means
    /// the built-in [`LogReporter`](crate::reporting::LogReporter) is used.
    #[cfg(feature = "reporting")]
    pub(crate) error_reporters: Vec<Arc<dyn crate::reporting::ErrorReporter>>,
    /// `OpenAPI` generation configuration. When `Some`, the router mounts
    /// `/v3/api-docs` (serving `openapi.json`) and `/swagger-ui` (if the
    /// Swagger UI path is set). When `None`, no docs endpoints are mounted.
    ///
    /// Gated behind the `openapi` feature: apps that don't need a
    /// served `OpenAPI` document shouldn't pay for the spec types or the
    /// runtime collision-check machinery.
    #[cfg(feature = "openapi")]
    openapi: Option<crate::openapi::OpenApiConfig>,
    /// MCP (Model Context Protocol) runtime config. `Some` once
    /// [`AppBuilder::mount_mcp`] is called; the contained `expose_all` flag is
    /// flipped by [`AppBuilder::expose_all_as_mcp`]. Gated behind the `mcp`
    /// feature (which implies `openapi`).
    #[cfg(feature = "mcp")]
    mcp: Option<crate::mcp::McpRuntime>,
    /// Shared audit logger used for append-only compliance events.
    audit_logger: Option<Arc<crate::audit::AuditLogger>>,
    /// Loaded i18n translation bundle. When `Some`, an `axum::Extension`
    /// layer publishing this bundle is added at `run()` time so the
    /// [`Locale`](crate::i18n::Locale) extractor can resolve translations.
    #[cfg(feature = "i18n")]
    i18n_bundle: Option<Arc<crate::i18n::Bundle>>,
    /// Whether to load the i18n bundle after the active config loader resolves
    /// [`AutumnConfig`]. This keeps `.i18n_auto()` aligned with
    /// `.with_config_loader(...)`.
    #[cfg(feature = "i18n")]
    i18n_auto_load: bool,
    /// Embedded `static/` tree (incl. the fingerprint manifest) registered via
    /// [`embedded_static`](AppBuilder::embedded_static). When set, `/static/*`
    /// is served from the binary and `asset_url()` resolves against the embedded
    /// manifest — no `static/` sidecar directory is read at runtime.
    #[cfg(feature = "embed-assets")]
    embedded_static: Option<crate::assets::EmbeddedStaticDir>,
    /// Embedded i18n locale bundles registered via
    /// [`embedded_locales`](AppBuilder::embedded_locales). When set (and no
    /// explicit bundle was provided), the bundle is loaded from the binary
    /// instead of the `i18n/` directory on disk.
    #[cfg(all(feature = "embed-assets", feature = "i18n"))]
    embedded_locales: Option<&'static include_dir::Dir<'static>>,
    /// Deferred [`Policy`](crate::authorization::Policy) and
    /// [`Scope`](crate::authorization::Scope) registrations applied
    /// to [`AppState::policy_registry`] just before the router is
    /// built. Stored as boxed closures so we can carry the
    /// generic type parameters across the builder boundary.
    policy_registrations: Vec<PolicyRegistration>,
    /// Durable mail delivery queue factory registered at builder time. Invoked
    /// with the freshly-built [`AppState`] before `install_mailer` runs so it
    /// can capture framework-managed resources (DB pool, channels, etc.).
    #[cfg(feature = "mail")]
    mail_delivery_queue_factory: Option<MailDeliveryQueueFactory>,
    #[cfg(feature = "mail")]
    pub(crate) suppression_store: Option<crate::mail::SuppressionStoreHandle>,
    #[cfg(feature = "mail")]
    pub(crate) mount_unsubscribe_endpoint: bool,
    /// Mail template previews registered for the dev preview UI.
    #[cfg(feature = "mail")]
    mail_previews: Vec<crate::mail::MailPreview>,
    /// Routes explicitly declared by plugins for listing purposes, to complement
    /// opaque `nest_routers`. Included in `autumn routes` output even though
    /// the underlying Axum router is not enumerable.
    declared_routes: Vec<crate::route_listing::RouteInfo>,
    /// Whether `.idempotent()` was called on this builder. Applied to the
    /// loaded `AutumnConfig` before router assembly so that startup validation
    /// and `apply_middleware` both see `config.idempotency.enabled = true`.
    idempotency_enabled: bool,
    #[cfg(feature = "mail")]
    mail_interceptor: Option<Arc<dyn crate::interceptor::MailInterceptor>>,
    job_interceptor: Option<Arc<dyn crate::interceptor::JobInterceptor>>,
    #[cfg(feature = "db")]
    db_interceptor: Option<Arc<dyn crate::interceptor::DbConnectionInterceptor>>,
    #[cfg(feature = "ws")]
    channels_interceptor: Option<Arc<dyn crate::interceptor::ChannelsInterceptor>>,
    #[cfg(feature = "oauth2")]
    http_interceptor: Option<Arc<dyn crate::interceptor::HttpInterceptor>>,
    /// Sitemap sources registered via [`AppBuilder::seo_source`].
    /// Each source provides dynamic URL entries for `/sitemap.xml`.
    seo_sources: Vec<Arc<dyn crate::seo::SitemapSource>>,

    /// Plugin-contributed metrics sources registered via [`AppBuilder::metrics_source`].
    pub(crate) metrics_sources: Vec<(String, Arc<dyn crate::actuator::MetricsSource>)>,
    /// Custom health indicators registered via [`AppBuilder::health_indicator`].
    pub(crate) health_indicators: Vec<(
        String,
        crate::actuator::IndicatorGroup,
        Arc<dyn crate::actuator::HealthIndicator>,
    )>,
    /// Inbound mail router registered via [`AppBuilder::inbound_mail_router`].
    /// HTTP webhook routes are derived from the router's endpoint configs and
    /// merged into the Axum router at startup.
    #[cfg(feature = "inbound-mail")]
    pub(crate) inbound_mail_router: Option<Arc<crate::inbound_mail::InboundMailRouter>>,
}

/// Boxed builder closure that constructs a durable
/// [`MailDeliveryQueue`](crate::mail::MailDeliveryQueue) from the live
/// [`AppState`].
#[cfg(feature = "mail")]
pub(crate) type MailDeliveryQueueFactory = Box<
    dyn FnOnce(&AppState) -> crate::AutumnResult<Arc<dyn crate::mail::MailDeliveryQueue>> + Send,
>;

/// A group of routes sharing a common path prefix and middleware layer.
///
/// Created by [`AppBuilder::scoped`]. The routes are mounted under the
/// prefix with the middleware applied only to this group.
pub struct ScopedGroup {
    pub prefix: String,
    pub routes: Vec<Route>,
    /// Registration origin: user application or a named plugin.
    pub source: crate::route_listing::RouteSource,
    /// Closure that applies the layer to a sub-router.
    pub apply_layer: Box<dyn FnOnce(axum::Router<AppState>) -> axum::Router<AppState> + Send>,
}

/// A deferred router mutator that applies a user-registered
/// [`tower::Layer`] to the app-wide router.
///
/// Stored on [`AppBuilder`] by [`AppBuilder::layer`] and drained inside
/// `apply_middleware` where the final layer stack is assembled.
pub(crate) type CustomLayerApplier =
    Box<dyn FnOnce(axum::Router<AppState>) -> axum::Router<AppState> + Send>;

/// Metadata and deferred application closure for a user-registered layer.
pub(crate) struct CustomLayerRegistration {
    /// Concrete type for the registered layer.
    pub(crate) type_id: TypeId,
    /// Concrete type name for generic layer families that need router-time
    /// classification without unstable specialization.
    pub(crate) type_name: &'static str,
    /// Deferred router mutation that applies the layer.
    pub(crate) apply: CustomLayerApplier,
}

mod sealed {
    pub trait Sealed {}
}

/// Marker trait for types that can be registered with
/// [`AppBuilder::layer`] as an app-wide Tower middleware.
///
/// Any [`tower::Layer`] whose produced service is a compatible axum
/// service (i.e. `Service<Request, Response = Response, Error = Infallible>`,
/// plus the usual `Clone + Send + Sync + 'static` bounds and a `Send`
/// future) implements this trait automatically via a blanket impl.
///
/// The trait is **sealed**: it exists only to surface a clean
/// `IntoAppLayer is not implemented for YourType` error message when a
/// candidate layer fails to meet axum's service bounds, instead of a
/// 40-line associated-type wall. You cannot implement it manually, and
/// you should not need to — just bring your own `tower::Layer`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a usable Autumn app-wide Tower layer",
    label = "this type does not implement `tower::Layer<axum::routing::Route>` with the required service bounds",
    note = "`AppBuilder::layer(..)` requires:\n    L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,\n    L::Service: Service<axum::extract::Request, Response = axum::response::Response, Error = Infallible> + Clone + Send + Sync + 'static,\n    <L::Service as Service<axum::extract::Request>>::Future: Send + 'static\nSee docs/guide/middleware.md for common patterns and how to wrap raw-error layers (e.g. TimeoutLayer) with HandleErrorLayer."
)]
pub trait IntoAppLayer: sealed::Sealed + Send + Sync + 'static {
    /// Apply this layer to the given router. Not intended for direct use.
    #[doc(hidden)]
    fn apply_to(self, router: axum::Router<AppState>) -> axum::Router<AppState>;
}

impl<L> sealed::Sealed for L
where
    L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
    L::Service: tower::Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + Sync
        + 'static,
    <L::Service as tower::Service<axum::extract::Request>>::Future: Send + 'static,
{
}

impl<L> IntoAppLayer for L
where
    L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
    L::Service: tower::Service<
            axum::extract::Request,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + Sync
        + 'static,
    <L::Service as tower::Service<axum::extract::Request>>::Future: Send + 'static,
{
    fn apply_to(self, router: axum::Router<AppState>) -> axum::Router<AppState> {
        router.layer(self)
    }
}

impl AppBuilder {
    /// Register a collection of routes with the application.
    ///
    /// Can be called multiple times -- routes are combined additively.
    /// Use the [`routes!`](crate::routes) macro to collect annotated
    /// handlers into the expected `Vec<Route>`.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use autumn_web::prelude::*;
    /// # #[get("/users")] async fn list_users() -> &'static str { "" }
    /// # #[get("/posts")] async fn list_posts() -> &'static str { "" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .routes(routes![list_users])
    ///     .routes(routes![list_posts])
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[must_use]
    pub fn routes(mut self, routes: Vec<Route>) -> Self {
        let source = self
            .current_plugin
            .as_ref()
            .map_or(crate::route_listing::RouteSource::User, |name| {
                crate::route_listing::RouteSource::Plugin(name.clone())
            });
        for _ in &routes {
            self.route_sources.push(source.clone());
        }
        self.routes.extend(routes);
        self
    }

    /// Register scheduled background tasks with the application.
    ///
    /// Tasks run alongside the HTTP server and are stopped during
    /// graceful shutdown. Use the [`tasks!`](crate::tasks) macro
    /// to collect `#[scheduled]` handlers.
    #[must_use]
    pub fn tasks(mut self, tasks: Vec<crate::task::TaskInfo>) -> Self {
        self.tasks.extend(tasks);
        self
    }

    /// Register one-off operational tasks runnable with `autumn task <name>`.
    ///
    /// Use the [`one_off_tasks!`](crate::one_off_tasks) macro to collect
    /// `#[task]` handlers.
    #[must_use]
    pub fn one_off_tasks(mut self, tasks: Vec<crate::task::OneOffTaskInfo>) -> Self {
        self.one_off_tasks.extend(tasks);
        self
    }

    /// Register ad-hoc background jobs with the application.
    #[must_use]
    pub fn jobs(mut self, jobs: Vec<crate::job::JobInfo>) -> Self {
        self.jobs.extend(jobs);
        self
    }

    /// Register event listeners with the application.
    ///
    /// Collect them with `listeners![..]`. Durable listeners are wired onto the
    /// job runtime automatically (no separate `jobs![..]` entry needed); sync
    /// listeners run in-request when their event is published. Decoupled from
    /// emitters: adding a listener never touches the code that publishes.
    #[must_use]
    pub fn listeners(mut self, listeners: Vec<crate::events::ListenerInfo>) -> Self {
        self.listeners.extend(listeners);
        self
    }

    /// Register static route metadata for build-time rendering.
    ///
    /// Use the [`static_routes!`](crate::static_routes) macro to collect
    /// `#[static_get]` handlers' metadata.
    #[must_use]
    pub fn static_routes(mut self, metas: Vec<crate::static_gen::StaticRouteMeta>) -> Self {
        self.static_metas.extend(metas);
        self
    }

    /// Register a [`SitemapSource`](crate::seo::SitemapSource) for dynamic sitemap entries.
    ///
    /// When called at least once, the framework automatically serves `/sitemap.xml` and
    /// `/robots.txt`. Dynamic sources (e.g. blog posts from a database) produce entries
    /// collected at request time.
    ///
    /// Combine with `[seo] base_url` in `autumn.toml` to auto-inject the `Sitemap:`
    /// directive in `robots.txt` and compute canonical URLs.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    /// use autumn_web::seo::{SitemapEntry, SitemapSource};
    /// use std::pin::Pin;
    /// use std::future::Future;
    ///
    /// struct PostsSitemap;
    ///
    /// impl SitemapSource for PostsSitemap {
    ///     fn entries(&self) -> Pin<Box<dyn Future<Output = Vec<SitemapEntry>> + Send>> {
    ///         Box::pin(async {
    ///             vec![SitemapEntry::new("https://example.com/posts/hello")]
    ///         })
    ///     }
    /// }
    ///
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// # #[get("/")] async fn index() -> &'static str { "" }
    /// autumn_web::app()
    ///     .routes(routes![index])
    ///     .seo_source(PostsSitemap)
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[must_use]
    pub fn seo_source<S: crate::seo::SitemapSource + 'static>(mut self, source: S) -> Self {
        self.seo_sources.push(Arc::new(source));
        self
    }

    /// Enable `OpenAPI` (Swagger) spec auto-generation.
    ///
    /// When called, the framework inspects every registered route's
    /// [`ApiDoc`](crate::openapi::ApiDoc) metadata — inferred at compile
    /// time from the route path, HTTP method, extractor types, and any
    /// [`#[api_doc(...)]`](crate::api_doc) overrides — and serves an
    /// `OpenAPI` 3.0 JSON document at `OpenApiConfig::openapi_json_path`
    /// (default `/v3/api-docs`). If
    /// `OpenApiConfig::swagger_ui_path` is set (default `/swagger-ui`),
    /// a Swagger UI HTML page is served there too.
    ///
    /// Routes marked `#[api_doc(hidden)]` are excluded.
    ///
    /// **Gated behind the `openapi` Cargo feature.** Add
    /// `features = ["openapi"]` to your `autumn-web` dependency to
    /// enable it; the default build excludes the runtime spec types
    /// and endpoints to keep the binary small.
    ///
    /// # Examples
    ///
    /// Zero-config:
    ///
    /// ```rust,ignore
    /// use autumn_web::prelude::*;
    /// use autumn_web::openapi::OpenApiConfig;
    ///
    /// # #[get("/hello")] async fn hello() -> &'static str { "hi" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .routes(routes![hello])
    ///     .openapi(OpenApiConfig::new("My API", "1.0.0"))
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    ///
    /// With custom paths:
    ///
    /// ```rust,ignore
    /// use autumn_web::openapi::OpenApiConfig;
    ///
    /// let config = OpenApiConfig::new("My API", "1.0.0")
    ///     .description("Full product API")
    ///     .openapi_json_path("/openapi.json")
    ///     .swagger_ui_path(Some("/docs".to_owned()));
    /// ```
    #[cfg(feature = "openapi")]
    #[must_use]
    pub fn openapi(mut self, config: crate::openapi::OpenApiConfig) -> Self {
        self.openapi = Some(config);
        self
    }

    /// Mount a Model Context Protocol (MCP) endpoint at `path` (e.g. `/mcp`).
    ///
    /// Projects opted-in routes — those tagged `#[api_doc(mcp)]` — as
    /// agent-callable MCP tools over Streamable HTTP, handling `initialize`,
    /// `tools/list`, and `tools/call`. A tool's `name`, `description`, and
    /// `inputSchema` are derived from the handler's existing
    /// [`ApiDoc`](crate::openapi::ApiDoc), so the tool catalog cannot drift
    /// from the handler's typed contract. `tools/call` dispatches through the
    /// real handler pipeline, so `#[secured]`, authorization, rate limits, and
    /// validation apply identically to agent and HTTP calls.
    ///
    /// Opt-in is per-endpoint; nothing is exposed implicitly. Use
    /// [`expose_all_as_mcp`](Self::expose_all_as_mcp) for the whole-API hatch.
    ///
    /// Only **JSON** endpoints are projected: a route is eligible when it
    /// returns `Json<T>` (the structural signal for a JSON response). The
    /// generated tool's `body` input is derived solely from a `Json<T>`
    /// request extractor, so a handler that returns `Json<T>` but reads its
    /// body via `Form<T>`, `Multipart`, `Bytes`, or `String` should **not** be
    /// opted in — the tool would carry no body input and replay an empty
    /// request. Use JSON request bodies for endpoints exposed as MCP tools.
    ///
    /// `tools/call` replays through the same pipeline as a direct HTTP request,
    /// so `#[secured]`, route guards, rate limits, and validation apply
    /// identically. One caveat applies only in **static/ISR mode** (an app with
    /// a `dist` manifest): a global [`layer`](Self::layer) is applied outside
    /// the static-first middleware and is therefore *not* traversed by MCP
    /// `tools/call` replays. Prefer `#[secured]` or route-level guards (which do
    /// apply) for MCP-exposed handlers in that mode.
    ///
    /// Requires the `mcp` Cargo feature.
    ///
    /// ```rust,ignore
    /// autumn_web::app()
    ///     .routes(routes![list_todos, create_todo])
    ///     .mount_mcp("/mcp")
    ///     .run()
    ///     .await;
    /// ```
    #[cfg(feature = "mcp")]
    #[must_use]
    pub fn mount_mcp(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        if let Some(rt) = self.mcp.as_mut() {
            rt.mount_path = path;
        } else {
            self.mcp = Some(crate::mcp::McpRuntime::new(path));
        }
        self
    }

    /// Whole-API escape hatch: expose **every** eligible read (`GET`) endpoint
    /// as an MCP tool without per-endpoint tags.
    ///
    /// This is an explicit, separate opt-in — never the default. It still
    /// honors per-endpoint exclusions (`#[api_doc(mcp = false)]`) and the
    /// JSON-only rule, and **mutating verbs (`POST`/`PUT`/`PATCH`/`DELETE`)
    /// still require an explicit `#[api_doc(mcp)]` opt-in** even under the
    /// hatch.
    ///
    /// On its own this mounts the endpoint at the default `/mcp`; chain
    /// [`mount_mcp`](Self::mount_mcp) to serve it at a different path.
    ///
    /// Requires the `mcp` Cargo feature.
    #[cfg(feature = "mcp")]
    #[must_use]
    pub fn expose_all_as_mcp(mut self) -> Self {
        if let Some(rt) = self.mcp.as_mut() {
            rt.expose_all = true;
        } else {
            let mut rt = crate::mcp::McpRuntime::new("/mcp");
            rt.expose_all = true;
            self.mcp = Some(rt);
        }
        self
    }

    /// Gate the **entire** MCP endpoint — the catalog (`initialize`/
    /// `tools/list`) as well as tool dispatch — behind a tower `layer`.
    ///
    /// The `/mcp` envelope is otherwise reachable without the app's global
    /// middleware. Pass an auth layer (e.g.
    /// [`RequireApiToken`](crate::auth::RequireApiToken)) here to require a
    /// credential for the whole endpoint, the way you'd protect a normal
    /// route group. Combine with [`mount_mcp`](Self::mount_mcp); the MCP
    /// transport's spec-required `Origin` validation (sourced from your CORS
    /// `allowed_origins`) always applies regardless of this layer.
    ///
    /// Requires the `mcp` Cargo feature.
    #[cfg(feature = "mcp")]
    #[must_use]
    pub fn secure_mcp<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<
                axum::http::Request<axum::body::Body>,
                Response = axum::http::Response<axum::body::Body>,
                Error = std::convert::Infallible,
            > + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as tower::Service<axum::http::Request<axum::body::Body>>>::Future:
            Send + 'static,
    {
        let applier: crate::mcp::McpEndpointLayer = Box::new(move |router| router.layer(layer));
        if let Some(rt) = self.mcp.as_mut() {
            rt.endpoint_layer = Some(applier);
        } else {
            let mut rt = crate::mcp::McpRuntime::new("/mcp");
            rt.endpoint_layer = Some(applier);
            self.mcp = Some(rt);
        }
        self
    }

    /// Register a global exception filter.
    ///
    /// Exception filters intercept error responses produced by
    /// [`AutumnError`](crate::AutumnError) before they are sent to the
    /// client. Filters run in registration order.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::middleware::{ExceptionFilter, AutumnErrorInfo};
    /// use axum::response::Response;
    ///
    /// struct LogFilter;
    /// impl ExceptionFilter for LogFilter {
    ///     fn filter(&self, error: &AutumnErrorInfo, response: Response) -> Response {
    ///         eprintln!("Error: {}", error.message);
    ///         response
    ///     }
    /// }
    ///
    /// # use autumn_web::prelude::*;
    /// # #[get("/")] async fn index() -> &'static str { "" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .exception_filter(LogFilter)
    ///     .routes(routes![index])
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[must_use]
    pub fn exception_filter(mut self, filter: impl ExceptionFilter) -> Self {
        self.exception_filters.push(Arc::new(filter));
        self
    }

    /// Register a custom error page renderer.
    ///
    /// The renderer replaces the built-in default error pages (404, 422, 500,
    /// and generic errors). Implement [`ErrorPageRenderer`] to provide your
    /// own branded error pages.
    ///
    /// Only one renderer can be active. Calling this method multiple times
    /// replaces the previous renderer.
    ///
    /// Requires the `maud` feature.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::error_pages::{ErrorPageRenderer, ErrorContext};
    /// use maud::{Markup, html};
    ///
    /// struct MyErrors;
    ///
    /// impl ErrorPageRenderer for MyErrors {
    ///     fn render_error(&self, ctx: &ErrorContext) -> Markup {
    ///         html! {
    ///             h1 { (ctx.status.as_u16()) " - Custom error page" }
    ///         }
    ///     }
    /// }
    ///
    /// # use autumn_web::prelude::*;
    /// # #[get("/")] async fn index() -> &'static str { "" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .error_pages(MyErrors)
    ///     .routes(routes![index])
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[must_use]
    #[cfg(feature = "maud")]
    pub fn error_pages(mut self, renderer: impl ErrorPageRenderer) -> Self {
        self.error_page_renderer = Some(Arc::new(renderer));
        self
    }

    /// Register a group of routes with a shared path prefix and middleware.
    ///
    /// The `layer` is applied only to routes within this group, not to the
    /// rest of the application. The routes are mounted under `prefix`.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    /// use autumn_web::middleware::RequestIdLayer; // any Tower Layer
    ///
    /// # #[get("/")]  async fn index() -> &'static str { "" }
    /// # #[get("/users")] async fn list_users() -> &'static str { "" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .routes(routes![index])
    ///     .scoped("/api", RequestIdLayer, routes![list_users])
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[must_use]
    pub fn scoped<L>(mut self, prefix: &str, layer: L, routes: Vec<Route>) -> Self
    where
        L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<
                axum::http::Request<axum::body::Body>,
                Response = axum::http::Response<axum::body::Body>,
                Error = std::convert::Infallible,
            > + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as tower::Service<axum::http::Request<axum::body::Body>>>::Future:
            Send + 'static,
    {
        let source = self
            .current_plugin
            .as_ref()
            .map_or(crate::route_listing::RouteSource::User, |name| {
                crate::route_listing::RouteSource::Plugin(name.clone())
            });
        self.scoped_groups.push(ScopedGroup {
            prefix: prefix.to_owned(),
            routes,
            source,
            apply_layer: Box::new(move |router| router.layer(layer)),
        });
        self
    }

    /// Apply a custom [`tower::Layer`] to the entire application.
    ///
    /// This is the escape hatch for integrating any middleware from the
    /// Tower / Tower-HTTP ecosystem (timeouts, rate limiting, bespoke
    /// tracing, request signing, etc.) without forking the framework.
    ///
    /// The generic bound is [`IntoAppLayer`], a sealed trait with a blanket
    /// impl for every `tower::Layer` that meets axum's service requirements
    /// — in practice this means any standard Tower layer whose service
    /// produces `Infallible` errors. If your layer produces real errors
    /// (like `TimeoutLayer`'s `BoxError`), wrap it with
    /// [`axum::error_handling::HandleErrorLayer`] before passing it here.
    ///
    /// # Ordering
    ///
    /// User layers are applied **inside** Autumn's request-ID layer on the
    /// ingress path, which means your middleware always sees the generated
    /// `RequestId` in the request extensions. The full stack (outermost to
    /// innermost on ingress) is:
    ///
    /// `Metrics -> ExceptionFilter -> ErrorPageContext -> Session ->`
    /// `SecurityHeaders -> RequestId -> [user layers, registration order]`
    /// `-> CSRF -> CORS -> route handler`
    ///
    /// When `.layer()` is called multiple times, the **first** call becomes
    /// the outermost user layer on ingress (matching `tower::ServiceBuilder`
    /// semantics): the layer from the first `.layer(...)` call sees the
    /// request first on the way in and the response last on the way out.
    ///
    /// # Scope
    ///
    /// This layer applies **globally** to every route in the app, including
    /// routes added later by plugins, routes mounted via `.merge` / `.nest`,
    /// and the built-in `404` fallback. Use [`AppBuilder::scoped`] when you
    /// need middleware scoped to a group of routes.
    ///
    /// Shared state (pools, metrics registries, rate-limit stores, etc.)
    /// should be wrapped in `Arc` so the layer can satisfy the
    /// `Clone + Send + Sync + 'static` bounds without moving the state.
    ///
    /// See [the middleware guide](https://github.com/madmax983/autumn/blob/trunk/docs/guide/middleware.md)
    /// for ready-made recipes.
    ///
    /// # Examples
    ///
    /// Adding a Tower timeout layer in one line (Tower's `TimeoutLayer`
    /// returns `BoxError`, so it must be paired with `HandleErrorLayer` to
    /// satisfy axum's `Infallible` error requirement):
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use autumn_web::prelude::*;
    /// use axum::{error_handling::HandleErrorLayer, http::StatusCode};
    /// use tower::{ServiceBuilder, timeout::TimeoutLayer};
    ///
    /// # #[get("/")] async fn index() -> &'static str { "ok" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .routes(routes![index])
    ///     .layer(
    ///         ServiceBuilder::new()
    ///             .layer(HandleErrorLayer::new(|_| async {
    ///                 StatusCode::REQUEST_TIMEOUT
    ///             }))
    ///             .layer(TimeoutLayer::new(Duration::from_secs(5))),
    ///     )
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[must_use]
    pub fn layer<L: IntoAppLayer>(mut self, layer: L) -> Self {
        self.custom_layers.push(CustomLayerRegistration {
            type_id: TypeId::of::<L>(),
            type_name: std::any::type_name::<L>(),
            apply: Box::new(move |router| layer.apply_to(router)),
        });
        self
    }

    /// Returns `true` when a custom layer of type `L` has already been
    /// registered via [`AppBuilder::layer`].
    ///
    /// Intended for plugin pre-flight validation before the app is started.
    #[must_use]
    pub fn has_layer<L: 'static>(&self) -> bool {
        let layer_type = TypeId::of::<L>();
        self.custom_layers
            .iter()
            .any(|registered| registered.type_id == layer_type)
    }

    /// Enable the HTTP idempotency-key middleware for this application.
    ///
    /// Mutating requests (`POST`, `PUT`, `PATCH`, `DELETE`) that carry an
    /// `Idempotency-Key` header are deduplicated: the first response is cached
    /// and replayed byte-for-byte on subsequent identical requests.
    /// Session-mutating responses are cached after the outer session middleware
    /// has finalized `Set-Cookie`, so retries can observe the successful
    /// mutation without re-entering the handler.
    ///
    /// Raw Axum routers registered with [`merge`](Self::merge) or
    /// [`nest`](Self::nest) are opaque to Autumn. They are protected from
    /// duplicate mutating retries by failing closed on cache hits; install
    /// idempotency and replay-stop layers inside those routers when raw routes
    /// need successful cached-response replay after their own route-local
    /// checks.
    ///
    /// The storage backend and TTL are taken from the `[idempotency]` block in
    /// `autumn.toml` (defaulting to in-process memory with a 24 h TTL).
    /// For multi-replica deployments set `backend = "redis"` and configure
    /// `[idempotency.redis]`.
    ///
    /// # Startup validation
    ///
    /// In production (`AUTUMN_PROFILE=production`) the memory backend is
    /// rejected unless `allow_memory_in_production = true` is set explicitly.
    #[must_use]
    pub const fn idempotent(mut self) -> Self {
        self.idempotency_enabled = true;
        self
    }

    /// Returns the registered custom layer types in registration order.
    ///
    /// This includes only user-installed layers from
    /// [`AppBuilder::layer`], not framework-managed middleware.
    #[must_use]
    pub fn get_layer_types(&self) -> Vec<TypeId> {
        self.custom_layers
            .iter()
            .map(|registered| registered.type_id)
            .collect()
    }

    /// Register a Tower layer that runs **before** the static file middleware
    /// and the static cache lookup — Autumn's equivalent of Next.js *Edge
    /// Middleware*.
    ///
    /// Cached SSG/ISG pages are served by the static-first middleware before
    /// the inner router (session, auth) is ever reached, so framework auth
    /// layers cannot gate pre-rendered responses. A `static_gate` layer runs
    /// outermost — outside the session layer and ahead of the static cache —
    /// so it can redirect or reject a request before a cached page is served.
    ///
    /// This is the right place for auth gating that protects pre-rendered
    /// routes: redirect unauthenticated visitors to a login page while leaving
    /// the cached HTML free of user-specific content. Personalised content
    /// still requires a fully dynamic route or client-side fetching.
    ///
    /// # Position and limitations
    ///
    /// * Runs as the **outermost** user middleware in *both* SSG/ISG and
    ///   fully-dynamic modes, so the same gate behaves identically regardless
    ///   of whether static generation is active.
    /// * Has access to request **headers and cookies**, but **NOT** the
    ///   session [`Extension`](axum::Extension) — the session layer runs inside
    ///   it. Verify a signed/JWT session cookie directly (e.g. with the same
    ///   signing key configured for the session) rather than relying on
    ///   session-populated extensions.
    /// * Like [`layer`](Self::layer), it applies globally to every route.
    /// * **Page-cache gate, not API auth.** The gate guards GET/HEAD page
    ///   serving and acts by issuing a browser redirect/reject. It is **not**
    ///   applied to MCP `tools/call` dispatch (a JSON-RPC call, where a redirect
    ///   is meaningless) in *either* mode: the gate is applied after the MCP
    ///   dispatch clone is taken. Gate MCP tools and JSON APIs with route-level
    ///   guards / `#[secured]` / session auth, which always traverse the
    ///   dispatch path. A well-behaved gate should therefore no-op on non-GET
    ///   requests (such as the `/mcp` JSON-RPC POST transport).
    /// * Short-circuit responses (the redirect/reject) are wrapped by the
    ///   framework's security-header layer, so they still carry HSTS/CSP, etc.
    /// * Because the gate runs **outside** the request stack (it must run before
    ///   session and the static cache), a gate short-circuit does **not** pass
    ///   through trusted-host validation or the per-request timeout — same as any
    ///   middleware registered with [`layer`](Self::layer) that runs before
    ///   those framework layers. Keep gate work bounded (prefer local
    ///   cookie/JWT checks over unbounded remote calls), and rely on the
    ///   framework's trusted-host policy for the routes the gate forwards to.
    ///
    /// Layers are wrapped in registration order with the first-registered gate
    /// outermost, matching [`tower::ServiceBuilder`] semantics.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    /// use axum::{
    ///     extract::Request,
    ///     http::{header, Method, StatusCode},
    ///     middleware::Next,
    ///     response::Response,
    /// };
    ///
    /// async fn require_auth(req: Request, next: Next) -> Response {
    ///     // Only gate page navigation. Pass non-GET/HEAD requests (JSON APIs,
    ///     // form POSTs, the `/mcp` JSON-RPC transport, CORS preflights) straight
    ///     // through so a browser redirect never turns them into a 302.
    ///     let is_page = matches!(req.method(), &Method::GET | &Method::HEAD);
    ///     // Inspect a signed session cookie directly — no session Extension
    ///     // is available this far out in the stack.
    ///     if !is_page || req.headers().contains_key("x-authed") {
    ///         next.run(req).await
    ///     } else {
    ///         Response::builder()
    ///             .status(StatusCode::FOUND)
    ///             .header(header::LOCATION, "/login")
    ///             .body(axum::body::Body::empty())
    ///             .unwrap()
    ///     }
    /// }
    ///
    /// # #[get("/")] async fn index() -> &'static str { "ok" }
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .routes(routes![index])
    ///     .static_gate(axum::middleware::from_fn(require_auth))
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    #[must_use]
    pub fn static_gate<L: IntoAppLayer>(mut self, layer: L) -> Self {
        self.static_gate_layers.push(CustomLayerRegistration {
            type_id: TypeId::of::<L>(),
            type_name: std::any::type_name::<L>(),
            apply: Box::new(move |router| layer.apply_to(router)),
        });
        self
    }

    /// Returns `true` when a pre-static gate layer of type `L` has already
    /// been registered via [`AppBuilder::static_gate`].
    ///
    /// Intended for plugin pre-flight validation before the app is started.
    #[must_use]
    pub fn has_static_gate<L: 'static>(&self) -> bool {
        let layer_type = TypeId::of::<L>();
        self.static_gate_layers
            .iter()
            .any(|registered| registered.type_id == layer_type)
    }

    /// Returns the registered pre-static gate layer types in registration
    /// order.
    ///
    /// This includes only user-installed gates from
    /// [`AppBuilder::static_gate`], not regular layers or framework
    /// middleware.
    #[must_use]
    pub fn get_static_gate_types(&self) -> Vec<TypeId> {
        self.static_gate_layers
            .iter()
            .map(|registered| registered.type_id)
            .collect()
    }

    /// Merge a raw Axum router into the application.
    ///
    /// This is an escape hatch for when Autumn's route macros are not
    /// sufficient -- for example, when integrating a third-party Axum
    /// middleware crate or mounting a hand-built WebSocket handler.
    ///
    /// The merged router shares the same [`AppState`] (database pool,
    /// config, etc.) and Autumn's global middleware (request IDs,
    /// security headers, session management) applies to its routes.
    /// When `.idempotent()` is enabled, retries that hit an existing raw-route
    /// idempotency record fail closed instead of rerunning the raw handler or
    /// replaying around opaque route-local checks. Install idempotency and
    /// replay-stop layers inside the raw router when successful replay is
    /// required.
    ///
    /// Merged routes are added **after** Autumn's annotated routes.
    /// If both define the same method+path pair, Axum treats that as an
    /// overlap and router construction will fail.
    ///
    /// Can be called multiple times -- routers are accumulated.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    /// use autumn_web::AppState;
    ///
    /// #[get("/")]
    /// async fn index() -> &'static str { "hi" }
    ///
    /// #[autumn_web::main]
    /// async fn main() {
    ///     let raw = axum::Router::<AppState>::new()
    ///         .route("/ws", axum::routing::get(|| async { "websocket" }));
    ///
    ///     autumn_web::app()
    ///         .routes(routes![index])
    ///         .merge(raw)
    ///         .run()
    ///         .await;
    /// }
    /// ```
    #[must_use]
    pub fn merge(mut self, router: axum::Router<AppState>) -> Self {
        self.merge_routers.push(router);
        self
    }

    /// Mount a raw Axum router under a path prefix.
    ///
    /// This is an escape hatch similar to [`merge`](Self::merge), but the
    /// router's routes are nested under the given `path` prefix. Useful
    /// for mounting a self-contained API version or third-party router.
    ///
    /// The nested router shares the same [`AppState`] and Autumn's global
    /// middleware applies to its routes. When `.idempotent()` is enabled,
    /// retries that hit an existing raw-route idempotency record fail closed
    /// instead of rerunning the raw handler or replaying around opaque
    /// route-local checks. Install idempotency and replay-stop layers inside
    /// the raw router when successful replay is required.
    ///
    /// Can be called multiple times with different prefixes.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    /// use autumn_web::AppState;
    ///
    /// #[get("/")]
    /// async fn index() -> &'static str { "hi" }
    ///
    /// #[autumn_web::main]
    /// async fn main() {
    ///     let v2 = axum::Router::<AppState>::new()
    ///         .route("/users", axum::routing::get(|| async { "v2 users" }));
    ///
    ///     autumn_web::app()
    ///         .routes(routes![index])
    ///         .nest("/api/v2", v2)
    ///         .run()
    ///         .await;
    /// }
    /// ```
    #[must_use]
    pub fn nest(mut self, path: &str, router: axum::Router<AppState>) -> Self {
        self.nest_routers.push((path.to_owned(), router));
        self
    }

    /// Explicitly register route metadata for listing via `autumn routes`.
    ///
    /// Plugins that mount routes via [`AppBuilder::nest`] (which is opaque to
    /// the route listing) can call this method so that `autumn routes --format json`
    /// shows their routes with the correct plugin attribution.
    ///
    /// Routes are automatically attributed to the current plugin when called from
    /// within a plugin's `build()` method. The `source` field of each supplied
    /// `RouteInfo` is overwritten with that attribution.
    #[must_use]
    pub fn declare_plugin_routes(
        mut self,
        routes: impl IntoIterator<Item = crate::route_listing::RouteInfo>,
    ) -> Self {
        let source = self
            .current_plugin
            .as_deref()
            .map_or(crate::route_listing::RouteSource::User, |name| {
                crate::route_listing::RouteSource::Plugin(name.to_owned())
            });
        for mut route in routes {
            route.source = source.clone();
            self.declared_routes.push(route);
        }
        self
    }

    /// Register an async startup hook that runs after [`AppState`] exists and
    /// before the server begins accepting requests.
    ///
    /// This is intended for background runtimes that need the fully built app
    /// state, such as workers or pollers that share the database pool.
    #[must_use]
    pub fn on_startup<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(AppState) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::AutumnResult<()>> + Send + 'static,
    {
        self.startup_hooks
            .push(Box::new(move |state| Box::pin(hook(state))));
        self
    }

    /// Register a synchronous initializer that mutates [`AppState`] after
    /// framework-managed extensions are installed and before job workers start.
    #[must_use]
    pub fn state_initializer<F>(mut self, initializer: F) -> Self
    where
        F: FnOnce(&AppState) + Send + 'static,
    {
        self.state_initializers.push(Box::new(initializer));
        self
    }

    /// Register an async shutdown hook that runs during graceful shutdown.
    ///
    /// Hooks execute in reverse registration order so later-added runtimes
    /// shut down before earlier infrastructure they might depend on.
    #[must_use]
    pub fn on_shutdown<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.shutdown_hooks.push(Box::new(move || Box::pin(hook())));
        self
    }

    /// Register a single API version. If a version with the same name already exists, it is updated.
    #[must_use]
    pub fn api_version(mut self, version: ApiVersion) -> Self {
        if let Some(pos) = self
            .api_versions
            .iter()
            .position(|v| v.version == version.version)
        {
            self.api_versions[pos] = version;
        } else {
            self.api_versions.push(version);
        }
        self
    }

    /// Register multiple API versions, replacing duplicates.
    #[must_use]
    pub fn api_versions(mut self, versions: impl IntoIterator<Item = ApiVersion>) -> Self {
        for version in versions {
            if let Some(pos) = self
                .api_versions
                .iter()
                .position(|v| v.version == version.version)
            {
                self.api_versions[pos] = version;
            } else {
                self.api_versions.push(version);
            }
        }
        self
    }

    /// Store or replace a typed builder extension.
    ///
    /// External crates use this to accumulate configuration across fluent
    /// extension-trait calls without Autumn needing to know the concrete type.
    #[must_use]
    pub fn with_extension<T>(mut self, value: T) -> Self
    where
        T: Any + Send + 'static,
    {
        self.extensions.insert(TypeId::of::<T>(), Box::new(value));
        self
    }

    /// Mutate a typed builder extension, inserting a default value first when
    /// the extension has not been registered yet.
    ///
    /// # Panics
    ///
    /// Panics if the internal extension type map is corrupted and the value
    /// stored under `T`'s [`TypeId`] cannot be downcast back to `T`.
    #[must_use]
    pub fn update_extension<T, Init, Update>(mut self, init: Init, update: Update) -> Self
    where
        T: Any + Send + 'static,
        Init: FnOnce() -> T,
        Update: FnOnce(&mut T),
    {
        let type_id = TypeId::of::<T>();
        let entry = self
            .extensions
            .entry(type_id)
            .or_insert_with(|| Box::new(init()));
        let typed = entry
            .downcast_mut::<T>()
            .expect("extension type map corrupted");
        update(typed);
        self
    }

    /// Borrow a typed builder extension if it has been registered.
    #[must_use]
    pub fn extension<T>(&self) -> Option<&T>
    where
        T: Any + Send + 'static,
    {
        self.extensions.get(&TypeId::of::<T>())?.downcast_ref::<T>()
    }

    #[cfg(feature = "mail")]
    #[must_use]
    pub fn with_mail_interceptor(
        mut self,
        interceptor: impl crate::interceptor::MailInterceptor,
    ) -> Self {
        self.mail_interceptor = Some(Arc::new(interceptor));
        self
    }

    #[must_use]
    pub fn with_job_interceptor(
        mut self,
        interceptor: impl crate::interceptor::JobInterceptor,
    ) -> Self {
        self.job_interceptor = Some(Arc::new(interceptor));
        self
    }

    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_db_interceptor(
        mut self,
        interceptor: impl crate::interceptor::DbConnectionInterceptor,
    ) -> Self {
        self.db_interceptor = Some(Arc::new(interceptor));
        self
    }

    #[cfg(feature = "ws")]
    #[must_use]
    pub fn with_channels_interceptor(
        mut self,
        interceptor: impl crate::interceptor::ChannelsInterceptor,
    ) -> Self {
        self.channels_interceptor = Some(Arc::new(interceptor));
        self
    }

    #[cfg(feature = "oauth2")]
    #[must_use]
    pub fn with_http_interceptor(
        mut self,
        interceptor: impl crate::interceptor::HttpInterceptor,
    ) -> Self {
        self.http_interceptor = Some(Arc::new(interceptor));
        self
    }

    /// Register a pre-loaded i18n translation bundle.
    ///
    /// Most apps prefer [`Self::i18n_auto`] which loads from the
    /// `i18n/` directory using the configured `[i18n]` block. Use this
    /// directly when you need to construct a [`Bundle`](crate::i18n::Bundle)
    /// from non-filesystem sources (in-memory tests, embedded `.ftl` files,
    /// translation-management-system clients, etc.).
    #[cfg(feature = "i18n")]
    #[must_use]
    pub fn i18n(mut self, bundle: crate::i18n::Bundle) -> Self {
        self.i18n_bundle = Some(Arc::new(bundle));
        self.i18n_auto_load = false;
        self
    }

    /// Auto-load the i18n translation bundle from the configured directory
    /// (`i18n/` by default), reading the `[i18n]` block from the active
    /// [`AutumnConfig`].
    ///
    /// Fails fast during [`Self::run`] if the configured default locale's file is
    /// missing — the spec calls out this as the desired behaviour: a
    /// half-localized app is worse than a clearly-broken one. The error
    /// path here panics with the typed [`LoadError`](crate::i18n::LoadError)
    /// formatted as a string so it surfaces in the same banner as other
    /// fatal startup errors.
    ///
    /// # Panics
    ///
    /// Panics when configuration cannot be loaded, the configured i18n
    /// directory is unreadable, or the default locale bundle is missing or
    /// invalid.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::prelude::*;
    ///
    /// #[get("/")]
    /// async fn index() -> &'static str { "ok" }
    ///
    /// #[autumn_web::main]
    /// async fn main() {
    ///     # #[cfg(feature = "i18n")]
    ///     autumn_web::app()
    ///         .i18n_auto()
    ///         .routes(routes![index])
    ///         .run()
    ///         .await;
    /// }
    /// ```
    #[cfg(feature = "i18n")]
    #[must_use]
    pub fn i18n_auto(mut self) -> Self {
        self.i18n_bundle = None;
        self.i18n_auto_load = true;
        self
    }

    // ── Tier-1 subsystem replacement hooks ─────────────────────
    //
    // Each `with_*` method swaps a framework-default subsystem for a
    // user-provided trait impl. The defaults preserve current behaviour, so
    // applications that don't customize see no change. Plugins typically chain
    // these in their `build()` body to ship a subsystem (e.g. an
    // `AwsSecretsConfigPlugin` that calls `app.with_config_loader(...)`).
    // See `docs/guides/extensibility.md`.

    /// Install a custom [`ConfigLoader`],
    /// replacing the default TOML + env loader.
    ///
    /// Useful when your config lives somewhere other than `autumn.toml` —
    /// AWS Secrets Manager, Vault, a JSON file, an HTTP fetch, etc. Emits a
    /// `tracing::warn!` if a loader was already installed.
    #[must_use]
    pub fn with_config_loader<L>(mut self, loader: L) -> Self
    where
        L: crate::config::ConfigLoader,
    {
        if self.config_loader_factory.is_some() {
            tracing::warn!(
                "config loader replaced; the previously-installed loader was overwritten"
            );
        }
        self.config_loader_factory = Some(Box::new(move || {
            Box::pin(async move { loader.load().await })
        }));
        self
    }

    /// Install a custom [`crate::db::DatabasePoolProvider`],
    /// replacing the default `deadpool + diesel-async` pool factory.
    ///
    /// Useful for adding metrics/circuit-breaker wrappers, switching to a
    /// per-shard pool, or driving a non-default backend at the same
    /// `Pool<AsyncPgConnection>` interface. Emits a `tracing::warn!` if a
    /// provider was already installed.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_pool_provider<P>(mut self, provider: P) -> Self
    where
        P: crate::db::DatabasePoolProvider,
    {
        if self.pool_provider_factory.is_some() {
            tracing::warn!(
                "database pool provider replaced; the previously-installed provider was overwritten"
            );
        }
        // The provider serves both the control topology and any configured
        // shard topologies; share it between the two captured closures.
        let provider = Arc::new(provider);
        let shard_provider = Arc::clone(&provider);
        self.pool_provider_factory =
            Some(Box::new(move |config: crate::config::DatabaseConfig| {
                Box::pin(async move { provider.create_topology(&config).await })
            }));
        self.shard_provider_factory =
            Some(Box::new(move |config: crate::config::DatabaseConfig| {
                Box::pin(async move {
                    let mut topologies = Vec::with_capacity(config.shards.len());
                    for shard in &config.shards {
                        topologies
                            .push(shard_provider.create_shard_topology(shard, &config).await?);
                    }
                    Ok(topologies)
                })
            }));
        self
    }

    /// Install a custom [`ShardRouter`](crate::sharding::ShardRouter),
    /// replacing the default slot-hash router for `[[database.shards]]`
    /// routing.
    ///
    /// Useful for directory/lookup routing — e.g. a control-plane table
    /// that pins hot tenants to dedicated shards. Custom routers can
    /// still compose with the deterministic hash via
    /// [`ShardSet::slot_for_key`](crate::sharding::ShardSet::slot_for_key)
    /// and
    /// [`ShardSet::shard_for_slot`](crate::sharding::ShardSet::shard_for_slot).
    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_shard_router<R>(mut self, router: R) -> Self
    where
        R: crate::sharding::ShardRouter,
    {
        if self.shard_router.is_some() {
            tracing::warn!(
                "shard router replaced; the previously-installed router was overwritten"
            );
        }
        self.shard_router = Some(Arc::new(router));
        self
    }

    /// Route tenants through the control-plane `_autumn_shard_directory` table
    /// via a [`DirectoryShardRouter`](crate::sharding::DirectoryShardRouter).
    ///
    /// The router is bound to the control primary pool at build time. Tenants
    /// with a directory row are pinned to the named shard; everyone else falls
    /// back to the slot-hash router. Apply the framework migrations to the
    /// control database (`autumn migrate`) so `_autumn_shard_directory` exists.
    ///
    /// An explicit [`with_shard_router`](Self::with_shard_router) takes
    /// precedence over this flag.
    #[cfg(feature = "db")]
    #[must_use]
    pub const fn with_directory_shard_router(mut self) -> Self {
        self.directory_shard_router = true;
        self
    }

    /// Install a custom [`TelemetryProvider`](crate::telemetry::TelemetryProvider),
    /// replacing the default `tracing-subscriber + OTLP` initializer.
    ///
    /// Useful for shipping a Datadog tracer, Honeycomb beeline, Sentry
    /// integration, or any other observability backend. Emits a
    /// `tracing::warn!` if a provider was already installed.
    #[must_use]
    pub fn with_telemetry_provider<T>(mut self, provider: T) -> Self
    where
        T: crate::telemetry::TelemetryProvider,
    {
        if self.telemetry_provider.is_some() {
            tracing::warn!(
                "telemetry provider replaced; the previously-installed provider was overwritten"
            );
        }
        self.telemetry_provider = Some(Box::new(provider));
        self
    }

    /// Install a custom [`SessionStore`](crate::session::SessionStore),
    /// bypassing the config-driven `memory`/`redis` backend selection.
    ///
    /// Useful for backing sessions with a database, encrypted cookie store,
    /// or enterprise SSO bridge. Emits a `tracing::warn!` if a store was
    /// already installed.
    #[must_use]
    pub fn with_session_store<S>(mut self, store: S) -> Self
    where
        S: crate::session::SessionStore,
    {
        if self.session_store.is_some() {
            tracing::warn!(
                "session store replaced; the previously-installed store was overwritten"
            );
        }
        self.session_store = Some(Arc::new(store));
        self
    }

    /// Install a custom [`ChannelsBackend`](crate::channels::ChannelsBackend),
    /// bypassing the config-driven `in_process`/`redis` backend selection.
    ///
    /// Useful for NATS, Postgres `LISTEN/NOTIFY`, test harnesses, or a
    /// sharded pub/sub fabric. Emits a `tracing::warn!` if a backend was
    /// already installed.
    #[cfg(feature = "ws")]
    #[must_use]
    pub fn with_channels_backend<B>(mut self, backend: B) -> Self
    where
        B: crate::channels::ChannelsBackend,
    {
        if self.channels_backend.is_some() {
            tracing::warn!(
                "channels backend replaced; the previously-installed backend was overwritten"
            );
        }
        self.channels_backend = Some(Arc::new(backend));
        self
    }

    /// Install a custom [`BlobStore`](crate::storage::BlobStore),
    /// bypassing the config-driven `local`/`s3` backend selection.
    ///
    /// The typical use case is the `autumn-storage-s3` plugin:
    ///
    /// ```rust,ignore
    /// use autumn_storage_s3::S3BlobStore;
    ///
    /// # async fn example() {
    /// let config = autumn_web::config::TomlEnvConfigLoader::new()
    ///     .load().await.unwrap();
    /// let store = S3BlobStore::from_config(&config.storage.s3)
    ///     .await.unwrap();
    /// autumn_web::app()
    ///     .with_blob_store(store)
    ///     .run()
    ///     .await;
    /// # }
    /// ```
    ///
    /// Emits a `tracing::warn!` if a store was already installed (last
    /// call wins).
    ///
    /// # Note on `LocalBlobStore`
    ///
    /// **Do not** pass a [`LocalBlobStore`](crate::storage::LocalBlobStore)
    /// here. The local backend requires the framework to mount a `/_blobs`
    /// serving route (for HMAC-signed presigned URLs); that route is only
    /// wired up when the store is provisioned through the config-driven path
    /// (`backend = "local"` in `autumn.toml`). Calling
    /// `.with_blob_store(LocalBlobStore::new(...))` will silently succeed but
    /// presigned URLs will return 404. Use the `[storage]` config section for
    /// local storage.
    #[cfg(feature = "storage")]
    #[must_use]
    pub fn with_blob_store<B>(mut self, store: B) -> Self
    where
        B: crate::storage::BlobStore,
    {
        if self.blob_store.is_some() {
            tracing::warn!("blob store replaced; the previously-installed store was overwritten");
        }
        self.blob_store = Some(std::sync::Arc::new(store));
        self
    }

    /// Register a shared cache backend for the application.
    ///
    /// Once registered, `#[cached]` functions will use this backend as their
    /// primary store (falling back to their per-function Moka cache only if the
    /// global backend is absent). `CacheResponseLayer::from_app` returns a layer
    /// wired to this same backend.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use autumn_cache_redis::RedisCache;
    ///
    /// let cache = RedisCache::connect("redis://redis:6379", "myapp:cache").await?;
    /// autumn_web::app()
    ///     .with_cache_backend(cache)
    ///     .run()
    ///     .await;
    /// ```
    #[must_use]
    pub fn with_cache_backend<C: crate::cache::Cache>(mut self, cache: C) -> Self {
        if self.cache_backend.is_some() {
            tracing::warn!(
                "cache backend replaced; the previously-installed backend was overwritten"
            );
        }
        self.cache_backend = Some(Arc::new(cache) as Arc<dyn crate::cache::Cache>);
        self
    }

    /// Register an [`ErrorReporter`](crate::reporting::ErrorReporter) for
    /// unhandled panics and 5xx responses.
    ///
    /// Reporters receive a structured
    /// [`ErrorEvent`](crate::reporting::ErrorEvent) for every caught handler
    /// panic and every server-error response, carrying request context (route,
    /// method, request id, status) and — for panics — the panic payload and a
    /// backtrace (when `RUST_BACKTRACE` is set). Call this multiple times to
    /// chain reporters; each receives every event. When none are registered,
    /// the built-in [`LogReporter`](crate::reporting::LogReporter) is used.
    ///
    /// Mirrors [`with_blob_store`](Self::with_blob_store) /
    /// [`with_cache_backend`](Self::with_cache_backend).
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::reporting::{ErrorEvent, ErrorReporter, ReportFuture};
    ///
    /// struct MyReporter;
    /// impl ErrorReporter for MyReporter {
    ///     fn report<'a>(&'a self, event: &'a ErrorEvent) -> ReportFuture<'a> {
    ///         Box::pin(async move { eprintln!("error: {} {}", event.status, event.message); })
    ///     }
    /// }
    ///
    /// # #[autumn_web::main]
    /// # async fn main() {
    /// autumn_web::app()
    ///     .with_error_reporter(MyReporter)
    /// #   .routes(vec![])
    /// #   ;
    /// # }
    /// ```
    #[cfg(feature = "reporting")]
    #[must_use]
    pub fn with_error_reporter<R: crate::reporting::ErrorReporter>(mut self, reporter: R) -> Self {
        self.error_reporters
            .push(Arc::new(reporter) as Arc<dyn crate::reporting::ErrorReporter>);
        self
    }

    /// Register a [`FlagStore`](crate::feature_flags::FlagStore) backend for
    /// feature-flag evaluation.
    ///
    /// After registration, the [`Flags`](crate::feature_flags::Flags) extractor
    /// and `#[feature_flag]` macro are available in route handlers. Without a
    /// registered store, both return `500 Internal Server Error`.
    ///
    /// For tests use [`InMemoryFlagStore`](crate::feature_flags::InMemoryFlagStore);
    /// in production use the Postgres-backed
    /// `autumn_web::feature_flags::pg::PgFlagStore`.
    ///
    /// # Sharing the store with the poll listener
    ///
    /// When using `PgFlagStore` in a multi-replica deployment, pass an `Arc`
    /// clone so the app service and the poll listener share the **same** cache:
    ///
    /// ```rust,ignore
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use autumn_web::feature_flags::pg::PgFlagStore;
    ///
    /// let store = Arc::new(PgFlagStore::new(&config.database.primary_url));
    /// PgFlagStore::spawn_poll_listener(Arc::clone(&store), Duration::from_secs(1));
    /// autumn_web::app()
    ///     .with_flag_store(Arc::clone(&store))
    ///     .run()
    ///     .await;
    /// ```
    ///
    /// `Arc<PgFlagStore>` implements `FlagStore`, so the same `Arc` is
    /// accepted directly without creating a separate cache instance.
    ///
    /// # Basic example
    ///
    /// ```rust,ignore
    /// use autumn_web::feature_flags::InMemoryFlagStore;
    /// use std::sync::Arc;
    ///
    /// autumn_web::app()
    ///     .with_flag_store(InMemoryFlagStore::new())
    ///     .run()
    ///     .await;
    /// ```
    #[must_use]
    pub fn with_flag_store<S>(self, store: S) -> Self
    where
        S: crate::feature_flags::FlagStore,
    {
        let service = crate::feature_flags::FeatureFlagService::new(Arc::new(store) as Arc<_>);
        self.state_initializer(move |state| {
            state.insert_extension(service);
        })
    }

    /// Register a feature-flag store with a group-membership resolver.
    ///
    /// The resolver is called during flag evaluation to check whether an actor
    /// belongs to a named group listed in a flag's `group_allowlist`. Without
    /// registering a resolver, group gates are silently ignored.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use autumn_web::feature_flags::{InMemoryFlagStore, GroupResolver};
    /// use std::sync::Arc;
    ///
    /// let resolver: GroupResolver = Arc::new(|actor_id, group| {
    ///     group == "staff" && actor_id.starts_with("staff:")
    /// });
    ///
    /// autumn_web::app()
    ///     .with_flag_store_and_resolver(InMemoryFlagStore::new(), resolver)
    ///     .run()
    ///     .await;
    /// ```
    #[must_use]
    pub fn with_flag_store_and_resolver<S>(
        self,
        store: S,
        resolver: crate::feature_flags::GroupResolver,
    ) -> Self
    where
        S: crate::feature_flags::FlagStore,
    {
        let service = crate::feature_flags::FeatureFlagService::new(Arc::new(store) as Arc<_>)
            .with_group_resolver(resolver);
        self.state_initializer(move |state| {
            state.insert_extension(service);
        })
    }

    /// Register an experiment store, enabling the [`Experiments`] extractor.
    ///
    /// Wrap any [`ExperimentStore`] implementation. Use [`InMemoryExperimentStore`]
    /// for development and tests; use
    /// [`pg::PgExperimentStore`](crate::experiments::pg::PgExperimentStore)
    /// for production against the `autumn_experiments` tables.
    ///
    /// # Production example (Postgres-backed)
    ///
    /// ```rust,ignore
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use autumn_web::experiments::pg::PgExperimentStore;
    ///
    /// let store = Arc::new(PgExperimentStore::new(&config.database.primary_url));
    /// PgExperimentStore::spawn_poll_listener(Arc::clone(&store), Duration::from_secs(5));
    /// autumn_web::app()
    ///     .with_experiment_store(Arc::clone(&store))
    ///     .run()
    ///     .await;
    /// ```
    ///
    /// # Development / test example
    ///
    /// ```rust,ignore
    /// use autumn_web::experiments::InMemoryExperimentStore;
    ///
    /// autumn_web::app()
    ///     .with_experiment_store(InMemoryExperimentStore::new())
    ///     .run()
    ///     .await;
    /// ```
    ///
    /// [`Experiments`]: crate::experiments::Experiments
    /// [`ExperimentStore`]: crate::experiments::ExperimentStore
    /// [`InMemoryExperimentStore`]: crate::experiments::InMemoryExperimentStore
    #[must_use]
    pub fn with_experiment_store<S>(self, store: S) -> Self
    where
        S: crate::experiments::ExperimentStore,
    {
        let service = crate::experiments::ExperimentService::new(Arc::new(store) as Arc<_>);
        self.state_initializer(move |state| {
            state.insert_extension(service);
        })
    }

    /// Register an experiment store with a custom [`ExposureSink`].
    ///
    /// Use when you want to forward exposure events to an analytics pipeline
    /// rather than the default `tracing` log.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use autumn_web::experiments::{InMemoryExperimentStore, NoOpExposureSink};
    /// use std::sync::Arc;
    ///
    /// autumn_web::app()
    ///     .with_experiment_store_and_sink(
    ///         InMemoryExperimentStore::new(),
    ///         Arc::new(NoOpExposureSink),
    ///     )
    ///     .run()
    ///     .await;
    /// ```
    ///
    /// [`ExposureSink`]: crate::experiments::ExposureSink
    #[must_use]
    pub fn with_experiment_store_and_sink<S>(
        self,
        store: S,
        sink: Arc<dyn crate::experiments::ExposureSink>,
    ) -> Self
    where
        S: crate::experiments::ExperimentStore,
    {
        let service = crate::experiments::ExperimentService::new(Arc::new(store) as Arc<_>)
            .with_exposure_sink(sink);
        self.state_initializer(move |state| {
            state.insert_extension(service);
        })
    }

    /// Register a durable [`MailDeliveryQueue`](crate::mail::MailDeliveryQueue) for
    /// [`Mailer::deliver_later`](crate::mail::Mailer::deliver_later).
    ///
    /// Must be called before [`run`](Self::run). Plugins call this inside their
    /// `apply` implementation to satisfy the production delivery guard without
    /// requiring `mail.allow_in_process_deliver_later_in_production`.
    ///
    /// Use [`Self::with_mail_delivery_queue_factory`] when the queue needs
    /// framework-managed resources (the DB pool, channels, etc.) that only
    /// exist after the [`AppState`] is constructed.
    #[cfg(feature = "mail")]
    #[must_use]
    pub fn with_mail_delivery_queue(
        mut self,
        queue: impl crate::mail::MailDeliveryQueue + 'static,
    ) -> Self {
        let arc: Arc<dyn crate::mail::MailDeliveryQueue> = Arc::new(queue);
        self.mail_delivery_queue_factory = Some(Box::new(move |_state| Ok(arc)));
        self
    }

    /// Register a factory that builds the durable
    /// [`MailDeliveryQueue`](crate::mail::MailDeliveryQueue) from the
    /// fully-built [`AppState`].
    ///
    /// Use this when the queue captures framework-managed resources — for
    /// example a DB-outbox queue that needs the connection pool returned by
    /// [`AppState::pool`]. The factory runs once, immediately before
    /// `install_mailer`, with the live `AppState`. Returning `Err` aborts
    /// startup with the propagated error.
    #[cfg(feature = "mail")]
    #[must_use]
    pub fn with_mail_delivery_queue_factory<F, Q>(mut self, factory: F) -> Self
    where
        F: FnOnce(&AppState) -> crate::AutumnResult<Q> + Send + 'static,
        Q: crate::mail::MailDeliveryQueue + 'static,
    {
        self.mail_delivery_queue_factory = Some(Box::new(move |state| {
            factory(state).map(|q| Arc::new(q) as Arc<dyn crate::mail::MailDeliveryQueue>)
        }));
        self
    }

    /// Register a [`SuppressionStore`](crate::mail::SuppressionStore) used by
    /// List-Unsubscribe sends to skip opted-out recipients and by the default
    /// unsubscribe endpoint to record opt-outs.
    ///
    /// When the `db` feature is enabled and a connection pool is configured, a
    /// Diesel-backed store is auto-wired, so most apps never call this — use it
    /// to plug a custom backend. Mirrors
    /// [`Self::with_mail_delivery_queue`].
    #[cfg(feature = "mail")]
    #[must_use]
    pub fn with_suppression_store(
        mut self,
        store: impl crate::mail::SuppressionStore + 'static,
    ) -> Self {
        self.suppression_store = Some(crate::mail::SuppressionStoreHandle::new(store));
        self
    }

    /// Mount the framework's default RFC 8058 one-click unsubscribe endpoint at
    /// `/_autumn/unsubscribe` (`GET` confirmation page + `POST` one-click).
    ///
    /// Opt-in: a plain JSON API never gets an HTML endpoint it didn't ask for.
    /// Requires `mail.unsubscribe_base_url` to be configured. When mounted, the
    /// path is automatically exempted from CSRF and CAPTCHA (mailbox-provider
    /// POSTs carry neither token). To serve a custom unsubscribe page instead,
    /// skip this and register your own route at the path.
    #[cfg(feature = "mail")]
    #[must_use]
    pub const fn mount_unsubscribe_endpoint(mut self) -> Self {
        self.mount_unsubscribe_endpoint = true;
        self
    }

    /// Register an inbound mail router that creates webhook HTTP endpoints and
    /// dispatches parsed [`InboundEmail`](crate::inbound_mail::InboundEmail)
    /// values to registered handlers.
    ///
    /// Calling this method twice replaces the previously registered router.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use autumn_web::inbound_mail::{
    ///     InboundMailRouter, InboundMailEndpointConfig,
    ///     InboundMailHandlerInfo, ProcessingMode, RecipientPattern,
    /// };
    ///
    /// autumn_web::app()
    ///     .inbound_mail_router(
    ///         InboundMailRouter::new()
    ///             .endpoint(InboundMailEndpointConfig::mailgun("/inbound/mailgun", "key"))
    ///             .handler(InboundMailHandlerInfo {
    ///                 name: "support",
    ///                 pattern: RecipientPattern::Exact("support@company.com".to_string()),
    ///                 processing: ProcessingMode::Background,
    ///                 handler: handle_support,
    ///             })
    ///     )
    ///     .routes(routes![...])
    ///     .run()
    ///     .await;
    /// ```
    #[cfg(feature = "inbound-mail")]
    #[must_use]
    pub fn inbound_mail_router(mut self, router: crate::inbound_mail::InboundMailRouter) -> Self {
        self.inbound_mail_router = Some(Arc::new(router));
        self
    }

    /// Register mail template previews for the dev mail preview UI.
    ///
    /// Pair this with `#[mailer_preview]` and `mail_previews![...]`.
    #[cfg(feature = "mail")]
    #[must_use]
    pub fn mail_previews(
        mut self,
        previews: impl IntoIterator<Item = crate::mail::MailPreview>,
    ) -> Self {
        self.mail_previews.extend(previews);
        self
    }

    /// Register an additional audit sink for structured audit events.
    ///
    /// Multiple calls accumulate sinks. Logged events are fanned out to all
    /// configured sinks.
    #[must_use]
    pub fn with_audit_sink<S>(mut self, sink: S) -> Self
    where
        S: crate::audit::AuditSink,
    {
        let logger = self
            .audit_logger
            .take()
            .map_or_else(crate::audit::AuditLogger::new, |logger| (*logger).clone())
            .with_sink(Arc::new(sink));
        self.audit_logger = Some(Arc::new(logger));
        self
    }

    /// Register a [`Policy`](crate::authorization::Policy)
    /// implementation for resource type `R`.
    ///
    /// Multiple policies per resource are not supported: registering
    /// `R` twice causes a startup-time panic with a clear error
    /// message.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use autumn_web::authorization::{Policy, PolicyContext};
    ///
    /// #[derive(Default)]
    /// struct PostPolicy;
    /// impl Policy<Post> for PostPolicy { /* ... */ }
    ///
    /// autumn_web::app()
    ///     .routes(routes![...])
    ///     .policy::<Post, _>(PostPolicy)
    ///     .run()
    ///     .await;
    /// ```
    #[must_use]
    pub fn policy<R, P>(mut self, policy: P) -> Self
    where
        R: Send + Sync + 'static,
        P: crate::authorization::Policy<R>,
    {
        self.policy_registrations.push(Box::new(move |registry| {
            registry.register_policy::<R, _>(policy);
        }));
        self
    }

    /// Register a [`Scope`](crate::authorization::Scope) implementation
    /// for resource type `R`. The scope filters list endpoints
    /// (`GET /<api>` for `#[repository(api = "...", scope = ...)]`)
    /// to records the current user is allowed to read.
    ///
    /// Default impls return an empty list so a missing scope opt-in
    /// fails closed.
    #[must_use]
    pub fn scope<R, S>(mut self, scope: S) -> Self
    where
        R: Send + Sync + 'static,
        S: crate::authorization::Scope<R>,
    {
        self.policy_registrations.push(Box::new(move |registry| {
            registry.register_scope::<R, _>(scope);
        }));
        self
    }

    /// Apply a [`Plugin`](crate::plugin::Plugin) to the builder.
    ///
    /// The plugin's [`build`](crate::plugin::Plugin::build) runs exactly once
    /// per [`AppBuilder`]. Registering two plugins that share a
    /// [`name`](crate::plugin::Plugin::name) is a no-op after the first: the
    /// duplicate emits a `tracing::warn!` and the builder is returned
    /// unchanged.
    #[must_use]
    #[track_caller]
    pub fn plugin<P>(mut self, plugin: P) -> Self
    where
        P: crate::plugin::Plugin,
    {
        let name = plugin.name();
        if self.registered_plugins.contains(name.as_ref()) {
            tracing::warn!(
                plugin = name.as_ref(),
                "plugin already registered; skipping duplicate"
            );
            return self;
        }
        let name_str = name.into_owned();
        self.registered_plugins.insert(name_str.clone());
        // Save outer plugin context so nested plugin() calls don't permanently
        // clear it; restore it after this plugin's build() returns.
        let outer_plugin = self.current_plugin.replace(name_str);
        let mut result = plugin.build(self);
        result.current_plugin = outer_plugin;
        result
    }

    /// Apply a [`Plugins`](crate::plugin::Plugins) bundle (a plugin or tuple
    /// of plugins) to the builder, in declaration order.
    #[must_use]
    pub fn plugins<P>(self, plugins: P) -> Self
    where
        P: crate::plugin::Plugins,
    {
        plugins.apply(self)
    }

    /// Return `true` if a plugin with the given [`Plugin::name`](crate::plugin::Plugin::name)
    /// has already been applied to this builder.
    #[must_use]
    pub fn has_plugin(&self, name: &str) -> bool {
        self.registered_plugins.contains(name)
    }

    /// Register a named [`MetricsSource`](crate::actuator::MetricsSource) that contributes
    /// metric families to `/actuator/prometheus` and `/actuator/metrics`.
    ///
    /// The `name` is a stable identifier used for:
    /// - Duplicate-registration detection (same behaviour as duplicate plugins: a
    ///   `tracing::warn!` is emitted and the second registration is skipped).
    /// - The `source` label in the `autumn_metrics_source_errors_total` counter
    ///   that increments when a source panics during a scrape.
    ///
    /// `Plugin::build` implementations can call this to wire a source with no
    /// extra application-level glue code.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::actuator::{MetricsSource, MetricFamily, MetricKind, MetricSample};
    /// use autumn_web::app::AppBuilder;
    /// use std::sync::Arc;
    ///
    /// struct QueueMetrics;
    ///
    /// impl MetricsSource for QueueMetrics {
    ///     fn collect(&self) -> Vec<MetricFamily> {
    ///         vec![MetricFamily {
    ///             name: "myapp_queue_depth".to_string(),
    ///             help: "Current queue depth".to_string(),
    ///             kind: MetricKind::Gauge,
    ///             samples: vec![MetricSample { labels: vec![], value: 42.0 }],
    ///         }]
    ///     }
    /// }
    ///
    /// autumn_web::app()
    ///     .metrics_source("myapp_queue", Arc::new(QueueMetrics));
    /// ```
    #[must_use]
    pub fn metrics_source(
        mut self,
        name: impl Into<String>,
        source: Arc<dyn crate::actuator::MetricsSource>,
    ) -> Self {
        let name = name.into();
        if self.metrics_sources.iter().any(|(n, _)| n == &name) {
            tracing::warn!(
                source_name = %name,
                "MetricsSource '{}' is already registered; skipping duplicate",
                name
            );
            return self;
        }
        self.metrics_sources.push((name, source));
        self
    }

    /// Register a custom [`HealthIndicator`](crate::actuator::HealthIndicator) with the application.
    ///
    /// The indicator's [`check`](crate::actuator::HealthIndicator::check) method is called on every
    /// `/actuator/health` request (and on `/ready` for `Readiness`-group indicators).
    ///
    /// Duplicate registration names are silently ignored (a warning is logged).
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use autumn_web::actuator::{HealthCheckOutput, HealthIndicator};
    ///
    /// struct StripeIndicator;
    /// impl HealthIndicator for StripeIndicator {
    ///     fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
    ///         Box::pin(async move { HealthCheckOutput::up() })
    ///     }
    /// }
    ///
    /// autumn_web::app()
    ///     .health_indicator("stripe", Arc::new(StripeIndicator));
    /// ```
    #[must_use]
    pub fn health_indicator(
        mut self,
        name: impl Into<String>,
        indicator: Arc<dyn crate::actuator::HealthIndicator>,
    ) -> Self {
        let name = name.into();
        // "db" is a reserved built-in component name. Allowing a custom indicator
        // under this name would produce an inconsistent response: the custom result
        // would still gate the aggregate status while the built-in pool check owns
        // the components.db / checks.database display. The "db:shard:" prefix is
        // reserved for the framework's per-shard indicators for the same reason.
        #[cfg(feature = "db")]
        if name == "db" || name.starts_with("db:shard:") {
            tracing::warn!(
                indicator_name = %name,
                "\"db\" and \"db:shard:*\" are reserved built-in health indicator names; \
                 registration skipped. Use a different name for your custom indicator."
            );
            return self;
        }
        if self.health_indicators.iter().any(|(n, _, _)| n == &name) {
            tracing::warn!(
                indicator_name = %name,
                "HealthIndicator '{}' is already registered; skipping duplicate",
                name
            );
            return self;
        }
        let group = indicator.group();
        self.health_indicators.push((name, group, indicator));
        self
    }

    /// Register embedded Diesel migrations with the application.
    ///
    /// When migrations are registered:
    /// - They always target the primary/write database role
    ///   (`database.primary_url`, falling back to legacy `database.url`).
    /// - In **dev** mode, pending migrations run automatically on startup.
    /// - In **prod** mode, pending migrations are logged as warnings but
    ///   not applied -- use a one-shot `autumn migrate` job before rolling web
    ///   replicas.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
    ///
    /// const MIGRATIONS: EmbeddedMigrations = embed_migrations!();
    ///
    /// #[autumn_web::main]
    /// async fn main() {
    ///     autumn_web::app()
    ///         .routes(routes![...])
    ///         .migrations(MIGRATIONS)
    ///         .run()
    ///         .await;
    /// }
    /// ```
    #[cfg(feature = "db")]
    #[must_use]
    pub fn migrations(mut self, migrations: migrate::EmbeddedMigrations) -> Self {
        self.migrations.push(migrations);
        self
    }

    /// Embed the app's `static/` tree into the binary for single-binary deploys.
    ///
    /// Pass the directory produced by [`embed_static!`](crate::embed_static)
    /// (requires the `embed-assets` feature). When set, `/static/*` is served
    /// from the binary and `asset_url()` resolves against the embedded
    /// fingerprint manifest — copying only the release binary into an empty
    /// directory serves every referenced asset with no `static/` sidecar.
    /// Because the manifest and the files are baked from the same build,
    /// fingerprint-vs-manifest drift is impossible.
    ///
    /// This is a release-time concern: leave it unset in development so CSS/JS
    /// hot-reload keeps serving from disk.
    ///
    /// ```rust,ignore
    /// static STATIC: autumn_web::include_dir::Dir = autumn_web::embed_static!();
    ///
    /// #[autumn_web::main]
    /// async fn main() {
    ///     autumn_web::app().embedded_static(&STATIC).run().await;
    /// }
    /// ```
    #[cfg(feature = "embed-assets")]
    #[must_use]
    pub const fn embedded_static(mut self, dir: &'static include_dir::Dir<'static>) -> Self {
        self.embedded_static = Some(crate::assets::EmbeddedStaticDir(dir));
        self
    }

    /// Embed the app's i18n locale bundles into the binary.
    ///
    /// Pass the directory produced by [`embed_locales!`](crate::embed_locales)
    /// (requires the `embed-assets` and `i18n` features). When set (and no
    /// explicit [`i18n`](AppBuilder::i18n) bundle was provided), all configured
    /// locales render from the binary with no `i18n/` sidecar directory.
    ///
    /// ```rust,ignore
    /// static LOCALES: autumn_web::include_dir::Dir = autumn_web::embed_locales!();
    ///
    /// #[autumn_web::main]
    /// async fn main() {
    ///     autumn_web::app().embedded_locales(&LOCALES).run().await;
    /// }
    /// ```
    #[cfg(all(feature = "embed-assets", feature = "i18n"))]
    #[must_use]
    pub const fn embedded_locales(mut self, dir: &'static include_dir::Dir<'static>) -> Self {
        self.embedded_locales = Some(dir);
        self
    }

    /// Start the HTTP server.
    ///
    /// This method performs the full application lifecycle:
    ///
    /// 1. Loads configuration from `autumn.toml` (or defaults).
    /// 2. Initializes the tracing subscriber.
    /// 3. Validates that at least one route is registered.
    /// 4. Creates the database connection pool (if configured).
    /// 5. Builds the Axum router from collected routes.
    /// 6. Mounts built-in routes (health check, htmx JS, static files).
    /// 7. Binds to the configured address and port.
    /// 8. Serves requests with graceful shutdown on Ctrl+C (or `SIGTERM`
    ///    on Unix).
    ///
    /// # Panics
    ///
    /// Panics if no routes have been registered via [`.routes()`](Self::routes).
    /// This is intentional -- an application with no routes is always a
    /// developer error.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::cognitive_complexity)]
    pub async fn run(self) {
        // ── Build mode ─────────────────────────────────────────────────
        // When AUTUMN_BUILD_STATIC=1, render static routes to dist/ and exit
        // instead of starting the HTTP server. This is triggered by `autumn build`.
        if is_static_build_mode() {
            self.run_build_mode().await;
            return;
        }

        // ── Route dump mode ────────────────────────────────────────────
        // When AUTUMN_DUMP_ROUTES=1, print the route listing JSON and exit.
        // This is triggered by `autumn routes` to introspect the app's
        // route table without booting the server or connecting to a database.
        if is_dump_routes_mode() {
            self.run_dump_routes_mode().await;
            return;
        }

        if is_list_one_off_tasks_mode() {
            self.run_list_one_off_tasks_mode();
            return;
        }

        if let Some(task_name) = one_off_task_name_from_env() {
            self.run_one_off_task_mode(task_name).await;
            return;
        }

        let Self {
            routes,
            api_versions,
            route_sources: _,
            current_plugin: _,
            tasks,
            one_off_tasks: _,
            mut jobs,
            listeners,
            static_metas,
            exception_filters,
            scoped_groups,
            merge_routers,
            nest_routers,
            custom_layers,
            static_gate_layers,
            startup_hooks,
            state_initializers,
            shutdown_hooks,
            extensions: _,
            registered_plugins: _,
            #[cfg(feature = "maud")]
            error_page_renderer,
            #[cfg(feature = "db")]
            migrations,
            config_loader_factory,
            #[cfg(feature = "db")]
            pool_provider_factory,
            #[cfg(feature = "db")]
            shard_provider_factory,
            #[cfg(feature = "db")]
            shard_router,
            #[cfg(feature = "db")]
            directory_shard_router,
            telemetry_provider,
            session_store,
            #[cfg(feature = "ws")]
            channels_backend,
            #[cfg(feature = "storage")]
            blob_store,
            cache_backend,
            #[cfg(feature = "reporting")]
            error_reporters,
            #[cfg(feature = "openapi")]
            openapi,
            #[cfg(feature = "mcp")]
            mcp,
            audit_logger,
            #[cfg(feature = "i18n")]
            i18n_bundle,
            #[cfg(feature = "i18n")]
            i18n_auto_load,
            #[cfg(feature = "embed-assets")]
            embedded_static,
            #[cfg(all(feature = "embed-assets", feature = "i18n"))]
            embedded_locales,
            policy_registrations,
            #[cfg(feature = "mail")]
            mail_delivery_queue_factory,
            #[cfg(feature = "mail")]
            suppression_store,
            #[cfg(feature = "mail")]
            mount_unsubscribe_endpoint,
            #[cfg(feature = "mail")]
            mail_previews,
            declared_routes: _,
            idempotency_enabled,
            #[cfg(feature = "mail")]
            mail_interceptor,
            job_interceptor,
            #[cfg(feature = "db")]
            db_interceptor,
            #[cfg(feature = "ws")]
            channels_interceptor,
            #[cfg(feature = "oauth2")]
            http_interceptor,
            seo_sources,
            metrics_sources,
            health_indicators,
            #[cfg(feature = "inbound-mail")]
            inbound_mail_router,
        } = self;

        let all_routes = routes;

        // 1 & 2. Load configuration and initialize logging/telemetry
        let (mut config, telemetry_guard) =
            load_config_and_telemetry(config_loader_factory, telemetry_provider).await;

        #[cfg(feature = "mail")]
        if mount_unsubscribe_endpoint {
            config.mail.mount_unsubscribe_endpoint = true;
        }

        // Apply builder-level flag: `.idempotent()` enables the middleware when
        // neither `autumn.toml` nor the environment explicitly disable it.
        // The env var `AUTUMN_IDEMPOTENCY__ENABLED` is re-checked here so
        // operators can disable idempotency at runtime (e.g. during a Redis
        // incident) without code changes, even when `.idempotent()` is called.
        if idempotency_enabled {
            let env_disabled = std::env::var("AUTUMN_IDEMPOTENCY__ENABLED")
                .is_ok_and(|v| matches!(v.to_lowercase().as_str(), "false" | "0" | "no" | "off"));
            // Only apply the builder default when neither the env var nor the
            // loaded config file explicitly sets enabled = false.
            if !env_disabled && config.idempotency.enabled != Some(false) {
                config.idempotency.enabled = Some(true);
            }
        }

        // Register the embedded `static/` tree (if any) before the router is
        // built so `/static/*` serves from the binary and `asset_url()` resolves
        // against the embedded manifest, then prefer embedded locales over disk
        // auto-loading when no explicit bundle was provided.
        #[cfg(feature = "embed-assets")]
        register_embedded_static_dir(embedded_static);

        #[cfg(all(feature = "embed-assets", feature = "i18n"))]
        let i18n_bundle = embedded_i18n_bundle(i18n_bundle, embedded_locales, &config);

        #[cfg(feature = "i18n")]
        let i18n_bundle =
            resolve_i18n_bundle(i18n_bundle, i18n_auto_load, &config, &crate::config::OsEnv);

        // 3. Validate routes
        assert!(
            !all_routes.is_empty(),
            "No routes registered. Did you forget to call .routes()?"
        );

        // 4. Log banner with profile info
        let profile_display = config.profile.as_deref().unwrap_or("none");
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            profile = profile_display,
            "Autumn starting"
        );

        // 4b. Startup transparency log (AUTUMN_SHOW_CONFIG=1 or log level <= DEBUG)
        let show_config = std::env::var("AUTUMN_SHOW_CONFIG").as_deref() == Ok("1");
        if show_config {
            log_startup_transparency(&all_routes, &tasks, &scoped_groups, &config);
        }

        // 4c. Fail-fast on invalid session config — but only when no custom
        // SessionStore was installed via with_session_store(...). Done before
        // setup_database so a doomed boot doesn't run migrations first.
        fail_fast_on_invalid_session_config(&config, session_store.is_some());

        // 4d. Validate signing secret — production must have a stable, private,
        // entropy-meeting secret before the server binds. Dev/test are exempt.
        fail_fast_on_invalid_signing_secret(&config);
        fail_fast_on_missing_encryption_keys(&config);
        fail_fast_on_invalid_trusted_hosts(&config);

        // 4e. Signed webhook configs must resolve to usable key material
        // before the app binds. Missing secrets should fail before a real
        // provider retry loop starts hammering a broken endpoint.
        fail_fast_on_invalid_webhook_config(&config);

        // 4f. Idempotency backend must be production-ready when enabled.
        fail_fast_on_invalid_idempotency_config(&config);

        // 4f. Provision the configured BlobStore *before* `setup_database`.
        // `LocalBlobStore::new` does real IO (creates + canonicalizes the
        // root) and the storage code may `process::exit(1)` on failure
        // (unwritable root, or `storage.backend = "s3"` with no plugin).
        // Doing it before migrations means a doomed boot can't mutate
        // the DB schema first.
        // A custom store installed via `.with_blob_store(...)` bypasses
        // config-driven instantiation entirely (no IO, no fail-fast).
        #[cfg(feature = "storage")]
        let storage_bootstrap = blob_store.map_or_else(
            || preflight_storage(&config),
            |store| {
                Some(StorageBootstrap {
                    store,
                    serving: None,
                })
            },
        );

        // 5. Create database pool and run migrations (if configured)
        #[cfg(feature = "db")]
        let database = setup_database(
            &config,
            migrations,
            pool_provider_factory,
            shard_provider_factory,
            shard_router,
            directory_shard_router,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("{e}");
            std::process::exit(1);
        });
        #[cfg(feature = "db")]
        let pool = database.topology;
        #[cfg(feature = "db")]
        let shards = database.shards;
        #[cfg(feature = "db")]
        let replica_readiness = database.replica_readiness;
        #[cfg(feature = "db")]
        let replica_migration_check = database.replica_migration_check;

        #[cfg(feature = "db")]
        if pool.is_some() || shards.is_some() {
            // Pool sizes multiply across shards: surface the total so
            // N-shard deployments notice the aggregate connection count.
            let shard_max_connections = shards
                .as_ref()
                .map_or(0, crate::sharding::ShardSet::total_max_connections);
            let control_max_connections = pool.as_ref().map_or(0, |topology| {
                topology.primary().status().max_size
                    + topology.replica().map_or(0, |p| p.status().max_size)
            });
            let total_max_connections = control_max_connections + shard_max_connections;
            tracing::info!(
                primary_max_connections = config.database.effective_primary_pool_size(),
                replica_configured = config.database.replica_url.is_some(),
                replica_max_connections = config.database.effective_replica_pool_size(),
                shard_count = shards.as_ref().map_or(0, crate::sharding::ShardSet::len),
                total_max_connections,
                "Database topology configured"
            );
            // Pool sizes multiply across shards; warn before the aggregate
            // silently exhausts Postgres's server-side `max_connections`.
            let warn_threshold = config.database.max_connections_warn_threshold;
            if crate::config::should_warn_total_connections(total_max_connections, warn_threshold) {
                tracing::warn!(
                    total_max_connections,
                    warn_threshold,
                    "Aggregate database connection count is high: the control \
                     topology and all shard pools together may open \
                     {total_max_connections} connections (warn threshold \
                     {warn_threshold}). Ensure each Postgres server's \
                     max_connections (plus headroom for migrations and \
                     psql) exceeds the pools that target it, or lower \
                     database.pool_size. Set \
                     database.max_connections_warn_threshold = 0 to silence."
                );
            }
        } else {
            tracing::info!("Database not configured");
        }

        // 5b. Fail-fast on `#[repository(api = ...)]` endpoints that
        // were mounted without a paired `policy = ...` argument when
        // running in `prod` profile and the explicit escape hatch is
        // off. Hides exactly the footgun called out in the issue:
        // "a developer who flips the `api =` switch on a
        // `#[repository]` exposes mutate endpoints that any
        // authenticated user can call against any record."
        validate_repository_api_policies(&all_routes, &scoped_groups, &config);

        // 6. Build the router (with optional static-file layer)
        let mut state = build_state(
            &config,
            #[cfg(feature = "db")]
            pool.as_ref(),
            #[cfg(feature = "db")]
            shards,
            #[cfg(feature = "ws")]
            channels_backend,
        );

        // Wire the in-memory log capture buffer from the telemetry guard into the
        // app state so the `/actuator/logfile` endpoint can serve it.
        if let Some(buf) = telemetry_guard.log_buffer.clone() {
            state.insert_extension(buf);
        }

        // Instantiate MaintenanceState, load flag synchronously at startup, insert as extension, and start background poller task
        let maintenance_state = crate::maintenance::MaintenanceState::new();
        let flag_path = std::path::Path::new(crate::maintenance::MAINTENANCE_FLAG_FILE);
        if let Ok(Some(cfg)) = crate::maintenance::MaintenanceState::load_from_file(flag_path) {
            maintenance_state.enable(cfg);
        }
        state.insert_extension(maintenance_state.clone());

        let poller_state = maintenance_state.clone();
        tokio::spawn(async move {
            let path = std::path::Path::new(crate::maintenance::MAINTENANCE_FLAG_FILE);
            let interval = std::time::Duration::from_millis(500);
            loop {
                let load_res = tokio::task::spawn_blocking(move || {
                    crate::maintenance::MaintenanceState::load_from_file(path)
                })
                .await;

                match load_res {
                    Ok(Ok(Some(cfg))) => {
                        if poller_state.get() != Some(cfg.clone()) {
                            poller_state.enable(cfg);
                        }
                    }
                    Ok(Ok(None)) => {
                        if poller_state.is_active() {
                            poller_state.disable();
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "failed to load maintenance flag file");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "maintenance poller task panicked");
                    }
                }
                tokio::time::sleep(interval).await;
            }
        });

        // Resolve the canary deploy-version label (AUTUMN_DEPLOY_VERSION /
        // AUTUMN_CANARY) once at startup and publish it so the actuator metrics
        // endpoint can tag every metric family with version="stable|canary".
        let canary_state = crate::canary::CanaryState::from_env();
        if canary_state.is_canary() {
            tracing::info!(
                version = canary_state.version(),
                "canary: replica labelled as canary cohort"
            );
        }
        state.insert_extension(canary_state);

        // A rollback flag present at startup means a controller already retired
        // this replica. Flip /ready to draining immediately so a supervisor
        // restart cannot put a rolled-back replica back into the canary cohort;
        // `canary_rollback_signal` then drives the clean drain → exit.
        if crate::canary::CanaryState::rollback_flag_present(std::path::Path::new(
            crate::canary::CANARY_ROLLBACK_FLAG_FILE,
        )) {
            tracing::warn!(
                "canary: rollback flag present at startup; /ready will report draining until \
                 the flag is cleared (`autumn canary promote`)"
            );
            state.begin_shutdown();
        }

        #[cfg(feature = "mail")]
        if let Some(interceptor) = mail_interceptor {
            state.insert_extension(interceptor);
        }
        if let Some(interceptor) = job_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "db")]
        if let Some(interceptor) = db_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "ws")]
        if let Some(interceptor) = channels_interceptor {
            state.insert_extension(interceptor.clone());
            state.channels = crate::channels::Channels::with_shared_backend(std::sync::Arc::new(
                crate::channels::InterceptedChannelsBackend::new(
                    state.channels.backend().clone(),
                    vec![interceptor],
                ),
            ));
            #[cfg(feature = "presence")]
            {
                state.presence = crate::presence::Presence::new(state.channels.clone());
            }
        }
        #[cfg(feature = "oauth2")]
        if let Some(interceptor) = http_interceptor {
            state.insert_extension(interceptor);
        }

        // Populate the metrics source registry from builder registrations.
        // Duplicate names were already rejected in `metrics_source()`, so
        // all entries here are unique.
        for (name, source) in metrics_sources {
            if let Err(e) = state.metrics_source_registry.register(name, source) {
                tracing::warn!("{e}");
            }
        }

        // Populate the health indicator registry from builder registrations.
        for (name, group, indicator) in health_indicators {
            if let Err(e) = state
                .health_indicator_registry
                .register(name, group, indicator)
            {
                tracing::warn!("{e}");
            }
        }

        #[cfg(feature = "db")]
        configure_replica_migration_check(&state, replica_migration_check);
        #[cfg(feature = "db")]
        apply_replica_migration_readiness(&state, replica_readiness);
        if let Some(cache) = cache_backend {
            crate::cache::set_global_cache(cache.clone());
            state.shared_cache = Some(cache);
        } else {
            crate::cache::clear_global_cache();
        }
        state.insert_extension(RegisteredApiVersions(api_versions));

        // Install registered error reporters so the reporting layer (wired in
        // `apply_middleware`) can deliver panic + 5xx events. Empty is fine —
        // the layer falls back to the built-in `LogReporter`.
        #[cfg(feature = "reporting")]
        if !error_reporters.is_empty() {
            state.insert_extension(crate::reporting::RegisteredReporters(error_reporters));
        }
        // Apply deferred policy / scope registrations onto the live
        // app state. Done before the router is built so any panic
        // from double-registration surfaces during startup, not
        // mid-request.
        for register in policy_registrations {
            register(state.policy_registry());
        }
        // Now that registrations have been applied, verify that
        // every `#[repository(policy = X)]`-annotated route has
        // an X actually registered on the live registry. Catches
        // the "wired the macro arg, forgot the `.policy(...)`
        // builder call" footgun before any 500 lands.
        validate_repository_policies_registered(&all_routes, &scoped_groups, &state, &config);
        #[cfg(feature = "mail")]
        if let Some(handle) = suppression_store {
            state.insert_extension(handle);
        }
        #[cfg(feature = "mail")]
        crate::mail::install_mailer_with_factory(
            &state,
            &config.mail,
            mail_delivery_queue_factory,
            true,
        )
        .unwrap_or_else(|error| {
            tracing::error!(error = %error, "Failed to configure mailer");
            exit_stop_managed_pg();
            std::process::exit(1);
        });
        #[cfg(feature = "mail")]
        state.insert_extension(crate::mail::MailPreviewRegistry::new(mail_previews));
        if let Some(logger) = audit_logger {
            state.insert_extension::<crate::audit::AuditLogger>((*logger).clone());
        }
        #[cfg(feature = "i18n")]
        let custom_layers = install_i18n_bundle_layer(custom_layers, &state, i18n_bundle);

        // Install the preflighted blob store on the freshly-built
        // AppState, and remember the serving router so it gets merged
        // into the user's router below.
        #[cfg(feature = "storage")]
        let storage_router = storage_bootstrap.and_then(|b| b.install(&state));
        install_webhook_registry(&state, &config);
        run_state_initializers(state_initializers, &state);
        finalize_event_bus(listeners, &mut jobs, &state);

        let env = crate::config::OsEnv;
        let dist_dir = project_dir("dist", &env);
        let dist_ref = if dist_dir.exists() {
            Some(dist_dir.as_path())
        } else {
            None
        };
        #[cfg_attr(
            not(any(feature = "storage", feature = "inbound-mail")),
            allow(unused_mut)
        )]
        let mut merge_routers = merge_routers;
        #[cfg(feature = "storage")]
        if let Some(router) = storage_router {
            merge_routers.push(router);
        }

        // Register SEO routes (/robots.txt and /sitemap.xml) when any SEO
        // configuration is present or dynamic sources are registered.
        if !seo_sources.is_empty() || crate::seo::has_seo_config(&config.seo) {
            let seo_cfg = &config.seo;
            let raw_profile = config.profile.as_deref().unwrap_or("dev");
            let profile = crate::seo::effective_seo_profile(raw_profile, seo_cfg.robots.allow_all);
            let static_paths: Vec<&str> = static_metas.iter().map(|m| m.path).collect();
            let (robots_body, sitemap_body) = crate::seo::assemble_seo_bodies(
                profile,
                seo_cfg.base_url.as_deref(),
                seo_cfg.robots.sitemap_url.as_deref(),
                &seo_cfg.robots.additional_rules,
                &seo_sources,
                &static_paths,
            )
            .await;
            let seo_router = crate::seo::build_seo_router_from_bodies(robots_body, sitemap_body);
            let is_seo_path = |p: &str| p == "/robots.txt" || p == "/sitemap.xml";
            let seo_collision = all_routes.iter().any(|r| is_seo_path(r.path))
                || static_metas.iter().any(|m| is_seo_path(m.path))
                || scoped_groups.iter().any(|g| {
                    let prefix = g.prefix.trim_end_matches('/');
                    g.routes
                        .iter()
                        .any(|r| is_seo_path(&format!("{prefix}{}", r.path)))
                });
            if seo_collision {
                tracing::warn!(
                    "seo: /robots.txt or /sitemap.xml is already registered by the application; \
                     skipping automatic SEO routes to prevent a startup panic"
                );
            } else {
                merge_routers.push(seo_router);
            }
        }

        #[cfg(feature = "inbound-mail")]
        if let Some(ref im_router) = inbound_mail_router {
            let mut registered_inbound: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for (path, axum_router) in crate::inbound_mail::build_routes(im_router) {
                // Preflight collision check: if an annotated POST route already
                // claims this path, merging an opaque router at the same path
                // would cause Axum to panic at startup.  Warn and skip instead
                // so the application can still start and the conflict is visible.
                if all_routes
                    .iter()
                    .any(|r| r.method == http::Method::POST && r.path == path)
                    || scoped_groups.iter().any(|g| {
                        g.routes.iter().any(|r| {
                            r.method == http::Method::POST
                                && crate::router::join_nested_path(&g.prefix, r.path)
                                    == path.as_str()
                        })
                    })
                    || nest_routers.iter().any(|(nest_path, _)| {
                        let p = nest_path.as_str();
                        path.as_str() == p
                            || path.starts_with(p)
                                && (p.ends_with('/') || path.as_bytes().get(p.len()) == Some(&b'/'))
                    })
                {
                    tracing::warn!(
                        path = %path,
                        "inbound_mail: skipping webhook route — a POST handler is \
                         already registered at this path by the application"
                    );
                    continue;
                }
                // Also guard against two inbound endpoints sharing the same path,
                // which would cause the same Axum merge panic.
                if !registered_inbound.insert(path.clone()) {
                    tracing::warn!(
                        path = %path,
                        "inbound_mail: skipping duplicate inbound webhook path"
                    );
                    continue;
                }
                // Exempt each inbound webhook path from both CSRF and CAPTCHA:
                // these routes receive provider-signed POST requests that never
                // carry a CSRF or CAPTCHA token.
                config.security.csrf.exempt_paths.push(path.clone());
                config.security.captcha_exempt_paths.push(path);
                merge_routers.push(axum_router);
            }
        }
        let router = crate::router::try_build_router_with_static_inner(
            all_routes,
            &config,
            state.clone(),
            dist_ref,
            crate::router::RouterContext {
                exception_filters,
                scoped_groups,
                merge_routers,
                nest_routers,
                custom_layers,
                static_gate_layers,
                #[cfg(feature = "maud")]
                error_page_renderer,
                session_store,
                // Respect the [openapi] profile gate: if disabled in config,
                // suppress the endpoint even when .openapi(...) was called.
                #[cfg(feature = "openapi")]
                openapi: if config.openapi_runtime.enabled {
                    openapi
                } else {
                    None
                },
                #[cfg(feature = "mcp")]
                mcp,
            },
        )
        .unwrap_or_else(|error| {
            tracing::error!(error = %error, "Failed to build router");
            exit_stop_managed_pg();
            std::process::exit(1);
        });

        // 7. Bind and initialize pre-serve runtime dependencies. Once those
        // are ready, start listening before startup hooks finish so `/startup`
        // can honestly report startup progress.
        // Bind the configured transport. A `server.unix_socket` path selects a
        // Unix domain socket (local daemon mode); otherwise bind TCP on
        // `host:port` as before. `bound_desc` is the human/log description and
        // `unix_socket_cleanup` is the socket to unlink on clean exit (axum does
        // not remove it for us), as `(path, dev, inode)` so cleanup can confirm
        // the file is still the one *this* process bound before removing it.
        let (bound_listener, bound_desc, unix_socket_cleanup): (
            BoundListener,
            String,
            Option<(std::path::PathBuf, u64, u64)>,
        ) = if let Some(socket_path) = config.server.unix_socket.as_deref() {
            let _ = socket_path;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let path = std::path::Path::new(socket_path);
                if let Err(e) = prepare_unix_socket_path(path) {
                    tracing::error!(socket = %socket_path, "Failed to prepare unix socket: {e}");
                    // `setup_database` already started the managed Postgres child;
                    // `process::exit` skips `on_shutdown`, so stop it first.
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
                // Bind under an owner-only umask so the socket is created `0600`
                // from the start — a plain bind would briefly leave it
                // group/other-connectable (umask-dependent), and `chmod` afterward
                // does not revoke a connection already established in that window.
                // This matters for a user-configured `server.unix_socket` in a
                // shared dir; the CLI's own socket also sits in a `0700` parent.
                // `umask` is process-wide, so serialize the save/bind/restore: a
                // concurrent UDS bind in the same process (integration tests, or an
                // app running several servers) could otherwise interleave these
                // pairs and either bind under the wrong umask — reopening the
                // bind→chmod window this closes — or leave `0177` set permanently.
                // The guard is released before the `.await` in the error arm below.
                let bind_result = {
                    static UMASK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
                    let _umask_guard = UMASK_LOCK
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let prev_umask =
                        nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o177));
                    let result = tokio::net::UnixListener::bind(path);
                    nix::sys::stat::umask(prev_umask);
                    result
                };
                let listener = match bind_result {
                    Ok(listener) => listener,
                    Err(e) => {
                        tracing::error!(socket = %socket_path, "Failed to bind unix socket: {e}");
                        #[cfg(feature = "managed-pg")]
                        crate::managed_pg::emergency_stop_async().await;
                        std::process::exit(1);
                    }
                };
                // Owner-only access, belt-and-suspenders after the umask bind.
                // Fail *closed* — if we cannot enforce `0600` (chmod error, an ACL
                // /filesystem that rejects it), refuse to serve rather than expose
                // a reachable control socket. Remove the socket we just bound so
                // nothing keeps listening on it.
                if let Err(e) =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                {
                    tracing::error!(socket = %socket_path, "Failed to enforce owner-only permissions on unix socket: {e}");
                    let _ = std::fs::remove_file(path);
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
                // Capture the bound socket's identity so a later successor that
                // rebinds the same path isn't unlinked by our shutdown.
                let (dev, ino) = {
                    use std::os::unix::fs::MetadataExt;
                    std::fs::metadata(path).map_or((0, 0), |m| (m.dev(), m.ino()))
                };
                (
                    BoundListener::Unix(listener),
                    format!("unix:{socket_path}"),
                    Some((path.to_path_buf(), dev, ino)),
                )
            }
            #[cfg(not(unix))]
            {
                tracing::error!(
                    "server.unix_socket is only supported on Unix platforms; \
                     unset it or use server.host/server.port"
                );
                std::process::exit(1);
            }
        } else {
            let addr = format!("{}:{}", config.server.host, config.server.port);
            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(listener) => listener,
                Err(e) => {
                    tracing::error!(addr = %addr, "Failed to bind: {e}");
                    // Stop the managed Postgres child started by `setup_database`
                    // before bailing; `process::exit` skips `on_shutdown`.
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
            };
            (BoundListener::Tcp(listener), addr, None)
        };

        let shutdown_timeout = config.server.shutdown_timeout_secs;
        let prestop_grace = config.server.prestop_grace_secs;
        let server_shutdown = tokio_util::sync::CancellationToken::new();

        if let Err(error) = initialize_job_runtime(jobs, &state, &server_shutdown, &config.jobs) {
            tracing::error!(error = %error, "job runtime initialization failed");
            // Post-DB failure: `process::exit` skips `on_shutdown`, so stop any
            // managed Postgres before bailing.
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }

        #[cfg(feature = "db")]
        {
            #[cfg(feature = "ws")]
            crate::repository_commit_hooks::set_global_channels(state.channels().clone());
        }

        #[cfg(feature = "db")]
        if let Some(pool) = state.pool().cloned() {
            #[cfg(feature = "ws")]
            {
                let channels = state.channels().clone();
                crate::repository_commit_hooks::start_repository_commit_hook_worker(
                    pool,
                    Some(channels),
                    server_shutdown.child_token(),
                );
            }
            #[cfg(not(feature = "ws"))]
            crate::repository_commit_hooks::start_repository_commit_hook_worker(
                pool,
                server_shutdown.child_token(),
            );
        }
        // Repositories built over a shard pool (`with_pool`) enqueue durable
        // commit hooks into that shard's queue table; drain each one too.
        #[cfg(feature = "db")]
        if let Some(shards) = state.shards() {
            for shard in shards.iter() {
                #[cfg(feature = "ws")]
                crate::repository_commit_hooks::start_repository_commit_hook_worker(
                    shard.primary_pool().clone(),
                    Some(state.channels().clone()),
                    server_shutdown.child_token(),
                );
                #[cfg(not(feature = "ws"))]
                crate::repository_commit_hooks::start_repository_commit_hook_worker(
                    shard.primary_pool().clone(),
                    server_shutdown.child_token(),
                );
            }
        }

        #[cfg(feature = "presence")]
        {
            let presence = state.presence().clone();
            let sweep_shutdown = server_shutdown.child_token();
            tokio::spawn(async move {
                let interval = std::time::Duration::from_secs(15);
                loop {
                    tokio::select! {
                        () = tokio::time::sleep(interval) => {
                            presence.sweep_expired();
                        }
                        () = sweep_shutdown.cancelled() => break,
                    }
                }
            });
        }

        tracing::info!(bound = %bound_desc, "Listening");

        let server_shutdown_wait = server_shutdown.clone();
        // Wrap the built router with the HTML form method-override layer at
        // the very edge — outside path and method routing — so a plain
        // browser `<form method="post">` carrying `_method=PUT|PATCH|DELETE`
        // can reach the declared PUT/PATCH/DELETE handler. `Router::layer`
        // applies middleware per registered method handler in axum 0.8,
        // which is too late: the inner `MethodRouter` returns `405` before
        // a layered service ever runs. Wrapping the whole router as a
        // tower::Service is the documented way to run middleware before
        // route matching.
        // TrustedProxiesLayer must be outermost (stamped before MethodOverrideLayer
        // reads ResolvedClientIdentity for its same-origin form check).
        let after_method = tower::Layer::layer(
            &crate::middleware::MethodOverrideLayer::new()
                .with_max_scan_bytes(config.security.upload.max_request_size_bytes),
            router,
        );
        let service = tower::Layer::layer(
            &crate::security::TrustedProxiesLayer::from_config(&config.security.trusted_proxies),
            after_method,
        );
        // Spawn the serve task per transport. The two arms differ only in the
        // connect-info type baked into the make-service (`SocketAddr` for TCP,
        // `UdsConnectInfo` for Unix sockets); the graceful-shutdown wiring and
        // the resulting `JoinHandle<io::Result<()>>` are identical. Handlers
        // extracting `ConnectInfo<SocketAddr>` are unsupported under a Unix
        // socket (acceptable: daemon mode is loopback-equivalent and local).
        let server_task = match bound_listener {
            BoundListener::Tcp(listener) => {
                let make_service =
                    axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<
                        std::net::SocketAddr,
                    >(service);
                tokio::spawn(async move {
                    axum::serve(listener, make_service)
                        .with_graceful_shutdown(async move {
                            server_shutdown_wait.cancelled().await;
                        })
                        .await
                })
            }
            #[cfg(unix)]
            BoundListener::Unix(listener) => {
                // UDS requests carry no TCP peer, so stamp a loopback identity
                // before `TrustedProxiesLayer` runs — local daemon requests then
                // resolve a `ClientAddr` (and IP-based maintenance/rate-limit
                // behavior works) exactly like a localhost TCP connection.
                let service = tower::Layer::layer(
                    &axum::middleware::from_fn(stamp_loopback_connect_info),
                    service,
                );
                let make_service =
                    axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<
                        UdsConnectInfo,
                    >(service);
                tokio::spawn(async move {
                    axum::serve(listener, make_service)
                        .with_graceful_shutdown(async move {
                            server_shutdown_wait.cancelled().await;
                        })
                        .await
                })
            }
        };

        let shutdown_state = state.clone();
        let shutdown_signal_token = server_shutdown.clone();
        #[cfg(feature = "ws")]
        let websocket_shutdown = state.shutdown.clone();
        // Clone metrics so the drain-watchdog can record aborted requests.
        let shutdown_metrics = state.metrics.clone();

        // Shared timestamp: set by shutdown_task when the listener is cancelled
        // (phase 5). Main reads it after server_task completes to measure only
        // actual drain time for hook budget — not the app's full uptime.
        let drain_started_at: std::sync::Arc<std::sync::OnceLock<std::time::Instant>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let drain_started_clone = std::sync::Arc::clone(&drain_started_at);

        // Notified by main just before server_task.await (after startup hooks
        // complete). If SIGTERM arrives during startup hooks the watchdog waits
        // here so the drain deadline is always measured from when drain starts.
        let drain_phase_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let drain_phase_notify_for_watchdog = std::sync::Arc::clone(&drain_phase_notify);
        // Boolean companion so the watchdog can skip the wait when SIGTERM arrives
        // after startup has already finished (the common case).
        let server_entered_drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_entered_drain_for_watchdog = std::sync::Arc::clone(&server_entered_drain);

        // Shutdown task: handles the rolling-deploy lifecycle phases.
        //
        // Phases:
        //   1. SIGTERM / Ctrl-C received
        //   2. /ready → 503  (probe flips before listener closes)
        //   3. prestop_grace elapses  (load-balancer deregistration window)
        //   4. WebSocket sessions receive close frame
        //   5. TCP listener stops accepting new connections; jobs/scheduler
        //      stop dequeuing (they share server_shutdown CancellationToken)
        //   6. In-flight requests drain within shutdown_timeout_secs; if the
        //      deadline is exceeded the watchdog exits with code 1 and
        //      records autumn_shutdown_aborted_requests_total.
        //
        // Phases 7-9 (on_shutdown hooks, telemetry flush, DB pool close) run
        // in main after server_task completes — within the remaining portion
        // of the same shutdown_timeout_secs budget, not an additional window.
        let shutdown_task = tokio::spawn(async move {
            // Phase 1: Wait for OS signal.
            shutdown_signal().await;
            tracing::info!(
                phase = "signal_received",
                prestop_grace_secs = prestop_grace,
                shutdown_timeout_secs = shutdown_timeout,
                "shutdown: graceful shutdown initiated"
            );

            // Phase 2: flip /ready → 503 strictly before the listener closes.
            shutdown_state.begin_shutdown();
            tracing::info!(phase = "ready_draining", "shutdown: /ready now 503");

            // Phase 3: prestop grace — wait for load balancers to deregister.
            if prestop_grace > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(prestop_grace)).await;
            }
            tracing::info!(phase = "listener_stopping", "shutdown: stopping listener");

            // Phase 4: send WebSocket close frames.
            #[cfg(feature = "ws")]
            websocket_shutdown.cancel();

            // Phase 5: stop listener and signal jobs/scheduler to stop dequeuing.
            // Record drain-start before cancelling so main gets the right hook
            // budget even in the startup-overlap case.
            let _ = drain_started_clone.set(std::time::Instant::now());
            shutdown_signal_token.cancel();

            // Phase 6: drain watchdog — if in-flight drain exceeds the budget,
            // record aborted count and force non-zero exit before hooks run.
            //
            // Always measure the deadline from when drain actually starts so that
            // in-flight requests always get the full shutdown_timeout_secs window:
            //
            //   Normal (SIGTERM after startup): server_entered_drain is already
            //   true, skip the wait, sleep the full budget.
            //
            //   Startup-overlap (SIGTERM during hooks): wait for notify, then
            //   sleep the full budget. Without this, hooks completing just before
            //   the watchdog fires would let it exit(1) immediately with no fresh
            //   drain window for requests that arrived after hooks completed.
            if !server_entered_drain_for_watchdog.load(std::sync::atomic::Ordering::Acquire) {
                tracing::warn!(
                    phase = "signal_during_startup",
                    "shutdown: SIGTERM during startup hooks; waiting for drain phase \
                     to begin before enforcing the drain deadline"
                );
                // Suspend until main fires notify_one() at drain start.
                // Orchestrator hard-kill backstop: if hooks never complete, the
                // orchestrator's kill_timeout / terminationGracePeriodSeconds kills us.
                drain_phase_notify_for_watchdog.notified().await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(shutdown_timeout)).await;
            // Guard against the boundary race where server_task completes at
            // exactly the deadline before main has called shutdown_task.abort().
            // Zero active requests means drain completed cleanly; return and let
            // main complete the cleanup path.
            if shutdown_metrics.snapshot().http.requests_active == 0 {
                return;
            }
            let aborted = shutdown_metrics.snapshot().http.requests_active;
            shutdown_metrics.record_shutdown_aborted(aborted);
            tracing::error!(
                phase = "in_flight_drain",
                timeout_secs = shutdown_timeout,
                autumn_shutdown_aborted_requests_total = aborted,
                exit_code = 1,
                "shutdown: in_flight_drain phase exceeded deadline; terminating"
            );
            // The watchdog's `process::exit` skips the remaining `on_shutdown`
            // hooks — including a managed-Postgres `stop()` — so a drain that
            // overruns its budget would orphan the postmaster. Stop it here too.
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        });

        if let Err(error) = run_startup_hooks(&startup_hooks, state.clone()).await {
            tracing::error!(error = %error, "startup hook failed");
            server_shutdown.cancel();
            server_task.abort();
            // `process::exit` skips `on_shutdown`; stop any managed Postgres.
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }

        if !state.probes().is_shutting_down() {
            if !tasks.is_empty() {
                let res = start_task_scheduler_with_config(
                    tasks,
                    &state,
                    &server_shutdown,
                    &config.scheduler,
                );
                if let Err(err) = res {
                    tracing::error!(error = %err, "scheduled task runtime initialization failed");
                    server_shutdown.cancel();
                    server_task.abort();
                    // `process::exit` skips `on_shutdown`; stop any managed Postgres.
                    #[cfg(feature = "managed-pg")]
                    crate::managed_pg::emergency_stop_async().await;
                    std::process::exit(1);
                }
            }
            state.probes().mark_startup_complete();
            signal_serve_ready(
                config
                    .server
                    .prestop_grace_secs
                    .saturating_add(config.server.shutdown_timeout_secs),
            );
        }

        // Signal the drain phase. The watchdog checks the flag for the common
        // case (SIGTERM arrives after startup) and waits on the notify for the
        // rare case (SIGTERM arrived during startup hooks). Both must be set so
        // the watchdog never re-enforces the deadline before drain actually starts.
        server_entered_drain.store(true, std::sync::atomic::Ordering::Release);
        drain_phase_notify.notify_one();

        // Wait for the server to drain all in-flight requests.  The drain
        // watchdog in shutdown_task will force-exit if drain takes too long.
        let server_result = server_task.await.unwrap_or_else(|e| {
            tracing::error!("Server task join error: {e}");
            // `process::exit` skips the `on_shutdown` hooks, so stop a managed
            // Postgres child here to avoid orphaning it on an accept-loop/join
            // failure (direct/foreground runs have no CLI reaper).
            exit_stop_managed_pg();
            std::process::exit(1);
        });
        // Drain completed within the deadline; abort the watchdog.
        shutdown_task.abort();
        server_result.unwrap_or_else(|e| {
            tracing::error!("Server error: {e}");
            exit_stop_managed_pg();
            std::process::exit(1);
        });

        // Phase 7: run on_shutdown hooks within the *remaining* portion of
        // shutdown_timeout_secs (drain + hooks share one budget, not two).
        // Plugin ordering: plugins register during build() before app hooks,
        // so app hooks run before plugin hooks (LIFO = last-registered first).
        let drain_elapsed = drain_started_at
            .get()
            .map_or(std::time::Duration::ZERO, std::time::Instant::elapsed);
        let hook_budget =
            std::time::Duration::from_secs(shutdown_timeout).saturating_sub(drain_elapsed);
        run_shutdown_hooks_with_timeout(&shutdown_hooks, hook_budget, hook_budget).await;
        // If request drain consumed the whole `shutdown_timeout_secs`, the
        // managed-Postgres `on_shutdown` hook may have been budgeted away above.
        // Stop the cluster directly here (idempotent — a no-op once the hook
        // already stopped it) so a direct/foreground run, which has no CLI
        // reaper, never leaves the postmaster holding the data dir/port.
        #[cfg(feature = "managed-pg")]
        crate::managed_pg::emergency_stop_async().await;

        // Remove the Unix socket file on clean exit; axum does not unlink it.
        // (An abnormal force-exit may leave it behind, but the next bind's
        // `prepare_unix_socket_path` reclaims a stale socket.) Only unlink if the
        // socket is still the one we bound — a successor that rebound the same
        // path after we closed has a different inode, and removing it would make
        // the new server unreachable.
        #[cfg(unix)]
        if let Some((path, dev, ino)) = &unix_socket_cleanup {
            use std::os::unix::fs::MetadataExt;
            let still_ours =
                std::fs::metadata(path).is_ok_and(|m| m.dev() == *dev && m.ino() == *ino);
            if still_ours {
                let _ = std::fs::remove_file(path);
            }
        }
        #[cfg(not(unix))]
        let _ = &unix_socket_cleanup;

        tracing::info!(exit_code = 0, "shutdown: all phases completed cleanly");
    }

    /// Render all registered static routes to `dist/` and exit.
    ///
    /// Triggered when `AUTUMN_BUILD_STATIC=1` is set (by `autumn build`).
    /// Builds the Axum router, renders each static route through it, and
    /// writes HTML + manifest to the `dist/` directory.
    #[allow(clippy::too_many_lines)]
    async fn run_build_mode(self) {
        let Self {
            routes,
            api_versions,
            route_sources: _,
            current_plugin: _,
            tasks: _,
            one_off_tasks: _,
            jobs: _,
            listeners,
            static_metas,
            exception_filters: _,
            scoped_groups,
            merge_routers: _,
            nest_routers: _,
            custom_layers,
            static_gate_layers: _,
            startup_hooks: _,
            state_initializers,
            shutdown_hooks: _,
            extensions: _,
            registered_plugins: _,
            #[cfg(feature = "maud")]
                error_page_renderer: _,
            #[cfg(feature = "db")]
                migrations: _,
            config_loader_factory,
            #[cfg(feature = "db")]
            pool_provider_factory,
            #[cfg(feature = "db")]
            shard_provider_factory,
            #[cfg(feature = "db")]
            shard_router,
            #[cfg(feature = "db")]
            directory_shard_router,
            telemetry_provider,
            session_store,
            #[cfg(feature = "ws")]
            channels_backend,
            #[cfg(feature = "storage")]
            blob_store,
            cache_backend,
            #[cfg(feature = "reporting")]
            error_reporters,
            #[cfg(feature = "openapi")]
            openapi,
            #[cfg(feature = "mcp")]
                mcp: _,
            audit_logger: _,
            #[cfg(feature = "i18n")]
            i18n_bundle,
            #[cfg(feature = "i18n")]
            i18n_auto_load,
            #[cfg(feature = "embed-assets")]
            embedded_static,
            #[cfg(all(feature = "embed-assets", feature = "i18n"))]
            embedded_locales,
            policy_registrations,
            #[cfg(feature = "mail")]
            mail_delivery_queue_factory,
            #[cfg(feature = "mail")]
            suppression_store,
            #[cfg(feature = "mail")]
            mount_unsubscribe_endpoint,
            #[cfg(feature = "mail")]
            mail_previews,
            declared_routes: _,
            idempotency_enabled,
            #[cfg(feature = "mail")]
            mail_interceptor,
            job_interceptor,
            #[cfg(feature = "db")]
            db_interceptor,
            #[cfg(feature = "ws")]
            channels_interceptor,
            #[cfg(feature = "oauth2")]
            http_interceptor,
            seo_sources,
            metrics_sources,
            health_indicators,
            #[cfg(feature = "inbound-mail")]
                inbound_mail_router: _,
        } = self;

        let _ = &api_versions;
        let _ = &metrics_sources;
        let _ = &health_indicators;
        let all_routes = routes;

        // Load config (same as normal startup)
        let (mut config, telemetry_guard) =
            load_config_and_telemetry(config_loader_factory, telemetry_provider).await;

        #[cfg(feature = "mail")]
        if mount_unsubscribe_endpoint {
            config.mail.mount_unsubscribe_endpoint = true;
        }
        if idempotency_enabled {
            let env_disabled = std::env::var("AUTUMN_IDEMPOTENCY__ENABLED")
                .is_ok_and(|v| matches!(v.to_lowercase().as_str(), "false" | "0" | "no" | "off"));
            // Only apply the builder default when neither the env var nor the
            // loaded config file explicitly sets enabled = false.
            if !env_disabled && config.idempotency.enabled != Some(false) {
                config.idempotency.enabled = Some(true);
            }
        }

        // Register the embedded `static/` tree (if any) before the router is
        // built so `/static/*` serves from the binary and `asset_url()` resolves
        // against the embedded manifest, then prefer embedded locales over disk
        // auto-loading when no explicit bundle was provided.
        #[cfg(feature = "embed-assets")]
        register_embedded_static_dir(embedded_static);

        #[cfg(all(feature = "embed-assets", feature = "i18n"))]
        let i18n_bundle = embedded_i18n_bundle(i18n_bundle, embedded_locales, &config);

        #[cfg(feature = "i18n")]
        let i18n_bundle =
            resolve_i18n_bundle(i18n_bundle, i18n_auto_load, &config, &crate::config::OsEnv);

        // Snapshot ApiDocs before all_routes is moved into the router builder.
        // Includes top-level routes and scoped groups (with prefixed paths) so
        // the emitted dist/openapi.json matches what the runtime spec serves.
        #[cfg(feature = "openapi")]
        let api_docs_snapshot: Vec<crate::openapi::ApiDoc> = {
            let mut docs: Vec<crate::openapi::ApiDoc> = all_routes
                .iter()
                .map(|r| {
                    let mut doc = r.api_doc.clone();
                    doc.api_version = r.api_version;
                    doc.sunset_opt_out = r.sunset_opt_out;
                    doc
                })
                .collect();
            for group in &scoped_groups {
                // Mirror the same normalization as the runtime OpenAPI builder:
                // use join_nested_path for correct trailing-slash handling, and
                // merge prefix path params so they appear in the operation.
                let prefix_params = crate::router::extract_path_params(&group.prefix);
                for route in &group.routes {
                    let mut doc = route.api_doc.clone();
                    doc.api_version = route.api_version;
                    doc.sunset_opt_out = route.sunset_opt_out;
                    let full = crate::router::join_nested_path(&group.prefix, route.api_doc.path);
                    doc.path = Box::leak(full.into_boxed_str());
                    if !prefix_params.is_empty() {
                        let mut merged: Vec<&'static str> = prefix_params
                            .iter()
                            .map(|p| &*Box::leak(p.clone().into_boxed_str()))
                            .collect();
                        merged.extend_from_slice(doc.path_params);
                        doc.path_params = Box::leak(merged.into_boxed_slice());
                    }
                    docs.push(doc);
                }
            }
            docs
        };

        if static_metas.is_empty() {
            eprintln!("No static routes registered. Nothing to build.");
            eprintln!("Hint: use .static_routes(static_routes![...]) on your AppBuilder.");
            std::process::exit(1);
        }

        // Fail-fast on invalid session config — only when no custom store
        // was installed. Symmetrical to the same check in run() so static
        // builds don't run migrations against a doomed boot either.
        fail_fast_on_invalid_session_config(&config, session_store.is_some());
        fail_fast_on_invalid_signing_secret(&config);
        fail_fast_on_missing_encryption_keys(&config);
        fail_fast_on_invalid_trusted_hosts(&config);

        // Preflight the configured BlobStore the same way `run()` does.
        // Static routes can read presigned URLs out of `BlobStoreState`
        // during pre-rendering (e.g. `<img src=blob.url()>`); without
        // the bootstrap they'd 500 during `autumn build` even though
        // the server path works. A custom store from `.with_blob_store()`
        // bypasses config-driven instantiation.
        #[cfg(feature = "storage")]
        let storage_bootstrap = blob_store.map_or_else(
            || preflight_storage(&config),
            |store| {
                Some(StorageBootstrap {
                    store,
                    serving: None,
                })
            },
        );

        // Build state (with DB if configured)
        #[cfg(feature = "db")]
        let database = setup_database(
            &config,
            vec![],
            pool_provider_factory,
            shard_provider_factory,
            shard_router,
            directory_shard_router,
            RepositoryCommitHookQueueMigrationMode::StaticBuild,
        )
        .await
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });
        #[cfg(feature = "db")]
        let pool = database.topology;
        #[cfg(feature = "db")]
        let shards = database.shards;
        #[cfg(feature = "db")]
        let replica_readiness = database.replica_readiness;
        #[cfg(feature = "db")]
        let replica_migration_check = database.replica_migration_check;

        let mut state = build_state(
            &config,
            #[cfg(feature = "db")]
            pool.as_ref(),
            #[cfg(feature = "db")]
            shards,
            #[cfg(feature = "ws")]
            channels_backend,
        );
        if let Some(buf) = telemetry_guard.log_buffer.clone() {
            state.insert_extension(buf);
        }
        state.insert_extension(RegisteredApiVersions(api_versions.clone()));
        #[cfg(feature = "mail")]
        if let Some(interceptor) = mail_interceptor {
            state.insert_extension(interceptor);
        }
        if let Some(interceptor) = job_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "db")]
        if let Some(interceptor) = db_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "ws")]
        if let Some(interceptor) = channels_interceptor {
            state.insert_extension(interceptor.clone());
            state.channels = crate::channels::Channels::with_shared_backend(std::sync::Arc::new(
                crate::channels::InterceptedChannelsBackend::new(
                    state.channels.backend().clone(),
                    vec![interceptor],
                ),
            ));
            #[cfg(feature = "presence")]
            {
                state.presence = crate::presence::Presence::new(state.channels.clone());
            }
        }
        #[cfg(feature = "oauth2")]
        if let Some(interceptor) = http_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "db")]
        configure_replica_migration_check(&state, replica_migration_check);
        #[cfg(feature = "db")]
        apply_replica_migration_readiness(&state, replica_readiness);
        if let Some(cache) = cache_backend {
            crate::cache::set_global_cache(cache.clone());
            state.shared_cache = Some(cache);
        } else {
            crate::cache::clear_global_cache();
        }
        #[cfg(feature = "reporting")]
        if !error_reporters.is_empty() {
            state.insert_extension(crate::reporting::RegisteredReporters(error_reporters));
        }
        // Static-site builds are short-lived and don't run the request loop,
        // so deliver_later is never invoked. install_mailer_with_factory skips
        // the queue factory when enforce_durable_guard is false (the factory
        // may open Redis/Harvest connections unavailable here), and the guard
        // itself is bypassed too — the Mailer is still installed so static
        // routes that extract `Mailer` for immediate `send` calls resolve.
        #[cfg(feature = "mail")]
        if let Some(handle) = suppression_store {
            state.insert_extension(handle);
        }
        #[cfg(feature = "mail")]
        crate::mail::install_mailer_with_factory(
            &state,
            &config.mail,
            mail_delivery_queue_factory,
            false,
        )
        .unwrap_or_else(|error| {
            eprintln!("Failed to configure mailer: {error}");
            exit_stop_managed_pg();
            std::process::exit(1);
        });
        #[cfg(feature = "mail")]
        state.insert_extension(crate::mail::MailPreviewRegistry::new(mail_previews));
        // run_build_mode used ProbeState::default(), which does not start as pending
        state.probes = crate::probe::ProbeState::default();

        // Apply deferred policy / scope registrations onto the live
        // app state — same as `run()`. Static routes can carry
        // `#[authorize]` checks or live behind `#[repository(policy =
        // ..., scope = ...)]` index endpoints; without registering
        // here, every such pre-render call would 500 at build time
        // with `no policy/scope registered`, and `render_static_routes`
        // would treat that as a build failure even though
        // `.policy(...)` / `.scope(...)` was configured on the
        // builder.
        for register in policy_registrations {
            register(state.policy_registry());
        }

        #[cfg(feature = "i18n")]
        let custom_layers = install_i18n_bundle_layer(custom_layers, &state, i18n_bundle);

        // Install the preflighted storage and remember the serving
        // router so static generation hits the same `/_blobs/...`
        // routes the server path serves.
        #[cfg(feature = "storage")]
        let storage_router = storage_bootstrap.and_then(|b| b.install(&state));
        install_webhook_registry(&state, &config);
        run_state_initializers(state_initializers, &state);
        // Static generation has no job runtime, so register only sync listeners.
        // Durable listeners are dropped entirely (not just their jobs) so a
        // static route publishing such an event is a clean no-op for the durable
        // side effect rather than a "job runtime not initialized" error.
        let sync_listeners: Vec<_> = listeners
            .into_iter()
            .filter(|listener| listener.mode == crate::events::DispatchMode::Sync)
            .collect();
        finalize_event_bus(sync_listeners, &mut Vec::new(), &state);

        // Build the full router (same as production). Use the inner builder
        // so the custom session store installed via with_session_store(...)
        // is honored during static generation — apps that swap in a custom
        // store specifically to avoid Redis/external backends at build time
        // would otherwise silently fall back to the config-driven backend.
        // Custom Tower layers registered via .layer(...) are likewise
        // applied so static output matches the production response pipeline.
        #[cfg_attr(not(feature = "storage"), allow(unused_mut))]
        let mut merge_routers: Vec<axum::Router<AppState>> = Vec::new();
        #[cfg(feature = "storage")]
        if let Some(router) = storage_router {
            merge_routers.push(router);
        }
        let router = crate::router::try_build_router_inner(
            all_routes,
            &config,
            state,
            crate::router::RouterContext {
                exception_filters: Vec::new(),
                scoped_groups,
                merge_routers,
                nest_routers: Vec::new(),
                custom_layers,
                static_gate_layers: Vec::new(),
                #[cfg(feature = "maud")]
                error_page_renderer: None,
                session_store,
                #[cfg(feature = "openapi")]
                openapi: None,
                #[cfg(feature = "mcp")]
                mcp: None,
            },
        )
        .unwrap_or_else(|error| {
            eprintln!("Failed to build router: {error}");
            exit_stop_managed_pg();
            std::process::exit(1);
        });

        let env = crate::config::OsEnv;
        let dist_dir = project_dir("dist", &env);

        eprintln!("Building {} static route(s)...", static_metas.len());

        match crate::static_gen::render_static_routes(router, &static_metas, &dist_dir).await {
            Ok(()) => {
                eprintln!(
                    "\n  \u{2713} Static build complete \u{2192} {}",
                    dist_dir.display()
                );
            }
            Err(e) => {
                eprintln!("\n  \u{2717} Static build failed: {e}");
                exit_stop_managed_pg();
                std::process::exit(1);
            }
        }

        // When OpenAPI is configured, write the spec to dist/ so consumers
        // can retrieve a machine-readable API contract alongside the HTML.
        #[cfg(feature = "openapi")]
        if let Some(mut openapi_config) = openapi {
            openapi_config.api_versions = api_versions;
            let openapi_config =
                openapi_config.session_cookie_name(config.session.cookie_name.clone());
            let docs: Vec<&crate::openapi::ApiDoc> = api_docs_snapshot.iter().collect();
            let spec = crate::openapi::generate_spec(&openapi_config, &docs);
            match crate::openapi::write_openapi_spec_to_dist(&spec, &dist_dir) {
                Ok(()) => {
                    eprintln!(
                        "  \u{2713} OpenAPI spec written \u{2192} {}/openapi.json",
                        dist_dir.display()
                    );
                }
                Err(e) => {
                    eprintln!("  \u{26A0} Failed to write OpenAPI spec: {e}");
                }
            }
        }

        // Write robots.txt and sitemap.xml to dist/ — only when SEO is explicitly
        // configured or dynamic sources are registered, and never overwrite files
        // already produced by a custom #[static_get("/robots.txt")] route.
        if !seo_sources.is_empty() || crate::seo::has_seo_config(&config.seo) {
            let seo_cfg = &config.seo;
            let raw_profile = config.profile.as_deref().unwrap_or("dev");
            let profile = crate::seo::effective_seo_profile(raw_profile, seo_cfg.robots.allow_all);
            let static_paths: Vec<&str> = static_metas.iter().map(|m| m.path).collect();
            let (robots_body, sitemap_body) = crate::seo::assemble_seo_bodies(
                profile,
                seo_cfg.base_url.as_deref(),
                seo_cfg.robots.sitemap_url.as_deref(),
                &seo_cfg.robots.additional_rules,
                &seo_sources,
                &static_paths,
            )
            .await;
            // Write each file only if it wasn't already produced by a
            // custom #[static_get] route.
            let robots_path = dist_dir.join("robots.txt");
            let sitemap_path = dist_dir.join("sitemap.xml");
            if robots_path.exists() {
                eprintln!(
                    "  \u{2713} SEO: robots.txt already present (custom static route), skipping"
                );
            } else {
                match tokio::fs::write(&robots_path, robots_body).await {
                    Ok(()) => eprintln!(
                        "  \u{2713} SEO: robots.txt written \u{2192} {}",
                        robots_path.display()
                    ),
                    Err(e) => eprintln!("  \u{26A0} Failed to write robots.txt: {e}"),
                }
            }
            if sitemap_path.exists() {
                eprintln!(
                    "  \u{2713} SEO: sitemap.xml already present (custom static route), skipping"
                );
            } else {
                match tokio::fs::write(&sitemap_path, sitemap_body).await {
                    Ok(()) => eprintln!(
                        "  \u{2713} SEO: sitemap.xml written \u{2192} {}",
                        sitemap_path.display()
                    ),
                    Err(e) => eprintln!("  \u{26A0} Failed to write sitemap.xml: {e}"),
                }
            }
        }

        // Build finished: stop the managed Postgres child `setup_database` may
        // have started. Build mode discards the app's `on_shutdown` hooks, so
        // without this even a *successful* `autumn build` would leak the cluster.
        #[cfg(feature = "managed-pg")]
        crate::managed_pg::emergency_stop_async().await;
    }

    /// Dump the application's route listing as JSON and exit.
    ///
    /// Triggered when `AUTUMN_DUMP_ROUTES=1` is set (by `autumn routes`).
    /// Exits with code 0 on success, code 1 on JSON serialization failure.
    /// Does not connect to a database or bind a TCP port.
    async fn run_dump_routes_mode(self) {
        let Self {
            routes,
            api_versions,
            route_sources,
            scoped_groups,
            merge_routers,
            nest_routers,
            declared_routes,
            config_loader_factory,
            telemetry_provider,
            #[cfg(feature = "openapi")]
            openapi,
            ..
        } = self;

        // Validate that all versioned routes use a registered API version
        let registered_versions: std::collections::HashSet<&str> =
            api_versions.iter().map(|av| av.version.as_str()).collect();

        for route in &routes {
            if let Some(ver) = route
                .api_version
                .filter(|ver| !registered_versions.contains(*ver))
            {
                eprintln!(
                    "Failed to build router: route '{}' uses unregistered API version '{}'",
                    route.name, ver
                );
                std::process::exit(1);
            }
        }

        for group in &scoped_groups {
            for route in &group.routes {
                if let Some(ver) = route
                    .api_version
                    .filter(|ver| !registered_versions.contains(*ver))
                {
                    eprintln!(
                        "Failed to build router: route '{}' uses unregistered API version '{}'",
                        route.name, ver
                    );
                    std::process::exit(1);
                }
            }
        }

        // Raw Axum routers registered via .merge()/.nest() are opaque: there is
        // no public API to enumerate their routes. Always warn so callers know
        // some routes may be missing even if declare_plugin_routes was used.
        let hidden = merge_routers.len() + nest_routers.len();
        if hidden > 0 {
            eprintln!(
                "[autumn routes] warning: {hidden} raw router(s) added via \
                 .merge()/.nest() are not enumerable and are omitted from this listing"
            );
        }

        let (config, _telemetry_guard) =
            load_config_and_telemetry(config_loader_factory, telemetry_provider).await;

        let mut infos = match crate::route_listing::collect_route_infos(
            &routes,
            &route_sources,
            &scoped_groups,
            &api_versions,
        ) {
            Ok(infos) => infos,
            Err(e) => {
                eprintln!("Failed to build router: {e}");
                std::process::exit(1);
            }
        };
        infos.extend(declared_routes);
        crate::route_listing::append_framework_routes(&mut infos, &config);
        #[cfg(feature = "openapi")]
        if let Some(ref oa) = openapi {
            crate::route_listing::append_openapi_routes(&mut infos, oa);
        }
        crate::route_listing::append_dev_reload_routes(&mut infos);
        crate::route_listing::sort_route_infos(&mut infos);

        let json = serde_json::to_string_pretty(&infos).unwrap_or_else(|e| {
            eprintln!("Failed to serialize route listing: {e}");
            std::process::exit(1);
        });
        println!("{json}");
        std::process::exit(0);
    }

    /// Dump registered one-off tasks as JSON and exit.
    ///
    /// Triggered by `AUTUMN_LIST_TASKS=1` from `autumn task --list`.
    fn run_list_one_off_tasks_mode(self) {
        let Self { one_off_tasks, .. } = self;

        if let Err(error) = crate::task::validate_unique_one_off_task_names(&one_off_tasks) {
            eprintln!("Invalid task registration: {error}");
            std::process::exit(1);
        }

        let listing = crate::task::list_one_off_tasks(&one_off_tasks);
        let json = serde_json::to_string_pretty(&listing).unwrap_or_else(|error| {
            eprintln!("Failed to serialize task listing: {error}");
            std::process::exit(1);
        });
        println!("{json}");
        std::process::exit(0);
    }

    /// Run a registered one-off task with full application context and exit.
    ///
    /// Triggered by `AUTUMN_RUN_TASK=<name>` from `autumn task <name>`.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::cognitive_complexity)]
    async fn run_one_off_task_mode(self, requested_name: String) {
        let Self {
            one_off_tasks,
            mut jobs,
            listeners,
            #[cfg(feature = "i18n")]
            custom_layers,
            #[cfg(not(feature = "i18n"))]
                custom_layers: _,
            startup_hooks,
            state_initializers,
            shutdown_hooks,
            config_loader_factory,
            #[cfg(feature = "db")]
            migrations,
            #[cfg(feature = "db")]
            pool_provider_factory,
            #[cfg(feature = "db")]
            shard_provider_factory,
            #[cfg(feature = "db")]
            shard_router,
            #[cfg(feature = "db")]
            directory_shard_router,
            telemetry_provider,
            session_store,
            #[cfg(feature = "ws")]
            channels_backend,
            #[cfg(feature = "storage")]
            blob_store,
            audit_logger,
            #[cfg(feature = "i18n")]
            i18n_bundle,
            #[cfg(feature = "i18n")]
            i18n_auto_load,
            #[cfg(feature = "embed-assets")]
            embedded_static,
            #[cfg(all(feature = "embed-assets", feature = "i18n"))]
            embedded_locales,
            policy_registrations,
            cache_backend,
            #[cfg(feature = "mail")]
            mail_delivery_queue_factory,
            #[cfg(feature = "mail")]
            suppression_store,
            #[cfg(feature = "mail")]
                mount_unsubscribe_endpoint: _,
            #[cfg(feature = "mail")]
            mail_interceptor,
            job_interceptor,
            #[cfg(feature = "db")]
            db_interceptor,
            #[cfg(feature = "ws")]
            channels_interceptor,
            #[cfg(feature = "oauth2")]
            http_interceptor,
            ..
        } = self;

        if let Err(error) = crate::task::validate_unique_one_off_task_names(&one_off_tasks) {
            eprintln!("Invalid task registration: {error}");
            std::process::exit(1);
        }

        let Some((task_name, task_handler)) = one_off_tasks
            .iter()
            .find(|task| task.name == requested_name)
            .map(|task| (task.name.clone(), task.handler))
        else {
            eprintln!("No one-off task named '{requested_name}' is registered.");
            print_available_one_off_tasks(&one_off_tasks);
            std::process::exit(1);
        };

        let args = one_off_task_args_from_env().unwrap_or_else(|error| {
            eprintln!("Invalid task args: {error}");
            std::process::exit(1);
        });

        let (config, telemetry_guard) =
            load_config_and_telemetry(config_loader_factory, telemetry_provider).await;

        // Register the embedded `static/` tree (if any) before the router is
        // built so `/static/*` serves from the binary and `asset_url()` resolves
        // against the embedded manifest, then prefer embedded locales over disk
        // auto-loading when no explicit bundle was provided.
        #[cfg(feature = "embed-assets")]
        register_embedded_static_dir(embedded_static);

        #[cfg(all(feature = "embed-assets", feature = "i18n"))]
        let i18n_bundle = embedded_i18n_bundle(i18n_bundle, embedded_locales, &config);

        #[cfg(feature = "i18n")]
        let i18n_bundle =
            resolve_i18n_bundle(i18n_bundle, i18n_auto_load, &config, &crate::config::OsEnv);

        fail_fast_on_invalid_session_config(&config, session_store.is_some());
        fail_fast_on_invalid_signing_secret(&config);
        fail_fast_on_missing_encryption_keys(&config);
        fail_fast_on_invalid_trusted_hosts(&config);

        #[cfg(feature = "storage")]
        let storage_bootstrap = blob_store.map_or_else(
            || preflight_storage(&config),
            |store| {
                Some(StorageBootstrap {
                    store,
                    serving: None,
                })
            },
        );

        #[cfg(feature = "db")]
        let database = setup_database(
            &config,
            migrations,
            pool_provider_factory,
            shard_provider_factory,
            shard_router,
            directory_shard_router,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        )
        .await
        .unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(1);
        });
        #[cfg(feature = "db")]
        let pool = database.topology;
        #[cfg(feature = "db")]
        let shards = database.shards;
        #[cfg(feature = "db")]
        let replica_readiness = database.replica_readiness;
        #[cfg(feature = "db")]
        let replica_migration_check = database.replica_migration_check;

        let mut state = build_state(
            &config,
            #[cfg(feature = "db")]
            pool.as_ref(),
            #[cfg(feature = "db")]
            shards,
            #[cfg(feature = "ws")]
            channels_backend,
        );
        if let Some(buf) = telemetry_guard.log_buffer.clone() {
            state.insert_extension(buf);
        }
        #[cfg(feature = "mail")]
        if let Some(interceptor) = mail_interceptor {
            state.insert_extension(interceptor);
        }
        if let Some(interceptor) = job_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "db")]
        if let Some(interceptor) = db_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "ws")]
        if let Some(interceptor) = channels_interceptor {
            state.insert_extension(interceptor.clone());
            state.channels = crate::channels::Channels::with_shared_backend(std::sync::Arc::new(
                crate::channels::InterceptedChannelsBackend::new(
                    state.channels.backend().clone(),
                    vec![interceptor],
                ),
            ));
            #[cfg(feature = "presence")]
            {
                state.presence = crate::presence::Presence::new(state.channels.clone());
            }
        }
        #[cfg(feature = "oauth2")]
        if let Some(interceptor) = http_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "db")]
        configure_replica_migration_check(&state, replica_migration_check);
        #[cfg(feature = "db")]
        apply_replica_migration_readiness(&state, replica_readiness);
        if let Some(cache) = cache_backend {
            crate::cache::set_global_cache(cache.clone());
            state.shared_cache = Some(cache);
        } else {
            crate::cache::clear_global_cache();
        }

        for register in policy_registrations {
            register(state.policy_registry());
        }

        #[cfg(feature = "mail")]
        if let Some(handle) = suppression_store {
            state.insert_extension(handle);
        }
        #[cfg(feature = "mail")]
        crate::mail::install_mailer_with_factory(
            &state,
            &config.mail,
            mail_delivery_queue_factory,
            true,
        )
        .unwrap_or_else(|error| {
            eprintln!("Failed to configure mailer: {error}");
            exit_stop_managed_pg();
            std::process::exit(1);
        });

        if let Some(logger) = audit_logger {
            state.insert_extension::<crate::audit::AuditLogger>((*logger).clone());
        }

        #[cfg(feature = "i18n")]
        let _custom_layers = install_i18n_bundle_layer(custom_layers, &state, i18n_bundle);

        #[cfg(feature = "storage")]
        let _storage_router = storage_bootstrap.and_then(|bootstrap| bootstrap.install(&state));
        run_state_initializers(state_initializers, &state);
        finalize_event_bus(listeners, &mut jobs, &state);

        let task_shutdown = tokio_util::sync::CancellationToken::new();
        if let Err(error) = initialize_job_runtime(jobs, &state, &task_shutdown, &config.jobs) {
            eprintln!("job runtime initialization failed: {error}");
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }

        #[cfg(feature = "db")]
        {
            #[cfg(feature = "ws")]
            crate::repository_commit_hooks::set_global_channels(state.channels().clone());
        }

        #[cfg(feature = "db")]
        if let Some(pool) = state.pool().cloned() {
            #[cfg(feature = "ws")]
            {
                let channels = state.channels().clone();
                crate::repository_commit_hooks::start_repository_commit_hook_worker(
                    pool,
                    Some(channels),
                    task_shutdown.child_token(),
                );
            }
            #[cfg(not(feature = "ws"))]
            crate::repository_commit_hooks::start_repository_commit_hook_worker(
                pool,
                task_shutdown.child_token(),
            );
        }
        // Repositories built over a shard pool (`with_pool`) enqueue durable
        // commit hooks into that shard's queue table; drain each one too.
        #[cfg(feature = "db")]
        if let Some(shards) = state.shards() {
            for shard in shards.iter() {
                #[cfg(feature = "ws")]
                crate::repository_commit_hooks::start_repository_commit_hook_worker(
                    shard.primary_pool().clone(),
                    Some(state.channels().clone()),
                    task_shutdown.child_token(),
                );
                #[cfg(not(feature = "ws"))]
                crate::repository_commit_hooks::start_repository_commit_hook_worker(
                    shard.primary_pool().clone(),
                    task_shutdown.child_token(),
                );
            }
        }

        if let Err(error) = run_startup_hooks(&startup_hooks, state.clone()).await {
            eprintln!("startup hook failed: {error}");
            task_shutdown.cancel();
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            std::process::exit(1);
        }
        state.probes().mark_startup_complete();

        tracing::info!(task = %task_name, "Running one-off task");
        let span = tracing::info_span!("one_off_task", task = %task_name);
        #[cfg(feature = "oauth2")]
        let result = {
            use crate::interceptor::{ACTIVE_HTTP_INTERCEPTORS, HttpInterceptor};
            let interceptors: Vec<std::sync::Arc<dyn HttpInterceptor>> = state
                .extension::<std::sync::Arc<dyn HttpInterceptor>>()
                .map(|interceptor_arc| vec![(*interceptor_arc).clone()])
                .unwrap_or_default();
            ACTIVE_HTTP_INTERCEPTORS
                .scope(
                    interceptors,
                    (task_handler)(state.clone(), args).instrument(span),
                )
                .await
        };
        #[cfg(not(feature = "oauth2"))]
        let result = (task_handler)(state.clone(), args).instrument(span).await;

        task_shutdown.cancel();
        run_shutdown_hooks(&shutdown_hooks).await;
        // If the generated `pg.stop()` hook errored/timed out it keeps the
        // handle for a retry, but a one-off task then exits — so retry the stop
        // here (idempotent; a no-op once the hook stopped it cleanly) to avoid
        // orphaning the postmaster on the data dir/port.
        #[cfg(feature = "managed-pg")]
        crate::managed_pg::emergency_stop_async().await;

        match result {
            Ok(()) => {
                tracing::info!(task = %task_name, "One-off task completed");
            }
            Err(error) => {
                tracing::error!(task = %task_name, error = %error, "One-off task failed");
                eprintln!("Task '{task_name}' failed: {error}");
                for cause in error.source_chain() {
                    eprintln!("Caused by: {cause}");
                }
                std::process::exit(1);
            }
        }
    }
}

pub(crate) fn is_static_build_mode() -> bool {
    std::env::var("AUTUMN_BUILD_STATIC").as_deref() == Ok("1")
}

/// Stop a managed Postgres child from a synchronous `process::exit` path in a
/// non-server entrypoint (static build, one-off task). Those modes don't run
/// `on_shutdown` before their failure exits, and `process::exit` skips `Drop`,
/// so a managed cluster started by `setup_database` would otherwise be orphaned
/// on the data dir/port.
///
/// These call sites run on a Tokio worker thread; the (blocking, own-runtime)
/// `emergency_stop` would panic if entered there, so run it on a fresh thread
/// with no ambient runtime. No-op unless the `managed-pg` feature is active.
// The body is empty without `managed-pg` (so it can't be `const` with it).
#[allow(clippy::missing_const_for_fn)]
fn exit_stop_managed_pg() {
    #[cfg(feature = "managed-pg")]
    {
        let _ = std::thread::spawn(crate::managed_pg::emergency_stop).join();
    }
}

pub(crate) fn is_dump_routes_mode() -> bool {
    std::env::var("AUTUMN_DUMP_ROUTES").as_deref() == Ok("1")
}

pub(crate) fn is_list_one_off_tasks_mode() -> bool {
    std::env::var("AUTUMN_LIST_TASKS").as_deref() == Ok("1")
}

fn one_off_task_name_from_env() -> Option<String> {
    std::env::var("AUTUMN_RUN_TASK")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn one_off_task_args_from_env() -> Result<Vec<String>, String> {
    match std::env::var("AUTUMN_TASK_ARGS_JSON") {
        Ok(raw) if !raw.trim().is_empty() => serde_json::from_str(&raw)
            .map_err(|error| format!("AUTUMN_TASK_ARGS_JSON must be a JSON string array: {error}")),
        _ => Ok(Vec::new()),
    }
}

fn print_available_one_off_tasks(tasks: &[crate::task::OneOffTaskInfo]) {
    let listing = crate::task::list_one_off_tasks(tasks);
    if listing.is_empty() {
        eprintln!("No one-off tasks are registered. Add .one_off_tasks(one_off_tasks![...]).");
        return;
    }

    eprintln!("Available tasks:");
    for task in listing {
        if task.description.is_empty() {
            eprintln!("  {}", task.name);
        } else {
            eprintln!("  {:<24} {}", task.name, task.description);
        }
    }
}

/// Start scheduled tasks in background Tokio tasks.
///
/// Each task runs in its own spawned task with error logging.
/// Uses `tokio::time` for fixed-delay scheduling and `croner` for cron-based
/// scheduling. The `shutdown` token is used to stop cron loops gracefully when
/// the server receives a termination signal.
#[allow(clippy::cast_possible_truncation)]
#[allow(clippy::cognitive_complexity)]
#[allow(dead_code)]
fn start_task_scheduler(
    tasks: Vec<crate::task::TaskInfo>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
) {
    if let Err(error) = start_task_scheduler_with_config(
        tasks,
        state,
        shutdown,
        &crate::config::SchedulerConfig::default(),
    ) {
        tracing::error!(error = %error, "scheduled task runtime initialization failed");
    }
}

#[allow(clippy::cast_possible_truncation)]
#[allow(clippy::cognitive_complexity)]
fn start_task_scheduler_with_config(
    tasks: Vec<crate::task::TaskInfo>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
    scheduler_config: &crate::config::SchedulerConfig,
) -> crate::AutumnResult<()> {
    tracing::info!(count = tasks.len(), "Starting scheduled tasks");
    let coordinator = crate::scheduler::coordinator_from_config(scheduler_config, state)?;
    let lease_ttl = std::time::Duration::from_secs(scheduler_config.lease_ttl_secs);
    for task_info in &tasks {
        let schedule_desc = task_info.schedule.to_string();
        tracing::info!(
            name = %task_info.name,
            schedule = %schedule_desc,
            coordination = %task_info.coordination,
            scheduler_backend = coordinator.backend(),
            replica_id = coordinator.replica_id(),
            lease_ttl_secs = scheduler_config.lease_ttl_secs,
            "Registered task"
        );
    }

    let mut cron_tasks: Vec<CronTaskSpec> = Vec::new();

    for task_info in tasks {
        let state = state.clone();
        let name = task_info.name.clone();
        let handler = task_info.handler;
        let coordination = task_info.coordination;
        let schedule_desc = task_info.schedule.to_string();
        state.task_registry.register_scheduled(
            &name,
            &schedule_desc,
            coordination,
            coordinator.backend(),
            coordinator.replica_id(),
        );

        match task_info.schedule {
            crate::task::Schedule::FixedDelay(delay) => {
                let coordinator = Arc::clone(&coordinator);
                let shutdown = shutdown.child_token();
                tokio::spawn(async move {
                    loop {
                        state
                            .task_registry
                            .record_next_run_at(&name, &format_next_task_run_after(delay));
                        tokio::select! {
                            () = shutdown.cancelled() => break,
                            () = tokio::time::sleep(delay) => {
                                execute_fixed_delay_task(
                                    name.clone(),
                                    state.clone(),
                                    handler,
                                    delay,
                                    coordination,
                                    Arc::clone(&coordinator),
                                    lease_ttl,
                                )
                                .await;
                            }
                        }
                    }
                });
            }
            crate::task::Schedule::Cron {
                expression,
                timezone,
            } => {
                cron_tasks.push(CronTaskSpec {
                    name,
                    expression,
                    timezone,
                    coordination,
                    handler,
                });
            }
        }
    }

    run_cron_scheduler(cron_tasks, state, shutdown, &coordinator, lease_ttl);

    Ok(())
}

#[allow(unused_variables, clippy::needless_pass_by_value)]
fn send_ws_sys_task_msg(
    state: &AppState,
    event: &str,
    name: &str,
    extra: Vec<(&str, serde_json::Value)>,
) {
    #[cfg(feature = "ws")]
    {
        // ⚡ Bolt Optimization:
        // Use serde_json::json! to avoid multiple String allocations (`.to_string()`)
        // and repetitive `Map::insert` calls for `sys:tasks` websocket messages.
        let mut msg = serde_json::json!({
            "event": event,
            "task": name,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });
        if let Some(map) = msg.as_object_mut() {
            for (k, v) in extra {
                map.insert(k.to_string(), v);
            }
        }
        let _ = state.channels().sender("sys:tasks").send(msg.to_string());
    }
}

async fn execute_task_result(
    state: &AppState,
    handler: crate::task::TaskHandler,
    start: std::time::Instant,
    name: &str,
    schedule: &'static str,
) -> Result<u64, (u64, String)> {
    // A fresh span per run so OTLP-enabled deployments see each invocation
    // as its own trace rather than inheriting whatever was current on the
    // scheduler thread.
    let task_span = tracing::info_span!(
        parent: None,
        "scheduled_task",
        otel.kind = "internal",
        task = %name,
        schedule = schedule,
    );
    let future = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (handler)(state.clone()).instrument(task_span)
    })) {
        Ok(future) => future,
        Err(panic) => {
            let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            return Err((duration_ms, format_scheduled_task_panic(panic.as_ref())));
        }
    };
    let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    match result {
        Ok(Ok(())) => Ok(duration_ms),
        Ok(Err(e)) => Err((duration_ms, e.to_string())),
        Err(panic) => Err((duration_ms, format_scheduled_task_panic(panic.as_ref()))),
    }
}

fn format_scheduled_task_panic(panic: &(dyn Any + Send)) -> String {
    let detail = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&'static str>().copied())
        .unwrap_or("non-string panic payload");
    format!("scheduled task handler panicked: {detail}")
}

async fn execute_task_result_with_optional_lease_ttl(
    state: &AppState,
    handler: crate::task::TaskHandler,
    start: std::time::Instant,
    name: &str,
    schedule: &'static str,
    lease_ttl: Option<std::time::Duration>,
) -> Result<u64, (u64, String)> {
    let Some(lease_ttl) = lease_ttl else {
        return execute_task_result(state, handler, start, name, schedule).await;
    };

    tokio::time::timeout(
        lease_ttl,
        execute_task_result(state, handler, start, name, schedule),
    )
    .await
    .unwrap_or_else(|_| {
        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        Err((
            duration_ms,
            format!(
                "scheduled task exceeded lease TTL of {}s",
                lease_ttl.as_secs()
            ),
        ))
    })
}

/// Handle the execution of a single fixed-delay task.
#[allow(clippy::cognitive_complexity)]
async fn execute_fixed_delay_task(
    name: String,
    state: AppState,
    handler: crate::task::TaskHandler,
    delay: std::time::Duration,
    coordination: crate::task::TaskCoordination,
    coordinator: Arc<dyn crate::scheduler::SchedulerCoordinator>,
    lease_ttl: std::time::Duration,
) {
    let tick_key = crate::scheduler::fixed_delay_tick_key(
        &name,
        delay,
        crate::time::clock_unix_duration(state.clock()),
    );
    let lease = match coordinator
        .try_acquire(&name, &tick_key, coordination)
        .await
    {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            tracing::debug!(task = %name, tick = %tick_key, "Scheduled task tick already claimed");
            return;
        }
        Err(error) => {
            tracing::warn!(task = %name, tick = %tick_key, error = %error, "Failed to acquire scheduled task lease");
            return;
        }
    };
    state
        .task_registry
        .record_leader(&name, lease.leader_id(), &tick_key);
    tracing::debug!(task = %name, "Running scheduled task");
    state.task_registry.record_start(&name);

    send_ws_sys_task_msg(&state, "started", &name, vec![]);

    let start = std::time::Instant::now();
    let lease_ttl = lease_ttl_for_run(&lease, coordination, lease_ttl);
    match execute_task_result_with_optional_lease_ttl(
        &state,
        handler,
        start,
        &name,
        "fixed_delay",
        lease_ttl,
    )
    .await
    {
        Ok(duration_ms) => {
            state.task_registry.record_success(&name, duration_ms);
            tracing::debug!(task = %name, "Task completed");
            send_ws_sys_task_msg(
                &state,
                "success",
                &name,
                vec![("duration_ms", serde_json::json!(duration_ms))],
            );
        }
        Err((duration_ms, error_str)) => {
            state
                .task_registry
                .record_failure(&name, duration_ms, &error_str);
            tracing::warn!(task = %name, error = %error_str, "Task failed");
            send_ws_sys_task_msg(
                &state,
                "failure",
                &name,
                vec![
                    ("duration_ms", serde_json::json!(duration_ms)),
                    ("error", serde_json::json!(error_str)),
                ],
            );
        }
    }

    if let Err(error) = lease.release().await {
        tracing::warn!(task = %name, tick = %tick_key, error = %error, "Failed to release scheduled task lease");
    }
}

/// Handle the execution of a single cron task.
#[allow(clippy::cognitive_complexity)]
async fn execute_cron_task(
    name: String,
    state: AppState,
    handler: crate::task::TaskHandler,
    coordination: crate::task::TaskCoordination,
    coordinator: Arc<dyn crate::scheduler::SchedulerCoordinator>,
    lease_ttl: std::time::Duration,
    scheduled_unix_secs: u64,
) {
    let tick_key = crate::scheduler::cron_tick_key(&name, scheduled_unix_secs);
    let lease = match coordinator
        .try_acquire(&name, &tick_key, coordination)
        .await
    {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            tracing::debug!(task = %name, tick = %tick_key, "Cron task tick already claimed");
            return;
        }
        Err(error) => {
            tracing::warn!(task = %name, tick = %tick_key, error = %error, "Failed to acquire cron task lease");
            return;
        }
    };
    state
        .task_registry
        .record_leader(&name, lease.leader_id(), &tick_key);
    tracing::debug!(task = %name, "Running cron task");
    state.task_registry.record_start(&name);

    send_ws_sys_task_msg(&state, "started", &name, vec![]);

    let start = std::time::Instant::now();
    let lease_ttl = lease_ttl_for_run(&lease, coordination, lease_ttl);
    match execute_task_result_with_optional_lease_ttl(
        &state, handler, start, &name, "cron", lease_ttl,
    )
    .await
    {
        Ok(duration_ms) => {
            state.task_registry.record_success(&name, duration_ms);
            tracing::debug!(task = %name, "Cron task completed");
            send_ws_sys_task_msg(
                &state,
                "success",
                &name,
                vec![("duration_ms", serde_json::json!(duration_ms))],
            );
        }
        Err((duration_ms, error_str)) => {
            state
                .task_registry
                .record_failure(&name, duration_ms, &error_str);
            tracing::warn!(task = %name, error = %error_str, "Cron task failed");
            send_ws_sys_task_msg(
                &state,
                "failure",
                &name,
                vec![
                    ("duration_ms", serde_json::json!(duration_ms)),
                    ("error", serde_json::json!(error_str)),
                ],
            );
        }
    }

    if let Err(error) = lease.release().await {
        tracing::warn!(task = %name, tick = %tick_key, error = %error, "Failed to release cron task lease");
    }
}

struct CronTaskSpec {
    name: String,
    expression: String,
    timezone: Option<String>,
    coordination: crate::task::TaskCoordination,
    handler: crate::task::TaskHandler,
}

fn lease_ttl_for_run(
    lease: &crate::scheduler::SchedulerLease,
    coordination: crate::task::TaskCoordination,
    lease_ttl: std::time::Duration,
) -> Option<std::time::Duration> {
    (coordination == crate::task::TaskCoordination::Fleet && lease.backend() == "postgres")
        .then_some(lease_ttl)
}

fn run_cron_scheduler(
    tasks: Vec<CronTaskSpec>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
    coordinator: &Arc<dyn crate::scheduler::SchedulerCoordinator>,
    lease_ttl: std::time::Duration,
) {
    if tasks.is_empty() {
        return;
    }

    tracing::info!(count = tasks.len(), "Cron scheduler started");
    for task in tasks {
        let state = state.clone();
        let coordinator = Arc::clone(coordinator);
        let shutdown = shutdown.child_token();
        tokio::spawn(async move {
            run_cron_task_loop(task, state, shutdown, coordinator, lease_ttl).await;
        });
    }
}

#[allow(clippy::cognitive_complexity)]
async fn run_cron_task_loop(
    task: CronTaskSpec,
    state: AppState,
    shutdown: tokio_util::sync::CancellationToken,
    coordinator: Arc<dyn crate::scheduler::SchedulerCoordinator>,
    lease_ttl: std::time::Duration,
) {
    let CronTaskSpec {
        name,
        expression,
        timezone,
        coordination,
        handler,
    } = task;

    let cron = match expression.parse::<croner::Cron>() {
        Ok(cron) => cron,
        Err(error) => {
            tracing::error!(task = %name, expression = %expression, error = %error, "Failed to create cron job");
            return;
        }
    };
    let timezone = timezone
        .as_deref()
        .and_then(|timezone| {
            timezone.parse::<chrono_tz::Tz>().map_or_else(
                |_| {
                    tracing::warn!(task = %name, timezone = %timezone, "Unrecognized timezone; falling back to UTC");
                    None
                },
                Some,
            )
        })
        .unwrap_or(chrono_tz::UTC);
    let mut cursor = chrono::Utc::now().with_timezone(&timezone);

    loop {
        let now = chrono::Utc::now().with_timezone(&timezone);
        let scheduled_at = match next_cron_occurrence_after(&cron, &cursor, &now) {
            Ok(scheduled_at) => scheduled_at,
            Err(error) => {
                tracing::error!(task = %name, expression = %expression, error = %error, "Failed to compute next cron tick");
                return;
            }
        };
        state.task_registry.record_next_run_at(
            &name,
            &scheduled_at.with_timezone(&chrono::Utc).to_rfc3339(),
        );
        let sleep_for = cron_sleep_duration_until(&scheduled_at);
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(sleep_for) => {
                let woke_at = chrono::Utc::now().with_timezone(&timezone);
                match cron_occurrence_is_overdue(&cron, &scheduled_at, &woke_at) {
                    Ok(true) => {
                        tracing::warn!(
                            task = %name,
                            scheduled_at = %scheduled_at,
                            woke_at = %woke_at,
                            "Skipping overdue cron task tick"
                        );
                        cursor = woke_at;
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::error!(task = %name, expression = %expression, error = %error, "Failed to evaluate cron tick lateness");
                        return;
                    }
                }
                let scheduled_unix_secs = u64::try_from(scheduled_at.timestamp()).unwrap_or_default();
                tokio::spawn(execute_cron_task(
                    name.clone(),
                    state.clone(),
                    handler,
                    coordination,
                    Arc::clone(&coordinator),
                    lease_ttl,
                    scheduled_unix_secs,
                ));
                cursor = scheduled_at;
            }
        }
    }
}

fn format_next_task_run_after(delay: std::time::Duration) -> String {
    let now = chrono::Utc::now();
    let Ok(delay) = chrono::TimeDelta::from_std(delay) else {
        return now.to_rfc3339();
    };
    (now + delay).to_rfc3339()
}

fn next_cron_occurrence_after<Tz: chrono::TimeZone>(
    cron: &croner::Cron,
    cursor: &chrono::DateTime<Tz>,
    now: &chrono::DateTime<Tz>,
) -> Result<chrono::DateTime<Tz>, croner::errors::CronError> {
    let anchor = if cursor < now { now } else { cursor };
    cron.find_next_occurrence(anchor, false)
}

fn cron_occurrence_is_overdue<Tz: chrono::TimeZone>(
    cron: &croner::Cron,
    scheduled_at: &chrono::DateTime<Tz>,
    now: &chrono::DateTime<Tz>,
) -> Result<bool, croner::errors::CronError> {
    let next_after_scheduled = cron.find_next_occurrence(scheduled_at, false)?;
    Ok(&next_after_scheduled <= now)
}

fn cron_sleep_duration_until<Tz: chrono::TimeZone>(
    scheduled_at: &chrono::DateTime<Tz>,
) -> std::time::Duration {
    scheduled_at
        .with_timezone(&chrono::Utc)
        .signed_duration_since(chrono::Utc::now())
        .to_std()
        .unwrap_or_default()
}

async fn run_startup_hooks(hooks: &[StartupHook], state: AppState) -> crate::AutumnResult<()> {
    for hook in hooks {
        hook(state.clone()).await?;
    }
    Ok(())
}

fn run_state_initializers(initializers: Vec<StateInitializer>, state: &AppState) {
    for initializer in initializers {
        initializer(state);
    }
}

/// Wire the typed event bus into the app at build time.
///
/// Builds the [`EventRegistry`](crate::events::EventRegistry) from registered
/// listeners, installs it onto `state` for the [`Events`](crate::events::Events)
/// extractor, appends a job per durable listener so they ride the job runtime
/// (retry + DLQ + restart-safety), and initializes the process-global bus used
/// by the module-level `events::publish`.
fn finalize_event_bus(
    listeners: Vec<crate::events::ListenerInfo>,
    jobs: &mut Vec<crate::job::JobInfo>,
    state: &AppState,
) {
    let registry = crate::events::EventRegistry::from_listeners(listeners);
    jobs.extend(registry.durable_job_infos());
    state.insert_extension(registry.clone());
    crate::events::init_global_event_bus(&registry, state, None);
}

fn initialize_job_runtime(
    jobs: Vec<crate::job::JobInfo>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
    config: &crate::config::JobConfig,
) -> crate::AutumnResult<()> {
    crate::job::clear_global_job_client();
    if jobs.is_empty() {
        Ok(())
    } else {
        crate::job::start_runtime(jobs, state, shutdown, config)
    }
}

/// A bound network listener for the server, abstracting over the transport.
///
/// `run()` binds one of these based on `config.server.unix_socket`: a TCP
/// listener on `host:port` (the default) or a Unix domain socket (local
/// daemon mode). The two carry different connect-info types, so the serve
/// task is spawned per-variant.
enum BoundListener {
    /// TCP listener on `host:port`.
    Tcp(tokio::net::TcpListener),
    /// Unix domain socket listener (local daemon transport).
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

/// Connection info for a Unix-domain-socket request.
///
/// axum's `into_make_service_with_connect_info::<C>` requires `C:
/// Connected<IncomingStream>`. Unlike TCP there is no peer `SocketAddr` for a
/// Unix socket, so this carries no data — it exists purely to satisfy the
/// connect-info bound on the UDS serve path.
#[cfg(unix)]
#[derive(Clone, Debug)]
struct UdsConnectInfo;

#[cfg(unix)]
impl
    axum::extract::connect_info::Connected<
        axum::serve::IncomingStream<'_, tokio::net::UnixListener>,
    > for UdsConnectInfo
{
    fn connect_info(_stream: axum::serve::IncomingStream<'_, tokio::net::UnixListener>) -> Self {
        Self
    }
}

/// Stamp a loopback peer (`127.0.0.1`) on Unix-domain-socket requests.
///
/// A UDS connection has no TCP peer `SocketAddr`, so without this the
/// trusted-proxy resolver and the [`ClientAddr`](crate::extract::ClientAddr)
/// extractor resolve no client address — breaking any route or middleware that
/// requires `ClientAddr` and any IP-based maintenance/rate-limit behavior. Local
/// daemon requests are loopback-equivalent, so present them as a `127.0.0.1`
/// connection (matching how an equivalent localhost TCP request is treated).
/// Installed before `TrustedProxiesLayer` on the UDS serve path only.
#[cfg(unix)]
async fn stamp_loopback_connect_info(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .is_none()
    {
        let loopback =
            std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(loopback));
    }
    next.run(req).await
}

/// Signal `autumn serve --daemon`'s supervisor that startup is complete.
///
/// The CLI passes a path via `AUTUMN_SERVE_READY_FILE` and polls for it; we
/// create it here, immediately after [`mark_startup_complete`], so the
/// supervisor's notion of "ready" means the socket is bound and serving *and*
/// startup hooks/migrations have finished — with no dependence on the app's HTTP
/// middleware (the startup barrier, maintenance mode, rate limiting, or custom
/// health paths, which an HTTP readiness probe would all have to thread).
///
/// The file's contents are the app's *resolved* graceful-drain budget in seconds
/// (`prestop_grace_secs + shutdown_timeout_secs`). The supervisor records this so
/// `autumn serve stop` waits for the budget the app will actually drain for —
/// even when a custom `with_config_loader` set it — instead of reconstructing it
/// from TOML/env and risking a premature `SIGKILL`.
///
/// Best-effort: a write failure only delays readiness detection until the
/// supervisor's timeout, and a non-daemon run leaves the variable unset (no-op).
///
/// [`mark_startup_complete`]: crate::probe::ProbeState::mark_startup_complete
fn signal_serve_ready(drain_budget_secs: u64) {
    let Some(path) = std::env::var_os("AUTUMN_SERVE_READY_FILE") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let path = std::path::PathBuf::from(path);
    // Write to a temp sibling and rename into place so the supervisor — which
    // polls for the file's existence and then reads the budget from it — never
    // observes a half-written file: it appears atomically with its full
    // contents. A plain `write` would make the path exist before the bytes land.
    let mut tmp = path.clone();
    tmp.as_mut_os_string().push(".tmp");
    if let Err(e) = std::fs::write(&tmp, drain_budget_secs.to_string())
        .and_then(|()| std::fs::rename(&tmp, &path))
    {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!(error = %e, path = %path.display(),
            "could not write serve readiness file");
    }
}

/// Prepare a Unix-socket path for binding: remove a *stale* socket left by a
/// previous run, but refuse to touch a non-socket file (guards against
/// clobbering a regular file) or a socket with a **live** listener (probed via
/// `connect`; clobbering it would silently make that service unreachable —
/// instead we fail like a TCP `EADDRINUSE`). A missing path is fine.
///
/// # Errors
///
/// Returns an error if the path exists and is not a socket, names a live
/// listener, or the stale socket cannot be removed.
#[cfg(unix)]
fn prepare_unix_socket_path(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                // A successful connect means another process is listening here.
                Ok(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "refusing to bind unix socket: {} is already in use by a \
                         live listener",
                        path.display()
                    ),
                )),
                // `ECONNREFUSED` (no listener) — or the path vanishing — means the
                // socket is stale; reclaim it.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path)
                }
                // `EACCES`/`EPERM` (or any other error): the socket may be a live,
                // operator-managed listener whose mode/ACL denies us. Connecting
                // failed, but liveness is unproven — refuse rather than clobber a
                // possibly-live service.
                Err(e) => Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "refusing to bind unix socket: cannot determine whether {} \
                         is live ({e}); not removing it",
                        path.display()
                    ),
                )),
            }
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to bind unix socket: {} exists and is not a socket",
                path.display()
            ),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

async fn run_shutdown_hooks(hooks: &[ShutdownHook]) {
    for hook in hooks.iter().rev() {
        hook().await;
    }
}

/// Run shutdown hooks in reverse-registration order (LIFO), enforcing a
/// per-hook timeout and a hard total-budget ceiling.
///
/// Plugin ordering rule: plugins register hooks during `build()`, which is
/// called before any app `on_shutdown` calls, so app hooks run **before**
/// plugin hooks (LIFO means last-registered runs first).
///
/// Overruns are logged at WARN but do not block the remaining budget.
async fn run_shutdown_hooks_with_timeout(
    hooks: &[ShutdownHook],
    per_hook_budget: std::time::Duration,
    total_budget: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now().checked_add(total_budget).unwrap_or_else(tokio::time::Instant::now);
    for hook in hooks.iter().rev() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!("shutdown: total hook budget exhausted; skipping remaining hooks");
            break;
        }
        let timeout = remaining.min(per_hook_budget);
        // Hook overruns are intentionally non-fatal (exit 0 per ADR addendum).
        // Only drain deadline exhaustion (phase 6) triggers exit(1).
        if tokio::time::timeout(timeout, hook()).await.is_err() {
            tracing::warn!(
                per_hook_budget_ms = timeout.as_millis(),
                "shutdown: hook overran per-hook timeout; continuing with remaining budget"
            );
        }
    }
}

/// Log a structured startup transparency report.
///
/// Activated by setting `AUTUMN_SHOW_CONFIG=1` (or `autumn dev --show-config`).
/// Prints all registered routes, scheduled tasks, active middleware, and
/// resolved configuration to the `INFO` log so developers can see exactly
/// what the macros and conventions configured.
#[allow(clippy::cognitive_complexity)]
fn log_startup_transparency(
    routes: &[Route],
    tasks: &[crate::task::TaskInfo],
    scoped_groups: &[ScopedGroup],
    config: &AutumnConfig,
) {
    tracing::info!(
        "Registered routes:{}",
        format_route_lines(routes, scoped_groups, config)
    );

    if let Some(task_lines) = format_task_lines(tasks) {
        tracing::info!("Scheduled tasks:{task_lines}");
    }

    tracing::info!("Active middleware: {}", format_middleware_list(config));

    tracing::info!("Configuration:{}", format_config_summary(config));
}

/// Fail the boot fast (before any DB side effects) when the default
/// session backend is misconfigured.
///
/// `AutumnConfig::validate()` is intentionally session-agnostic so that a
/// custom [`SessionStore`](crate::session::SessionStore) installed via
/// [`AppBuilder::with_session_store`] can override an otherwise-invalid
/// `session.backend = "redis"`-without-`redis.url` config. But when no
/// custom store is installed, the config-driven path will fail later in
/// `apply_session_layer` — and by then, `setup_database` has already run
/// migrations, leaving DB side effects from a doomed boot. This helper
/// runs the same `backend_plan` check `apply_session_layer` does, but
/// before any side effects, and only when the override path is inactive.
fn fail_fast_on_invalid_session_config(config: &AutumnConfig, has_custom_session_store: bool) {
    if has_custom_session_store {
        return;
    }
    if let Err(error) = config.session.backend_plan(config.profile.as_deref()) {
        eprintln!("Invalid session backend config: {error}");
        std::process::exit(1);
    }
}

/// Resolve at-rest column-encryption keys at boot (#805).
///
/// On success this installs the process-global key ring. When encrypted columns
/// are registered but the key material under `active_record_encryption` is
/// missing or malformed, the behaviour mirrors the signing-secret check (#597):
/// a **hard failure in production** (the server must not bind with unusable
/// encryption), but only a **warning in dev/test** so zero-config local
/// development and the example apps continue to run. Apps that do not opt into
/// encrypted columns are unaffected (no registered columns -> no-op).
fn fail_fast_on_missing_encryption_keys(config: &AutumnConfig) {
    if let Err(diagnostic) = crate::encryption::init_attribute_encryption(config.credentials()) {
        let is_production = matches!(config.profile.as_deref(), Some("prod" | "production"));
        if is_production {
            eprintln!("Attribute encryption misconfiguration: {diagnostic}");
            std::process::exit(1);
        }
        eprintln!(
            "warning: attribute encryption is not fully configured (dev): {diagnostic}\n  \
             note: encrypted-column reads/writes will fail until keys are set; \
             this is a hard error in production."
        );
    }
}

/// Fail immediately if the signing secret is misconfigured for the active profile.
///
/// In production, a missing, too-short, or demo-valued signing secret is a
/// hard failure — the server must not bind. In dev/test the check is skipped
/// so zero-config local development continues to work.
fn fail_fast_on_invalid_signing_secret(config: &AutumnConfig) {
    use crate::security::config::validate_signing_secret;

    let is_production = matches!(config.profile.as_deref(), Some("prod" | "production"));
    let secret = config.security.signing_secret.secret.as_deref();

    if let Err(error) = validate_signing_secret(secret, is_production) {
        eprintln!("Invalid signing secret configuration: {error}");
        eprintln!(
            "  hint: generate a secret with `openssl rand -hex 32` and set \
             AUTUMN_SECURITY__SIGNING_SECRET"
        );
        std::process::exit(1);
    }

    // Previous secrets accepted during rotation must meet the same bar as the
    // current secret — a weak previous key can still be used to forge tokens.
    if is_production {
        for (i, prev) in config
            .security
            .signing_secret
            .previous_secrets
            .iter()
            .enumerate()
        {
            if let Err(error) = validate_signing_secret(Some(prev.as_str()), true) {
                eprintln!("Invalid signing secret configuration: previous_secrets[{i}]: {error}");
                eprintln!(
                    "  hint: every previous secret must meet the same entropy requirement \
                     as the current secret"
                );
                std::process::exit(1);
            }
        }
    }
}

fn fail_fast_on_invalid_webhook_config(config: &AutumnConfig) {
    let is_production = matches!(config.profile.as_deref(), Some("prod" | "production"));
    if let Err(error) = config.security.webhooks.validate(is_production) {
        eprintln!("Invalid signed webhook configuration: {error}");
        std::process::exit(1);
    }
}

fn fail_fast_on_invalid_trusted_hosts(config: &AutumnConfig) {
    let is_production = matches!(config.profile.as_deref(), Some("prod" | "production"));
    if !is_production {
        return;
    }
    let hosts: Vec<String> = config
        .security
        .trusted_hosts
        .hosts
        .iter()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .collect();
    if hosts.is_empty() {
        eprintln!(
            "[security.trusted_hosts] is required in production; set hosts = [\"example.com\"] or explicit entries"
        );
        std::process::exit(1);
    }
    if hosts.iter().any(|h| h == "*") {
        tracing::warn!("trusted host validation disabled via wildcard '*' in production");
    }
}

fn fail_fast_on_invalid_idempotency_config(config: &AutumnConfig) {
    if !config.idempotency.enabled.unwrap_or(false) {
        return;
    }
    let is_production = matches!(config.profile.as_deref(), Some("prod" | "production"));
    if is_production
        && config.idempotency.backend == crate::config::IdempotencyBackend::Memory
        && !config.idempotency.allow_memory_in_production
    {
        eprintln!(
            "The in-memory idempotency backend is not safe for multi-replica production use.\n\
             Set `[idempotency] backend = \"redis\"` in autumn.toml, or set \
             `allow_memory_in_production = true` to suppress this check."
        );
        std::process::exit(1);
    }
    #[cfg(feature = "redis")]
    if config.idempotency.backend == crate::config::IdempotencyBackend::Redis {
        let url_missing = config
            .idempotency
            .redis
            .url
            .as_deref()
            .is_none_or(|u| u.trim().is_empty());
        if url_missing {
            eprintln!(
                "Redis idempotency backend requires a connection URL.\n\
                 Set AUTUMN_IDEMPOTENCY__REDIS__URL or `[idempotency.redis] url` in autumn.toml."
            );
            std::process::exit(1);
        }
    }
}

pub(crate) fn install_webhook_registry(state: &AppState, config: &AutumnConfig) {
    if let Err(error) =
        crate::webhook::install_registry_from_config(state, &config.security.webhooks)
    {
        eprintln!("Invalid signed webhook configuration: {error}");
        std::process::exit(1);
    }
}

/// Constructed [`BlobStore`](crate::storage::BlobStore) plus the
/// optional axum router that serves signed URLs for the Local backend.
/// Returned by [`preflight_storage`] before any DB side effects so a
/// doomed boot can't run migrations first; installed onto
/// [`AppState`] later via [`StorageBootstrap::install`].
#[cfg(feature = "storage")]
struct StorageBootstrap {
    store: crate::storage::SharedBlobStore,
    serving: Option<axum::Router<AppState>>,
}

#[cfg(feature = "storage")]
impl StorageBootstrap {
    /// Install the preflighted store on `AppState` and return the
    /// optional serving router so the caller can merge it into the
    /// app router.
    fn install(self, state: &AppState) -> Option<axum::Router<AppState>> {
        state.insert_extension::<crate::storage::BlobStoreState>(
            crate::storage::BlobStoreState::new(self.store),
        );
        self.serving
    }
}

/// Provision the configured [`BlobStore`](crate::storage::BlobStore)
/// before any database side effects. Construction is the side-effecting
/// step (creates + canonicalizes the storage root, may
/// `process::exit(1)` on a misconfiguration); we deliberately run it
/// before `setup_database` so a doomed boot doesn't apply migrations
/// first. Installation onto `AppState` happens later via
/// [`StorageBootstrap::install`].
#[cfg(feature = "storage")]
#[allow(clippy::too_many_lines)] // Single switch over backend variants reads as one unit.
fn preflight_storage(config: &AutumnConfig) -> Option<StorageBootstrap> {
    use crate::storage::StorageBackendPlan;

    let plan = config
        .storage
        .backend_plan(config.profile.as_deref())
        .unwrap_or_else(|error| {
            // Cover the cases `backend_plan` rejects up front:
            // `LocalInProduction` (prod + local without ack),
            // `MissingS3Bucket`/`MissingS3Region`/`S3FeatureDisabled`.
            // Each is a configuration mistake — fail the boot loudly
            // rather than running migrations and then dying.
            tracing::error!(%error, "invalid storage backend config; aborting startup");
            std::process::exit(1);
        });

    match plan {
        StorageBackendPlan::Disabled => None,
        StorageBackendPlan::Local {
            provider_id,
            root,
            mount_path,
            default_url_expiry_secs,
            warn_in_production,
        } => Some(bootstrap_local_storage(
            config,
            &provider_id,
            &root,
            &mount_path,
            default_url_expiry_secs,
            warn_in_production,
        )),
        StorageBackendPlan::S3 { .. } => {
            // `storage.backend = "s3"` requires the `autumn-storage-s3` plugin.
            // Construct an `S3BlobStore` and register it with `.with_blob_store()`
            // before calling `.run()` — when you do, the custom store bypasses
            // this path entirely and `preflight_storage` is never called.
            tracing::error!(
                "storage.backend=s3 requires the `autumn-storage-s3` plugin. \
                 Add it to your Cargo.toml, build an S3BlobStore from your config, \
                 and call `.with_blob_store(store)` on your AppBuilder. \
                 Aborting startup."
            );
            std::process::exit(1);
        }
    }
}

#[cfg(feature = "storage")]
fn bootstrap_local_storage(
    config: &AutumnConfig,
    provider_id: &str,
    root: &std::path::Path,
    mount_path: &str,
    default_url_expiry_secs: u64,
    warn_in_production: bool,
) -> StorageBootstrap {
    use crate::storage::{LocalBlobStore, SharedBlobStore, local::SigningKey};

    if warn_in_production {
        tracing::warn!(
            "prod profile is using the local-disk blob store; \
             bytes won't survive replica turnover. Set \
             storage.backend=s3 or storage.allow_local_in_production=true \
             to acknowledge"
        );
    }

    // Signing key precedence:
    // 1. security.signing_secret (canonical, shared with session/CSRF)
    // 2. storage.local.signing_key (legacy override — still respected)
    // 3. Random ephemeral key (dev only — warns in prod)
    let (signing_key, previous_signing_keys) = config
        .security
        .signing_secret
        .secret
        .as_deref()
        .filter(|s| !s.is_empty())
        .map_or_else(
            || {
                config
                    .storage
                    .local
                    .signing_key
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map_or_else(
                        || {
                            if matches!(config.profile.as_deref(), Some("prod" | "production")) {
                                tracing::warn!(
                                    "no signing secret configured in prod; blob URL signatures \
                                     won't survive a process restart. Set \
                                     AUTUMN_SECURITY__SIGNING_SECRET."
                                );
                            }
                            (SigningKey::random(), vec![])
                        },
                        |legacy| (SigningKey::new(legacy.as_bytes().to_vec()), vec![]),
                    )
            },
            |secret| {
                let current = SigningKey::new(secret.as_bytes().to_vec());
                let previous = config
                    .security
                    .signing_secret
                    .previous_secrets
                    .iter()
                    .map(|s| SigningKey::new(s.as_bytes().to_vec()))
                    .collect::<Vec<_>>();
                (current, previous)
            },
        );

    let store = match LocalBlobStore::new(
        provider_id.to_string(),
        root.to_path_buf(),
        mount_path.to_string(),
        std::time::Duration::from_secs(default_url_expiry_secs),
        signing_key,
        previous_signing_keys,
    ) {
        Ok(store) => store,
        Err(err) => {
            // The operator explicitly chose `storage.backend = "local"`
            // — a non-writable root means uploads can't possibly
            // work, so abort the boot rather than letting upload
            // handlers serve 500s after deploy.
            tracing::error!(
                error = %err,
                root = %root.display(),
                "failed to initialize local blob store; aborting startup"
            );
            std::process::exit(1);
        }
    };

    let serving = crate::storage::local::serve_router(&store);
    let arc: SharedBlobStore = std::sync::Arc::new(store);

    tracing::info!(
        provider = %provider_id,
        root = %root.display(),
        mount = %mount_path,
        "Local blob store mounted"
    );

    StorageBootstrap {
        store: arc,
        serving: Some(serving),
    }
}
async fn load_config_and_telemetry(
    config_loader: Option<ConfigLoaderFactory>,
    telemetry_provider: Option<Box<dyn crate::telemetry::TelemetryProvider>>,
) -> (AutumnConfig, crate::telemetry::TelemetryGuard) {
    // 1. Load configuration via the installed loader, falling back to the
    //    five-layer TOML + env default.
    let mut config = match config_loader {
        Some(factory) => factory().await,
        None => crate::config::TomlEnvConfigLoader::new().load().await,
    }
    .unwrap_or_else(|e| {
        eprintln!("Failed to load configuration: {e}");
        std::process::exit(1);
    });

    // `autumn serve --daemon` binds the app on a private Unix socket and then
    // discovers/health-probes it by path. A custom `with_config_loader` can
    // construct its `ServerConfig` from scratch and silently drop the
    // `AUTUMN_SERVER__UNIX_SOCKET` env override, leaving the daemon on TCP where
    // the supervisor can't reach it. The CLI therefore also passes the socket
    // out-of-band via `AUTUMN_SERVE_FORCE_UNIX_SOCKET`, applied here *after* the
    // loader runs so no loader can drop it.
    if let Ok(forced) = std::env::var("AUTUMN_SERVE_FORCE_UNIX_SOCKET")
        && !forced.is_empty()
    {
        config.server.unix_socket = Some(forced);
    }

    // 2. Initialize logging/telemetry via the installed provider, falling
    //    back to the default `tracing-subscriber + OTLP` initializer.
    let provider: Box<dyn crate::telemetry::TelemetryProvider> = telemetry_provider
        .unwrap_or_else(|| Box::new(crate::telemetry::TracingOtlpTelemetryProvider::new()));
    let telemetry_guard = provider
        .init(&config.log, &config.telemetry, config.profile.as_deref())
        .unwrap_or_else(|error| {
            eprintln!("Failed to initialize telemetry: {error}");
            std::process::exit(1);
        });

    (config, telemetry_guard)
}

/// Register the embedded `static/` tree (if any) as the process-wide asset
/// source. Called by each `run` path before the router is built so `/static/*`
/// serves from the binary and `asset_url()` resolves against the embedded
/// manifest.
#[cfg(feature = "embed-assets")]
fn register_embedded_static_dir(embedded_static: Option<crate::assets::EmbeddedStaticDir>) {
    if let Some(dir) = embedded_static {
        crate::assets::register_embedded_static(dir);
    }
}

/// Prefer an embedded locale bundle over disk auto-loading when no explicit
/// bundle was provided. Returns `explicit` unchanged when it is `Some` or when
/// no embedded locales were registered.
#[cfg(all(feature = "embed-assets", feature = "i18n"))]
fn embedded_i18n_bundle(
    explicit: Option<Arc<crate::i18n::Bundle>>,
    embedded_locales: Option<&'static include_dir::Dir<'static>>,
    config: &AutumnConfig,
) -> Option<Arc<crate::i18n::Bundle>> {
    explicit.or_else(|| {
        embedded_locales.map(|dir| {
            Arc::new(
                crate::i18n::Bundle::load_from_embedded(dir, &config.i18n)
                    .unwrap_or_else(|e| panic!("embedded_locales: {e}")),
            )
        })
    })
}

#[cfg(feature = "i18n")]
fn resolve_i18n_bundle(
    explicit_bundle: Option<Arc<crate::i18n::Bundle>>,
    auto_load: bool,
    config: &AutumnConfig,
    env: &dyn crate::config::Env,
) -> Option<Arc<crate::i18n::Bundle>> {
    if explicit_bundle.is_some() {
        return explicit_bundle;
    }
    if !auto_load {
        return None;
    }

    let dir = project_dir(&config.i18n.dir, env);
    Some(Arc::new(
        crate::i18n::Bundle::load_from_dir(&dir, &config.i18n)
            .unwrap_or_else(|e| panic!("i18n_auto: {e}")),
    ))
}

#[cfg(feature = "i18n")]
fn install_i18n_bundle_layer(
    mut custom_layers: Vec<CustomLayerRegistration>,
    state: &AppState,
    bundle: Option<Arc<crate::i18n::Bundle>>,
) -> Vec<CustomLayerRegistration> {
    let Some(bundle) = bundle else {
        return custom_layers;
    };

    tracing::info!(
        locales = ?bundle.locales(),
        default = bundle.default_locale(),
        "i18n bundle loaded"
    );
    state.insert_extension::<Arc<crate::i18n::Bundle>>(bundle.clone());
    // Use the existing IntoAppLayer plumbing so the Extension is visible to
    // every request. axum::Extension<T> is itself a tower::Layer when T:
    // Clone + Send + Sync + 'static.
    let ext_layer = axum::Extension(bundle);
    custom_layers.push(CustomLayerRegistration {
        type_id: TypeId::of::<axum::Extension<Arc<crate::i18n::Bundle>>>(),
        type_name: std::any::type_name::<axum::Extension<Arc<crate::i18n::Bundle>>>(),
        apply: Box::new(move |router| router.layer(ext_layer)),
    });
    custom_layers
}

#[cfg(feature = "db")]
struct DatabaseBootstrap {
    topology: Option<crate::db::DatabaseTopology>,
    shards: Option<crate::sharding::ShardSet>,
    replica_readiness: Option<crate::migrate::ReplicaMigrationReadiness>,
    replica_migration_check: Option<(String, String)>,
}

/// Build the `ShardSet` for a sharded app (or `None` when no `[[database.shards]]`
/// are configured). Resolves the shard router first: an explicit
/// `with_shard_router` wins; otherwise `directory_routing_enabled` opts into the
/// control-DB directory router (bound to the just-built control primary pool);
/// otherwise the hash router. The directory flag is documented as having no
/// effect without shards, so a shardless profile that leaves it enabled must not
/// fail startup — hence the early `None` return.
///
/// `spawn_directory_listener` gates the directory-router cache-invalidation
/// listener: it opens control-DB connections, so it is spawned only at real
/// runtime, never during a static build (`autumn build`) which must not touch
/// the database.
#[cfg(feature = "db")]
async fn resolve_shard_set(
    config: &AutumnConfig,
    shard_router: Option<Arc<dyn crate::sharding::ShardRouter>>,
    shard_provider: Option<ShardProviderFactory>,
    directory_routing_enabled: bool,
    spawn_directory_listener: bool,
    topology: Option<&crate::db::DatabaseTopology>,
) -> Result<Option<crate::sharding::ShardSet>, String> {
    if !config.database.has_shards() {
        return Ok(None);
    }
    let router: Arc<dyn crate::sharding::ShardRouter> = match shard_router {
        Some(explicit) => explicit,
        None if directory_routing_enabled => {
            let control_primary = topology
                .map(crate::db::DatabaseTopology::primary)
                .ok_or_else(|| {
                    "directory_shard_router is enabled but no control database is configured. \
                     The directory router needs a control `database.primary_url`/`url` to read \
                     the tenant→shard directory. Set one, or disable directory routing to use \
                     the hash router."
                        .to_owned()
                })?;
            // Directory routing resolves the tenant→shard key by checking out a
            // *second* control connection during extraction. A handler that
            // already holds `Db` (or another control checkout) before extracting
            // `ShardedDb` / a sharded repository would then deadlock on a control
            // pool sized to 1 — the first checkout cannot be released until the
            // handler runs. Require at least 2 control connections so these
            // mixed control+tenant handlers always make progress.
            let control_max = control_primary.status().max_size;
            if control_max < 2 {
                return Err(format!(
                    "directory_shard_router requires a control database pool of at least 2 \
                     connections, but the configured maximum is {control_max}. Directory \
                     routing checks out a second control connection during extraction to \
                     resolve the tenant→shard key, which deadlocks a pool sized to 1 when a \
                     handler already holds a control connection (e.g. `Db` + `ShardedDb`). \
                     Increase the control pool size (database.pool.max_size), or disable \
                     directory routing to use the hash router."
                ));
            }
            // Bound directory lookups with the configured database statement
            // timeout (capped to Postgres' i32 millisecond range).
            let timeout_ms = config.database.statement_timeout.map_or(0, |d| {
                u64::try_from(d.as_millis())
                    .unwrap_or(i32::MAX as u64)
                    .min(i32::MAX as u64)
            });
            let dir_router = Arc::new(
                crate::sharding::DirectoryShardRouter::new(control_primary.clone())
                    .with_statement_timeout_ms(timeout_ms),
            );
            // Spawn the cache-invalidation listener on the control DB so a re-pin
            // (e.g. during a slot move) evicts cached tenant→shard mappings fleet-
            // wide the moment it commits (LISTEN/NOTIFY) rather than waiting out
            // the TTL. Skipped during a static build (no DB access); needs the
            // control URL, without one we silently fall back to TTL-only refresh.
            if spawn_directory_listener {
                // Prefer the provider-resolved control URL carried on the
                // topology (managed Postgres has no `database.primary_url` in
                // config); fall back to the configured URL. Without this a
                // managed control DB would get no LISTEN/NOTIFY task (absent
                // URL) or listen on a stale pre-provider URL.
                if let Some(control_url) = topology
                    .and_then(crate::db::DatabaseTopology::migration_url)
                    .or_else(|| config.database.effective_primary_url())
                {
                    // Detach: the listener runs for the life of the process;
                    // dropping the JoinHandle leaves the task running rather than
                    // aborting it.
                    drop(
                        crate::sharding::DirectoryShardRouter::spawn_invalidation_listener(
                            Arc::clone(&dir_router),
                            control_url.to_owned(),
                            crate::sharding::DEFAULT_DIRECTORY_INVALIDATION_SWEEP_INTERVAL,
                        ),
                    );
                } else {
                    // Directory routing is active but there is no control URL to
                    // open a dedicated LISTEN connection — e.g. a custom
                    // `DatabasePoolProvider` supplied the control pool without
                    // `database.primary_url`/`url`. The router still serves
                    // lookups from the provided pool, but re-pins won't be
                    // invalidated fleet-wide on commit; they only take effect
                    // after the cache TTL expires. Warn rather than fall back
                    // silently so operators relying on the directory for slot
                    // moves can configure a control URL (or accept TTL-only
                    // refresh) deliberately.
                    tracing::warn!(
                        "directory shard routing is enabled but no control database URL is \
                         configured (database.primary_url/url is unset, e.g. a custom \
                         DatabasePoolProvider supplied the control pool); the cache-\
                         invalidation LISTEN/NOTIFY task cannot be started, so directory \
                         re-pins will only take effect after the cache TTL expires rather \
                         than fleet-wide on commit"
                    );
                }
            }
            dir_router
        }
        None => Arc::new(crate::sharding::HashShardRouter),
    };
    let set = match shard_provider {
        Some(factory) => {
            let topologies = factory(config.database.clone())
                .await
                .map_err(|e| format!("Failed to create shard pools: {e}"))?;
            crate::sharding::build_shard_set(&config.database, topologies, router)
        }
        None => crate::sharding::create_shard_set(&config.database, router)
            .map(|set| set.expect("has_shards() checked above")),
    }
    .map_err(|e| format!("Failed to configure shards: {e}"))?;
    Ok(Some(set))
}

#[cfg(feature = "db")]
async fn setup_database(
    config: &AutumnConfig,
    migrations: Vec<crate::migrate::EmbeddedMigrations>,
    pool_provider: Option<PoolProviderFactory>,
    shard_provider: Option<ShardProviderFactory>,
    shard_router: Option<Arc<dyn crate::sharding::ShardRouter>>,
    directory_shard_router: bool,
    hook_queue_migration_mode: RepositoryCommitHookQueueMigrationMode,
) -> Result<DatabaseBootstrap, String> {
    let migrations = migrations_with_repository_framework_migrations(
        migrations,
        crate::repository_commit_hooks::has_repository_commit_hook_descriptors(),
        crate::version_history::has_versioned_repository_descriptors(),
        hook_queue_migration_mode,
    );
    // Directory routing is only actually active when the app did NOT supply an
    // explicit shard router: an explicit `with_shard_router(...)` takes
    // precedence over `directory_shard_router` in `resolve_shard_set`, so in
    // that case the `DirectoryShardRouter` is never constructed and the
    // directory table is never consulted. Gate the migration on the same
    // condition so an explicit-router app doesn't create `_autumn_shard_directory`
    // (or warn about a pending directory migration) for a table it won't use.
    let use_directory_router = shard_router.is_none()
        && (directory_shard_router || config.database.directory_shard_router);
    // The tenant→shard directory table is a CONTROL-plane table: create it at
    // startup only when directory routing is active (and shards exist), and
    // only on the control target — not via the shared list above, which is also
    // applied to every shard. Like the other runtime framework migrations, it is
    // suppressed during a static build (`autumn build`, AUTUMN_BUILD_STATIC=1):
    // the build only renders assets and must not touch the database, so it must
    // not create `_autumn_shard_directory`.
    let directory_migration_required = directory_migration_is_required(
        use_directory_router,
        config.database.has_shards(),
        hook_queue_migration_mode,
    );
    let shard_map_migration_required =
        shard_map_migration_is_required(config.database.has_shards(), hook_queue_migration_mode);
    let check_replica_migrations = !migrations.is_empty();
    let topology = match pool_provider {
        Some(factory) => factory(config.database.clone()).await,
        None => crate::db::create_topology(&config.database),
    }
    .map_err(|e| format!("Failed to create database pool: {e}"))?;

    // Spawn the directory invalidation listener only at real runtime — a static
    // build must not open control-DB connections.
    let runtime_boot = hook_queue_migration_mode == RepositoryCommitHookQueueMigrationMode::Runtime;
    let shards = match resolve_shard_set(
        config,
        shard_router,
        shard_provider,
        use_directory_router,
        runtime_boot,
        topology.as_ref(),
    )
    .await
    {
        Ok(shards) => shards,
        Err(e) => {
            // The (managed) control topology is already up at this point, so a
            // later setup failure — directory control-pool sizing, shard pool
            // construction — must stop the managed Postgres child before the
            // caller's `process::exit` (which skips `on_shutdown`/`Drop`).
            // No-op when no managed cluster was started.
            #[cfg(feature = "managed-pg")]
            crate::managed_pg::emergency_stop_async().await;
            return Err(e);
        }
    };

    // Skip migrations when the provider opted out of a database (returned
    // `Ok(None)`) — even if `database.url` is configured. Custom providers
    // signal "this app runs without a DB" by returning None; running
    // migrations against the URL anyway would defeat the opt-out.
    //
    // A provider may also resolve its primary URL at runtime (managed Postgres)
    // and carry it on the topology; prefer it so migrations target the pool that
    // was actually built rather than a stale/absent configured URL.
    let provider_migration_url = topology
        .as_ref()
        .and_then(|t| t.migration_url())
        .map(str::to_owned);
    run_startup_migrations(
        config,
        topology.is_some(),
        shards.is_some(),
        provider_migration_url,
        migrations,
        directory_migration_required,
        shard_map_migration_required,
    )
    .await;

    let (replica_readiness, replica_migration_check) = if topology
        .as_ref()
        .is_some_and(|topology| check_replica_migrations && topology.replica().is_some())
    {
        match (
            config.database.effective_primary_url(),
            config.database.replica_url.as_deref(),
        ) {
            (Some(primary_url), Some(replica_url)) => {
                let primary_url = primary_url.to_owned();
                let replica_url = replica_url.to_owned();
                let readiness = crate::migrate::check_replica_migration_readiness_blocking(
                    primary_url.clone(),
                    replica_url.clone(),
                )
                .await;
                (Some(readiness), Some((primary_url, replica_url)))
            }
            _ => (None, None),
        }
    } else {
        (None, None)
    };

    if check_replica_migrations && let Some(set) = &shards {
        check_shard_replica_migration_parity(config, set).await;
    }

    // Boot-time guard: compare the current auto-split slot map against the map
    // persisted on first boot. Refuses to start if they differ, preventing
    // silent data misrouting from topology changes. Inert during static builds,
    // when no control DB is configured, and in explicit-slot mode.
    #[allow(clippy::question_mark)]
    if let Err(e) = Box::pin(enforce_shard_map_guard(
        config,
        topology.as_ref(),
        runtime_boot,
    ))
    .await
    {
        // Needs explicit `if let` (not `?`) so the managed-pg child can be stopped
        // before unwinding — `?` would skip the cfg-gated emergency stop call.
        #[cfg(feature = "managed-pg")]
        crate::managed_pg::emergency_stop_async().await;
        return Err(e);
    }

    Ok(DatabaseBootstrap {
        topology,
        shards,
        replica_readiness,
        replica_migration_check,
    })
}

/// Apply the embedded migration sets control-first, then to each shard in
/// declaration order, failing fast on the first apply error: a
/// half-migrated fleet that boots is worse than a crashed deploy, and
/// already-migrated targets are idempotently skipped on retry.
///
/// `run_pending_locked` polls with `std::thread::sleep` (up to 60 s under
/// contention), so the whole sequence runs off the Tokio worker threads in
/// one blocking task that owns the embedded migration sets.
#[cfg(feature = "db")]
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
async fn run_startup_migrations(
    config: &AutumnConfig,
    control_configured: bool,
    shards_configured: bool,
    provider_migration_url: Option<String>,
    migrations: Vec<crate::migrate::EmbeddedMigrations>,
    directory_migration_required: bool,
    shard_map_migration_required: bool,
) {
    let control_url = if control_configured {
        // Prefer a provider-resolved URL (e.g. managed Postgres, whose socket
        // URL isn't in config) carried on the topology: the runtime pool is
        // built from it, so embedded startup migrations must target it — even if
        // a stale `database.url`/`primary_url` is still configured (an existing
        // app adopting the provider). Fall back to the configured URL otherwise.
        provider_migration_url
            .or_else(|| config.database.effective_primary_url().map(str::to_owned))
    } else {
        None
    };
    let shard_targets: Vec<(String, String)> = if shards_configured {
        config
            .database
            .shards
            .iter()
            .map(|shard| (format!("shard:{}", shard.name), shard.primary_url.clone()))
            .collect()
    } else {
        Vec::new()
    };
    let profile = config.profile.clone();
    let auto_in_prod = config.database.auto_migrate_in_production;
    let migration_result = tokio::task::spawn_blocking(move || {
        if let Some(url) = control_url {
            for mig in &migrations {
                crate::migrate::auto_migrate(
                    &url,
                    profile.as_deref(),
                    auto_in_prod,
                    mig,
                    "control",
                );
            }
            // The shard directory table lives on the control plane only, so it
            // is applied here and never to the per-shard targets below.
            if directory_migration_required {
                crate::migrate::auto_migrate(
                    &url,
                    profile.as_deref(),
                    auto_in_prod,
                    &crate::sharding::SHARD_DIRECTORY_MIGRATIONS,
                    "control",
                );
            }
            // The shard-map guard table also lives on the control plane only.
            // Always allow auto-applying this framework-internal table: the guard
            // depends on it existing and returns a hard error when it's missing,
            // so skipping the migration in production would block startup.
            if shard_map_migration_required {
                crate::migrate::auto_migrate(
                    &url,
                    profile.as_deref(),
                    true,
                    &crate::sharding::SHARD_MAP_MIGRATIONS,
                    "control",
                );
            }
        }
        // Shards hold tenant data, not the control-plane schema. If the app
        // registered the full control `FRAMEWORK_MIGRATIONS` set (as some
        // examples do), skip it for shard targets — otherwise startup would
        // create the control tables on every shard and (with auto-migrate off)
        // keep reporting them as pending, even though `autumn migrate --shard`
        // applies only the shard-required framework migrations.
        for (target, url) in &shard_targets {
            for mig in migrations
                .iter()
                .filter(|mig| !migration_set_is_control_framework(mig))
            {
                crate::migrate::auto_migrate(url, profile.as_deref(), auto_in_prod, mig, target);
            }
        }
    })
    .await;
    if let Err(e) = migration_result {
        tracing::error!(error = %e, "Migration task panicked");
        // Same orphan hazard as a migration failure: `process::exit` skips
        // `on_shutdown`, so stop any managed Postgres before bailing. We are back
        // on the Tokio runtime here (after the `spawn_blocking` await), so use the
        // async stop — the sync `emergency_stop` would panic nesting a runtime.
        #[cfg(feature = "managed-pg")]
        crate::managed_pg::emergency_stop_async().await;
        std::process::exit(1);
    }
}

/// Per-shard replica migration parity feeds each shard's runtime state
/// (the analogue of `ProbeState`'s control-replica dependency), which
/// gates that shard's replica reads per its `replica_fallback`.
#[cfg(feature = "db")]
async fn check_shard_replica_migration_parity(
    config: &AutumnConfig,
    set: &crate::sharding::ShardSet,
) {
    for (shard_config, shard) in config.database.shards.iter().zip(set.iter()) {
        let Some(replica_url) = shard_config.replica_url.as_deref() else {
            continue;
        };
        // Remember the URLs so the per-shard health indicator can re-run
        // the parity comparison on later readiness probes, and claim the
        // recheck throttle slot for the check that runs right here.
        shard
            .runtime()
            .configure_migration_check(shard_config.primary_url.clone(), replica_url.to_owned());
        let _ = shard.runtime().parity_check_due();
        let readiness = crate::migrate::check_replica_migration_readiness_blocking(
            shard_config.primary_url.clone(),
            replica_url.to_owned(),
        )
        .await;
        if readiness.is_ready() {
            shard.runtime().mark_replica_migrations_ready();
        } else if let Some(detail) = readiness.detail() {
            tracing::warn!(
                shard = %shard.name(),
                detail = %detail,
                "shard replica migrations are not ready"
            );
            shard.runtime().mark_replica_migrations_unready(detail);
        }
    }
}

#[cfg(feature = "db")]
const REPOSITORY_COMMIT_HOOK_QUEUE_MIGRATION: &str =
    "20260515000000_create_repository_commit_hook_queue";

#[cfg(feature = "db")]
const VERSION_HISTORY_MIGRATION: &str = "20260526000000_create_version_history";

/// Whether startup should create the control-plane `_autumn_shard_directory`
/// table. It is required only when directory routing is enabled AND shards are
/// configured AND we are in a real runtime boot — never during a static build
/// (`autumn build`, `AUTUMN_BUILD_STATIC=1`), which renders assets and must not
/// touch the database, mirroring how the other runtime framework migrations are
/// suppressed in [`migrations_with_repository_framework_migrations`].
#[cfg(feature = "db")]
const fn directory_migration_is_required(
    directory_routing_enabled: bool,
    has_shards: bool,
    mode: RepositoryCommitHookQueueMigrationMode,
) -> bool {
    directory_routing_enabled
        && has_shards
        && matches!(mode, RepositoryCommitHookQueueMigrationMode::Runtime)
}

/// Whether startup should create the control-plane `_autumn_shard_map` table.
/// Required whenever shards are configured and we are in a real runtime boot —
/// never during a static build (`autumn build`, `AUTUMN_BUILD_STATIC=1`).
/// The guard itself is further gated to auto-split mode inside
/// `enforce_shard_map_guard`; the table is always created when shards are
/// present so an app can switch from explicit to auto-split later without a
/// manual migration.
#[cfg(feature = "db")]
const fn shard_map_migration_is_required(
    has_shards: bool,
    mode: RepositoryCommitHookQueueMigrationMode,
) -> bool {
    has_shards && matches!(mode, RepositoryCommitHookQueueMigrationMode::Runtime)
}

/// Row type for reading `_autumn_shard_map`.
#[cfg(feature = "db")]
#[derive(diesel::QueryableByName)]
struct ShardMapRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    shard_name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    slots: String,
}

/// Check and persist the shard slot map in `_autumn_shard_map`.
///
/// This is the DB-backed core of the boot-time guard: it reads existing rows,
/// delegates to the pure [`crate::config::check_stored_slot_map`] for the
/// comparison, and persists the map on first boot (no rows yet). Factored out
/// of `enforce_shard_map_guard` so integration tests can drive it directly
/// without a full `AutumnConfig`.
///
/// # Errors
///
/// Returns a `String` error when the computed auto-split map differs from the
/// stored map, indicating a topology change that would silently misroute data.
#[cfg(feature = "db")]
pub async fn run_shard_map_guard(
    control_pool: &deadpool::managed::Pool<
        diesel_async::pooled_connection::AsyncDieselConnectionManager<
            diesel_async::AsyncPgConnection,
        >,
    >,
    computed: &[crate::config::ShardSlotAssignment],
    auto_split: bool,
) -> Result<(), String> {
    use diesel_async::RunQueryDsl as _;

    if !auto_split {
        return Ok(());
    }

    let mut conn = match control_pool.get().await {
        Ok(conn) => conn,
        Err(e) => {
            return Err(format!(
                "shard-map guard could not acquire a control connection: {e} — \
                 ensure the control database is reachable to enforce topology \
                 change detection"
            ));
        }
    };

    let rows: Vec<ShardMapRow> = match diesel::sql_query(
        "SELECT shard_name, slots FROM _autumn_shard_map ORDER BY shard_name",
    )
    .load::<ShardMapRow>(&mut conn)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return Err(format!(
                "shard-map guard could not read _autumn_shard_map: {e} — \
                 run `autumn migrate` to create the control schema before \
                 starting with auto-split shards"
            ));
        }
    };

    let stored: Vec<crate::config::ShardSlotAssignment> = rows
        .into_iter()
        .map(|r| crate::config::ShardSlotAssignment {
            name: r.shard_name,
            ranges: r.slots,
        })
        .collect();
    let stored_opt = if stored.is_empty() {
        None
    } else {
        Some(stored.as_slice())
    };

    crate::config::check_stored_slot_map(auto_split, computed, stored_opt)?;

    // First boot: persist the current map so future boots can compare against it.
    // Wrapped in a transaction so a mid-loop failure leaves no partial rows —
    // partial rows would cause a spurious mismatch error on the next boot attempt.
    if stored.is_empty() {
        use diesel_async::AsyncConnection as _;
        use scoped_futures::ScopedFutureExt as _;
        let assignments: Vec<_> = computed.to_vec();
        conn.transaction::<(), diesel::result::Error, _>(move |conn| {
            async move {
                for assignment in &assignments {
                    diesel::sql_query(
                        "INSERT INTO _autumn_shard_map (shard_name, slots) VALUES ($1, $2) \
                         ON CONFLICT (shard_name) DO UPDATE \
                         SET slots = EXCLUDED.slots, updated_at = NOW()",
                    )
                    .bind::<diesel::sql_types::Text, _>(&assignment.name)
                    .bind::<diesel::sql_types::Text, _>(&assignment.ranges)
                    .execute(conn)
                    .await?;
                }
                Ok(())
            }
            .scope_boxed()
        })
        .await
        .map_err(|e| format!("shard-map guard could not persist map: {e}"))?;
    }

    Ok(())
}

/// Boot-time shard-map guard: compare the auto-split slot map against the
/// persisted map and refuse to start if they differ.
///
/// No-op when:
/// - not a runtime boot (static build),
/// - no shards configured,
/// - no control database topology, or
/// - the slot map uses explicit `slots` declarations (auto-split is inactive).
#[cfg(feature = "db")]
async fn enforce_shard_map_guard(
    config: &AutumnConfig,
    topology: Option<&crate::db::DatabaseTopology>,
    runtime_boot: bool,
) -> Result<(), String> {
    if !runtime_boot || !config.database.has_shards() {
        return Ok(());
    }
    let Some(topology) = topology else {
        return Ok(());
    };
    if !config.database.shards_auto_split() {
        return Ok(());
    }
    let computed = config
        .database
        .resolved_shard_assignments()
        .map_err(|e| format!("shard-map guard: {e}"))?;
    run_shard_map_guard(topology.primary(), &computed, true).await
}

#[cfg(feature = "db")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepositoryCommitHookQueueMigrationMode {
    Runtime,
    StaticBuild,
}

#[cfg(feature = "db")]
fn migrations_with_repository_framework_migrations(
    mut migrations: Vec<crate::migrate::EmbeddedMigrations>,
    hook_queue_required: bool,
    version_history_required: bool,
    mode: RepositoryCommitHookQueueMigrationMode,
) -> Vec<crate::migrate::EmbeddedMigrations> {
    if hook_queue_required
        && mode == RepositoryCommitHookQueueMigrationMode::Runtime
        && !shard_applied_sets_include(&migrations, REPOSITORY_COMMIT_HOOK_QUEUE_MIGRATION)
    {
        migrations.push(crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS);
    }
    if version_history_required
        && mode == RepositoryCommitHookQueueMigrationMode::Runtime
        && !shard_applied_sets_include(&migrations, VERSION_HISTORY_MIGRATION)
    {
        migrations.push(crate::version_history::VERSION_HISTORY_MIGRATIONS);
    }
    migrations
}

/// Whether `migration_name` is already present in a set that shard targets will
/// actually apply — i.e. a *non*-control-framework set.
///
/// The full control [`FRAMEWORK_MIGRATIONS`](crate::migrate::FRAMEWORK_MIGRATIONS)
/// set is deliberately excluded: `run_startup_migrations` strips it from shard
/// targets, so a migration present *only* inside it never reaches the shards. If
/// de-duplication counted it, a sharded app that registers `FRAMEWORK_MIGRATIONS`
/// (and uses commit hooks / versioning) would skip appending the standalone
/// shard-required set yet have the control set filtered out on shards — leaving
/// shards without `_autumn_repository_commit_hook_queue` / `_autumn_version_history`.
/// Matching only shard-applied sets ensures the standalone set is appended
/// whenever the shards would otherwise be missing it. Re-applying it to the
/// control target is harmless: it shares the migration version already recorded
/// by the control framework set, so Diesel skips it there.
#[cfg(feature = "db")]
fn shard_applied_sets_include(
    migrations: &[crate::migrate::EmbeddedMigrations],
    migration_name: &str,
) -> bool {
    use diesel::migration::{Migration, MigrationSource as _};
    use diesel::pg::Pg;

    migrations
        .iter()
        .filter(|set| !migration_set_is_control_framework(set))
        .any(|source| {
            let Ok(source_migrations): Result<Vec<Box<dyn Migration<Pg>>>, _> = source.migrations()
            else {
                return false;
            };

            source_migrations
                .iter()
                .any(|migration| migration.name().to_string() == migration_name)
        })
}

/// Whether a migration set is the control-plane
/// [`FRAMEWORK_MIGRATIONS`](crate::migrate::FRAMEWORK_MIGRATIONS), so it can be
/// skipped on shard targets.
///
/// Identified by containing a *control-only* migration — one in
/// `FRAMEWORK_MIGRATIONS` but not in the shard-required version-history /
/// commit-hook sets. Those two sets' migrations are duplicated into the control
/// `migrations/` directory, so a plain name overlap would also (wrongly) match
/// the standalone `VERSION_HISTORY_MIGRATIONS` / `REPOSITORY_COMMIT_HOOK_MIGRATIONS`
/// sets and strip them from shards.
#[cfg(feature = "db")]
fn migration_set_is_control_framework(set: &crate::migrate::EmbeddedMigrations) -> bool {
    use diesel::migration::{Migration, MigrationSource as _};
    use diesel::pg::Pg;

    fn names(set: &crate::migrate::EmbeddedMigrations) -> std::collections::HashSet<String> {
        let migrations: Vec<Box<dyn Migration<Pg>>> = set.migrations().unwrap_or_default();
        migrations.iter().map(|m| m.name().to_string()).collect()
    }

    let mut control_only = names(&crate::migrate::FRAMEWORK_MIGRATIONS);
    for shard_required in [
        &crate::version_history::VERSION_HISTORY_MIGRATIONS,
        &crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS,
    ] {
        for name in names(shard_required) {
            control_only.remove(&name);
        }
    }

    names(set).iter().any(|name| control_only.contains(name))
}

#[cfg(feature = "db")]
fn apply_replica_migration_readiness(
    state: &AppState,
    readiness: Option<crate::migrate::ReplicaMigrationReadiness>,
) {
    let Some(readiness) = readiness else {
        return;
    };

    if readiness.is_ready() {
        state.probes().mark_replica_migrations_ready();
    } else if let Some(detail) = readiness.detail() {
        state.probes().mark_replica_migrations_unready(detail);
    }
}

#[cfg(feature = "db")]
fn configure_replica_migration_check(state: &AppState, check: Option<(String, String)>) {
    let Some((primary_url, replica_url)) = check else {
        return;
    };

    state
        .probes()
        .configure_replica_migration_check(primary_url, replica_url);
}

/// Refuse to start when a `#[repository(api = ...)]`-mounted route
/// has no paired `policy = ...` argument in `prod` profile builds.
///
/// The issue text spells out the rationale: silently shipping
/// auto-generated CRUD endpoints with no record-level authz is a
/// security regression. The escape hatch is
/// `[security] allow_unauthorized_repository_api = true`.
/// Pure offender-collection logic for
/// [`validate_repository_api_policies`].
///
/// Walks both top-level routes and routes registered under
/// `.scoped(prefix, layer, routes)` groups, returning every
/// `#[repository(api = ...)]`-mounted *mutating* route that has no
/// paired `policy = ...` argument. Read-only mounts (GET
/// `*_api_list` / `*_api_get`) are intentionally excluded — they
/// don't fit the "any authenticated user can write to any record"
/// footgun the issue calls out. Read-leak concerns are handled
/// separately by `scope = ...`.
///
/// Returned in (resource type name, api path) form, deduped per
/// `(type, path)` pair so a repository with multiple unguarded
/// methods only shows up once.
fn collect_unguarded_repository_writes(
    routes: &[Route],
    scoped_groups: &[ScopedGroup],
) -> Vec<(String, String)> {
    let mut offenders: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<(&'static str, &'static str)> =
        std::collections::HashSet::new();
    let mut record_route = |route: &Route| {
        if let Some(meta) = route.repository
            && !meta.has_policy
            && is_mutating_method(&route.method)
            && seen.insert((meta.resource_type_name, meta.api_path))
        {
            offenders.push((meta.resource_type_name.to_owned(), meta.api_path.to_owned()));
        }
    };
    for route in routes {
        record_route(route);
    }
    for group in scoped_groups {
        for route in &group.routes {
            record_route(route);
        }
    }
    offenders
}

/// Format a list of `(type, path)` offenders into the bulleted
/// listing the startup tracing emits. Pure so the format string
/// can be unit-tested without going through `tracing` machinery.
fn format_unguarded_repository_listing(offenders: &[(String, String)]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let mut first = true;
    for (name, path) in offenders {
        if !first {
            s.push('\n');
        }
        first = false;
        write!(s, "  - #[repository({name}, api = \"{path}\")]").unwrap();
    }
    s
}

fn validate_repository_api_policies(
    routes: &[Route],
    scoped_groups: &[ScopedGroup],
    config: &AutumnConfig,
) {
    let profile = config.profile.as_deref().unwrap_or("default");
    let strict =
        is_production_profile(profile) && !config.security.allow_unauthorized_repository_api;

    let offenders = collect_unguarded_repository_writes(routes, scoped_groups);
    if offenders.is_empty() {
        return;
    }

    let listing = format_unguarded_repository_listing(&offenders);

    if strict {
        tracing::error!(
            "refusing to start: the following #[repository(api = ...)] mutating endpoints have no paired `policy = ...` argument:\n{listing}\n\
             Add `policy = SomePolicy` to each, or set `[security] allow_unauthorized_repository_api = true` to opt out explicitly."
        );
        std::process::exit(1);
    } else {
        tracing::warn!(
            "the following #[repository(api = ...)] mutating endpoints have no paired `policy = ...` argument; \
             auto-generated POST/PUT/PATCH/DELETE handlers will accept writes from any authenticated user:\n{listing}\n\
             This will become a startup-time error in `prod` profile builds."
        );
    }
}

/// Refuse to start when a `#[repository(policy = X)]`-annotated
/// route exists but the corresponding `.policy::<R, _>(X)`
/// registration was never actually applied to the live
/// [`PolicyRegistry`](crate::authorization::PolicyRegistry).
///
/// `validate_repository_api_policies` runs *before* the registry is
/// populated and only checks the macro-set `has_policy` flag. This
/// runs *after* registrations are applied and walks the same routes,
/// invoking the macro-emitted `policy_check` probe to confirm the
/// policy is really there. Without this, forgetting the
/// `.policy::<R, _>(...)` builder call would compile, boot, and
/// then 500 on every protected request.
/// `(resource_type_name, api_path)` pair identifying a repository
/// route that's missing its required runtime registration.
type MissingRepositoryRegistration = (String, String);

/// Pure offender-collection logic for
/// [`validate_repository_policies_registered`].
///
/// Walks the same routes + scoped groups and invokes the macro-
/// emitted `policy_check` / `scope_check` probes against the live
/// registry, returning `(missing_policies, missing_scopes)` deduped
/// per `(type, path)` pair. Pure so the listing logic can be unit-
/// tested without going through the actual `tracing::error!` +
/// `std::process::exit(1)` strict path.
fn collect_unregistered_repository_handlers(
    routes: &[Route],
    scoped_groups: &[ScopedGroup],
    registry: &crate::authorization::PolicyRegistry,
) -> (
    Vec<MissingRepositoryRegistration>,
    Vec<MissingRepositoryRegistration>,
) {
    let mut missing_policies: Vec<(String, String)> = Vec::new();
    let mut missing_scopes: Vec<(String, String)> = Vec::new();
    let mut seen_policies: std::collections::HashSet<(&'static str, &'static str)> =
        std::collections::HashSet::new();
    let mut seen_scopes: std::collections::HashSet<(&'static str, &'static str)> =
        std::collections::HashSet::new();
    let mut record_route = |route: &Route| {
        if let Some(meta) = route.repository {
            if let Some(check) = meta.policy_check
                && !check(registry)
                && seen_policies.insert((meta.resource_type_name, meta.api_path))
            {
                missing_policies
                    .push((meta.resource_type_name.to_owned(), meta.api_path.to_owned()));
            }
            if let Some(check) = meta.scope_check
                && !check(registry)
                && seen_scopes.insert((meta.resource_type_name, meta.api_path))
            {
                missing_scopes.push((meta.resource_type_name.to_owned(), meta.api_path.to_owned()));
            }
        }
    };
    for route in routes {
        record_route(route);
    }
    for group in scoped_groups {
        for route in &group.routes {
            record_route(route);
        }
    }
    (missing_policies, missing_scopes)
}

/// Format a `(type, path)` listing for missing-policy startup
/// errors. Pure so the format string can be unit-tested.
fn format_missing_policy_listing(missing: &[(String, String)]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let mut first = true;
    for (name, path) in missing {
        if !first {
            s.push('\n');
        }
        first = false;
        write!(s, "  - #[repository({name}, api = \"{path}\", policy = ...)]: call `.policy::<{name}, _>(...)` on the app builder").unwrap();
    }
    s
}

/// Format a `(type, path)` listing for missing-scope startup
/// errors. Pure so the format string can be unit-tested.
fn format_missing_scope_listing(missing: &[(String, String)]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let mut first = true;
    for (name, path) in missing {
        if !first {
            s.push('\n');
        }
        first = false;
        write!(s, "  - #[repository({name}, api = \"{path}\", scope = ...)]: call `.scope::<{name}, _>(...)` on the app builder").unwrap();
    }
    s
}

#[allow(clippy::cognitive_complexity)]
fn validate_repository_policies_registered(
    routes: &[Route],
    scoped_groups: &[ScopedGroup],
    state: &AppState,
    config: &AutumnConfig,
) {
    let profile = config.profile.as_deref().unwrap_or("default");
    let strict = is_production_profile(profile);

    let (missing_policies, missing_scopes) =
        collect_unregistered_repository_handlers(routes, scoped_groups, state.policy_registry());

    if missing_policies.is_empty() && missing_scopes.is_empty() {
        return;
    }

    if !missing_policies.is_empty() {
        let listing = format_missing_policy_listing(&missing_policies);

        if strict {
            tracing::error!(
                "refusing to start: the following #[repository] routes declare a `policy = ...` argument, but no policy is registered for the resource type. Without registration, every protected request would fail at runtime with `500 no policy registered`:\n{listing}"
            );
        } else {
            tracing::warn!(
                "the following #[repository] routes declare `policy = ...` but no matching `.policy::<R, _>(...)` registration is on the app builder. Protected requests will 500 at runtime:\n{listing}\n\
                 This will become a startup-time error in `prod` profile builds."
            );
        }
    }

    if !missing_scopes.is_empty() {
        let listing = format_missing_scope_listing(&missing_scopes);

        if strict {
            tracing::error!(
                "refusing to start: the following #[repository] routes declare a `scope = ...` argument, but no scope is registered for the resource type. Without registration, every list request would fail at runtime with `500 missing scope registration`:\n{listing}"
            );
        } else {
            tracing::warn!(
                "the following #[repository] routes declare `scope = ...` but no matching `.scope::<R, _>(...)` registration is on the app builder. List requests will 500 at runtime:\n{listing}\n\
                 This will become a startup-time error in `prod` profile builds."
            );
        }
    }

    if strict {
        std::process::exit(1);
    }
}

const fn is_mutating_method(method: &http::Method) -> bool {
    matches!(
        *method,
        http::Method::POST | http::Method::PUT | http::Method::PATCH | http::Method::DELETE
    )
}

/// Returns `true` for the framework's accepted production profile
/// names. Mirrors the `prod | production` matching used elsewhere
/// (`app.rs::run_build_mode`, `migrate.rs::should_auto_apply`,
/// etc.) so the repository startup guards don't silently weaken in
/// deployments that pick the long-form alias.
fn is_production_profile(profile: &str) -> bool {
    matches!(profile, "prod" | "production")
}

#[cfg(test)]
mod validate_repository_api_policies_tests {
    use super::*;
    use crate::RepositoryApiMeta;

    fn build_route(
        method: http::Method,
        path: &'static str,
        meta: Option<RepositoryApiMeta>,
    ) -> Route {
        Route {
            method,
            path,
            handler: axum::routing::any(|| async { "" }),
            name: "test_route",
            api_doc: crate::openapi::ApiDoc::default(),
            repository: meta,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            api_version: None,
            sunset_opt_out: false,
        }
    }

    fn unguarded(path: &'static str, type_name: &'static str) -> RepositoryApiMeta {
        RepositoryApiMeta {
            resource_type_name: type_name,
            api_path: path,
            has_policy: false,
            policy_check: None,
            scope_check: None,
        }
    }

    /// Tests in this module historically used a duplicated copy of
    /// the offender-collection logic. Now they call the production
    /// helper directly so coverage tracks the real code path.
    fn collect_offenders(routes: &[Route]) -> Vec<(String, String)> {
        collect_unguarded_repository_writes(routes, &[])
    }

    #[test]
    fn read_only_mount_without_policy_is_not_an_offender() {
        let routes = vec![
            build_route(
                http::Method::GET,
                "/api/posts",
                Some(unguarded("/api/posts", "Post")),
            ),
            build_route(
                http::Method::GET,
                "/api/posts/{id}",
                Some(unguarded("/api/posts", "Post")),
            ),
        ];
        let offenders = collect_offenders(&routes);
        assert!(
            offenders.is_empty(),
            "read-only mounts should not trigger the unauthorized-repo guard"
        );
    }

    #[test]
    fn write_mount_without_policy_is_an_offender() {
        let routes = vec![build_route(
            http::Method::POST,
            "/api/posts",
            Some(unguarded("/api/posts", "Post")),
        )];
        let offenders = collect_offenders(&routes);
        assert_eq!(offenders.len(), 1);
        assert_eq!(offenders[0].0, "Post");
        assert_eq!(offenders[0].1, "/api/posts");
    }

    #[test]
    fn mixed_mount_only_dedups_one_offender_per_repository() {
        let routes = vec![
            build_route(
                http::Method::GET,
                "/api/posts",
                Some(unguarded("/api/posts", "Post")),
            ),
            build_route(
                http::Method::POST,
                "/api/posts",
                Some(unguarded("/api/posts", "Post")),
            ),
            build_route(
                http::Method::PUT,
                "/api/posts/{id}",
                Some(unguarded("/api/posts", "Post")),
            ),
            build_route(
                http::Method::DELETE,
                "/api/posts/{id}",
                Some(unguarded("/api/posts", "Post")),
            ),
        ];
        let offenders = collect_offenders(&routes);
        assert_eq!(offenders.len(), 1);
    }

    #[test]
    fn is_mutating_method_classifies_methods() {
        assert!(is_mutating_method(&http::Method::POST));
        assert!(is_mutating_method(&http::Method::PUT));
        assert!(is_mutating_method(&http::Method::PATCH));
        assert!(is_mutating_method(&http::Method::DELETE));
        assert!(!is_mutating_method(&http::Method::GET));
        assert!(!is_mutating_method(&http::Method::HEAD));
        assert!(!is_mutating_method(&http::Method::OPTIONS));
    }

    // ── registry-aware validation (post-registration) ─────────────

    use crate::authorization::{Policy, PolicyRegistry};

    #[derive(Debug, Clone, PartialEq)]
    struct TestPost;

    #[derive(Default)]
    struct TestPostPolicy;
    impl Policy<TestPost> for TestPostPolicy {}

    fn guarded_with_check(path: &'static str, type_name: &'static str) -> RepositoryApiMeta {
        RepositoryApiMeta {
            resource_type_name: type_name,
            api_path: path,
            has_policy: true,
            policy_check: Some(|registry: &PolicyRegistry| registry.has_policy::<TestPost>()),
            scope_check: None,
        }
    }

    fn collect_missing(routes: &[Route], registry: &PolicyRegistry) -> Vec<(String, String)> {
        let (missing_policies, _) = collect_unregistered_repository_handlers(routes, &[], registry);
        missing_policies
    }

    #[test]
    fn registry_check_flags_routes_missing_their_policy_registration() {
        // Macro emits `policy = X` but no `.policy::<TestPost, _>(...)`
        // call on the builder — registry has nothing.
        let registry = PolicyRegistry::default();
        let routes = vec![build_route(
            http::Method::POST,
            "/api/posts",
            Some(guarded_with_check("/api/posts", "TestPost")),
        )];
        let missing = collect_missing(&routes, &registry);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, "TestPost");
        assert_eq!(missing[0].1, "/api/posts");
    }

    #[test]
    fn registry_check_passes_when_policy_is_registered() {
        let registry = PolicyRegistry::default();
        registry.register_policy::<TestPost, _>(TestPostPolicy);
        let routes = vec![build_route(
            http::Method::POST,
            "/api/posts",
            Some(guarded_with_check("/api/posts", "TestPost")),
        )];
        let missing = collect_missing(&routes, &registry);
        assert!(missing.is_empty(), "policy is registered, no offenders");
    }

    #[test]
    fn registry_check_skips_routes_without_policy_check_fn() {
        // Routes mounted without `policy = ...` carry
        // `policy_check: None` and are not subject to this check —
        // they're handled by `validate_repository_api_policies` which
        // looks at `has_policy` instead.
        let registry = PolicyRegistry::default();
        let routes = vec![build_route(
            http::Method::POST,
            "/api/posts",
            Some(unguarded("/api/posts", "TestPost")),
        )];
        let missing = collect_missing(&routes, &registry);
        assert!(missing.is_empty());
    }

    #[test]
    fn registry_check_dedups_one_offender_per_repository() {
        let registry = PolicyRegistry::default();
        let routes = vec![
            build_route(
                http::Method::GET,
                "/api/posts",
                Some(guarded_with_check("/api/posts", "TestPost")),
            ),
            build_route(
                http::Method::POST,
                "/api/posts",
                Some(guarded_with_check("/api/posts", "TestPost")),
            ),
            build_route(
                http::Method::DELETE,
                "/api/posts/{id}",
                Some(guarded_with_check("/api/posts", "TestPost")),
            ),
        ];
        let missing = collect_missing(&routes, &registry);
        assert_eq!(missing.len(), 1);
    }

    // ── Scope registration validation ─────────────────────────────

    use crate::authorization::{BoxFuture, PolicyContext, Scope};

    #[derive(Default)]
    struct TestPostScope;
    impl Scope<TestPost> for TestPostScope {
        fn list<'a>(
            &'a self,
            _ctx: &'a PolicyContext,
            _conn: &'a mut diesel_async::AsyncPgConnection,
        ) -> BoxFuture<'a, crate::AutumnResult<Vec<TestPost>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn scope_only_meta(path: &'static str, type_name: &'static str) -> RepositoryApiMeta {
        RepositoryApiMeta {
            resource_type_name: type_name,
            api_path: path,
            has_policy: false,
            policy_check: None,
            scope_check: Some(|registry: &PolicyRegistry| registry.scope::<TestPost>().is_some()),
        }
    }

    fn collect_missing_scopes(
        routes: &[Route],
        registry: &PolicyRegistry,
    ) -> Vec<(String, String)> {
        let (_, missing_scopes) = collect_unregistered_repository_handlers(routes, &[], registry);
        missing_scopes
    }

    #[test]
    fn scope_check_flags_unregistered_scope() {
        let registry = PolicyRegistry::default();
        let routes = vec![build_route(
            http::Method::GET,
            "/api/posts",
            Some(scope_only_meta("/api/posts", "TestPost")),
        )];
        let missing = collect_missing_scopes(&routes, &registry);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, "TestPost");
    }

    #[test]
    fn scope_check_passes_when_scope_is_registered() {
        let registry = PolicyRegistry::default();
        registry.register_scope::<TestPost, _>(TestPostScope);
        let routes = vec![build_route(
            http::Method::GET,
            "/api/posts",
            Some(scope_only_meta("/api/posts", "TestPost")),
        )];
        let missing = collect_missing_scopes(&routes, &registry);
        assert!(missing.is_empty());
    }

    #[test]
    fn scope_check_skips_routes_without_scope_check_fn() {
        let registry = PolicyRegistry::default();
        let routes = vec![build_route(
            http::Method::POST,
            "/api/posts",
            Some(unguarded("/api/posts", "TestPost")),
        )];
        let missing = collect_missing_scopes(&routes, &registry);
        assert!(missing.is_empty());
    }

    // ── prod / production profile parity ────────────────────────

    #[test]
    fn is_production_profile_matches_both_aliases() {
        assert!(is_production_profile("prod"));
        assert!(is_production_profile("production"));
        assert!(!is_production_profile("dev"));
        assert!(!is_production_profile("staging"));
        assert!(!is_production_profile("test"));
        assert!(!is_production_profile("default"));
        // Case-sensitive (matches the framework's elsewhere
        // matching pattern in app.rs::run_build_mode and
        // migrate.rs).
        assert!(!is_production_profile("Prod"));
        assert!(!is_production_profile("Production"));
    }

    // ── Formatter helpers ─────────────────────────────────────────

    #[test]
    fn format_unguarded_listing_renders_one_bullet_per_offender() {
        let offenders = vec![
            ("Post".to_owned(), "/api/posts".to_owned()),
            ("Comment".to_owned(), "/api/comments".to_owned()),
        ];
        let listing = format_unguarded_repository_listing(&offenders);
        assert!(listing.contains("Post"));
        assert!(listing.contains("/api/posts"));
        assert!(listing.contains("Comment"));
        assert!(listing.contains("/api/comments"));
        assert_eq!(listing.matches("\n  - ").count() + 1, 2);
    }

    #[test]
    fn format_unguarded_listing_empty_input_yields_empty_string() {
        let listing = format_unguarded_repository_listing(&[]);
        assert!(listing.is_empty());
    }

    #[test]
    fn format_missing_policy_listing_includes_policy_call_hint() {
        let missing = vec![("Post".to_owned(), "/api/posts".to_owned())];
        let listing = format_missing_policy_listing(&missing);
        assert!(listing.contains("Post"));
        assert!(listing.contains("/api/posts"));
        assert!(listing.contains(".policy::<Post, _>"));
        assert!(listing.contains("policy = ..."));
    }

    #[test]
    fn format_missing_scope_listing_includes_scope_call_hint() {
        let missing = vec![("Post".to_owned(), "/api/posts".to_owned())];
        let listing = format_missing_scope_listing(&missing);
        assert!(listing.contains("Post"));
        assert!(listing.contains("/api/posts"));
        assert!(listing.contains(".scope::<Post, _>"));
        assert!(listing.contains("scope = ..."));
    }

    // ── Scoped-groups path coverage ──────────────────────────────

    #[test]
    fn collect_unguarded_walks_scoped_groups() {
        // The scoped-group path catches `#[repository(api = ...)]`
        // mounts that live inside `.scoped(prefix, layer, routes)`.
        // Without walking them, the prod-mode guard would silently
        // miss those routes.
        let group_route = build_route(
            http::Method::POST,
            "/api/posts",
            Some(unguarded("/api/posts", "Post")),
        );
        let group = ScopedGroup {
            prefix: "/scoped".to_owned(),
            routes: vec![group_route],
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(|r| r),
        };
        let offenders = collect_unguarded_repository_writes(&[], std::slice::from_ref(&group));
        assert_eq!(offenders.len(), 1);
        assert_eq!(offenders[0].0, "Post");
    }

    #[test]
    fn collect_unregistered_walks_scoped_groups() {
        let group_route = build_route(
            http::Method::POST,
            "/api/posts",
            Some(guarded_with_check("/api/posts", "TestPost")),
        );
        let group = ScopedGroup {
            prefix: "/scoped".to_owned(),
            routes: vec![group_route],
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(|r| r),
        };
        let registry = PolicyRegistry::default();
        let (missing, _) =
            collect_unregistered_repository_handlers(&[], std::slice::from_ref(&group), &registry);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, "TestPost");
    }
}

fn build_state(
    config: &AutumnConfig,
    #[cfg(feature = "db")] database_topology: Option<&crate::db::DatabaseTopology>,
    #[cfg(feature = "db")] shards: Option<crate::sharding::ShardSet>,
    #[cfg(feature = "ws")] channels_backend: Option<Arc<dyn crate::channels::ChannelsBackend>>,
) -> AppState {
    #[cfg(feature = "ws")]
    let shutdown = tokio_util::sync::CancellationToken::new();
    #[cfg(feature = "ws")]
    let channels = channels_backend.map_or_else(
        || {
            crate::channels::Channels::from_config(&config.channels, shutdown.child_token())
                .unwrap_or_else(|error| {
                    tracing::error!(error = %error, "Failed to configure channels backend");
                    std::process::exit(1);
                })
        },
        crate::channels::Channels::with_shared_backend,
    );

    let state = AppState {
        extensions: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        #[cfg(feature = "db")]
        pool: database_topology.map(|topology| topology.primary().clone()),
        #[cfg(feature = "db")]
        replica_pool: database_topology.and_then(|topology| topology.replica().cloned()),
        #[cfg(feature = "db")]
        shards,
        profile: config.profile.clone(),
        started_at: std::time::Instant::now(),
        health_detailed: config.health.detailed,
        probes: crate::probe::ProbeState::pending_startup(),
        metrics: crate::middleware::MetricsCollector::new(),
        log_levels: crate::actuator::LogLevels::new(&config.log.level),
        task_registry: crate::actuator::TaskRegistry::new(),
        job_registry: crate::actuator::JobRegistry::new(),
        config_props: crate::actuator::ConfigProperties::from_config(config),
        metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
        health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
        #[cfg(feature = "presence")]
        presence: crate::presence::Presence::new(channels.clone()),
        #[cfg(feature = "ws")]
        channels,
        #[cfg(feature = "ws")]
        shutdown,
        policy_registry: crate::authorization::PolicyRegistry::default(),
        forbidden_response: config.security.forbidden_response,
        auth_session_key: config.auth.session_key.clone(),
        shared_cache: None,
        clock: std::sync::Arc::new(crate::time::SystemClock),
    };
    #[cfg(feature = "db")]
    if state.replica_pool.is_some() {
        state
            .probes()
            .configure_replica_dependency(config.database.replica_fallback);
    }
    // Surface every shard in /ready and /actuator/health as a
    // `db:shard:<name>` component (replica readiness refresh + pool stats).
    #[cfg(feature = "db")]
    if let Some(set) = state.shards() {
        crate::sharding::register_shard_health_indicators(set, &state.health_indicator_registry);
    }
    state.insert_extension(config.clone());
    state.insert_extension(crate::step_up::StepUpGlobalConfig {
        default_max_age_secs: config.auth.step_up.default_max_age_secs,
    });
    #[cfg(feature = "http-client")]
    state.insert_extension(crate::http_client::SharedReqwestClient {
        client: crate::http_client::Client::build_inner(&config.http.client),
        timeout_secs: config.http.client.timeout_secs,
    });
    state
}

/// Build the route listing string for the transparency log.
fn format_route_lines(
    routes: &[Route],
    scoped_groups: &[ScopedGroup],
    config: &AutumnConfig,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for route in routes {
        let _ = write!(
            out,
            "\n    {} {:<8} -> {}",
            route.path, route.method, route.name
        );
    }
    for group in scoped_groups {
        for route in &group.routes {
            let _ = write!(
                out,
                "\n    {}{} {:<8} -> {} (scoped)",
                group.prefix, route.path, route.method, route.name
            );
        }
    }
    let mut probe_paths = std::collections::HashSet::new();
    for (path, name) in [
        (config.health.live_path.as_str(), "live"),
        (config.health.ready_path.as_str(), "ready"),
        (config.health.startup_path.as_str(), "startup"),
        (config.health.path.as_str(), "health"),
    ] {
        if probe_paths.insert(path) {
            let _ = write!(out, "\n    {} {:<8} -> {}", path, "GET", name);
        }
    }
    let _ = write!(
        out,
        "\n    {} {:<8} -> actuator",
        crate::actuator::actuator_route_glob(&config.actuator.prefix),
        "GET"
    );
    #[cfg(feature = "htmx")]
    {
        out.push_str("\n    /static/js/htmx.min.js GET -> htmx");
        out.push_str("\n    /static/js/autumn-htmx-csrf.js GET -> htmx csrf");
    }
    out
}

/// Build the scheduled task listing string. Returns `None` if there are no tasks.
fn format_task_lines(tasks: &[crate::task::TaskInfo]) -> Option<String> {
    use std::fmt::Write as _;

    if tasks.is_empty() {
        return None;
    }

    let mut out = String::new();
    for task in tasks {
        let schedule = task.schedule.to_string();
        let _ = write!(out, "\n    {} ({schedule})", task.name);
    }
    Some(out)
}

/// Build the active middleware listing string.
fn format_middleware_list(config: &AutumnConfig) -> String {
    let mut items = vec![
        "RequestId",
        "SecurityHeaders",
        "Session (in-memory)",
        "ErrorPages",
    ];
    if !config.cors.allowed_origins.is_empty() {
        items.push("CORS");
    }
    if config.security.csrf.enabled {
        items.push("CSRF");
    }
    items.push("Metrics");
    items.join(", ")
}

/// Mask a database URL password for safe logging.
fn mask_database_url(url: &str, pool_size: usize) -> String {
    if let Ok(mut parsed_url) = url::Url::parse(url) {
        if parsed_url.password().is_some() {
            let _ = parsed_url.set_password(Some("****"));
            return format!("{parsed_url} (pool_size={pool_size})");
        }
        format!("{parsed_url} (pool_size={pool_size})")
    } else {
        // Fallback: If URL parsing fails, mask the entire URL string to prevent any
        // potential data exposure (e.g. if the malformed string still contained a password)
        format!("**** (pool_size={pool_size})")
    }
}

/// Build the configuration summary string.
fn format_config_summary(config: &AutumnConfig) -> String {
    let profile = config.profile.as_deref().unwrap_or("none");
    let db_status = config.database.effective_primary_url().map_or_else(
        || "not configured".to_owned(),
        |url| {
            let primary = mask_database_url(url, config.database.effective_primary_pool_size());
            if config.database.replica_url.is_some() {
                format!(
                    "primary={primary}, replica=configured (pool_size={})",
                    config.database.effective_replica_pool_size()
                )
            } else {
                primary
            }
        },
    );
    let telemetry_status = if config.telemetry.enabled {
        let endpoint = config
            .telemetry
            .otlp_endpoint
            .as_deref()
            .unwrap_or("<missing endpoint>");
        format!("{:?} -> {endpoint}", config.telemetry.protocol)
    } else {
        "disabled".to_owned()
    };
    format!(
        "\
        \n    profile:    {profile}\
        \n    server:     {}:{}\
        \n    database:   {db_status}\
        \n    log_level:  {}\
        \n    log_format: {:?}\
        \n    telemetry:  {telemetry_status}\
        \n    health:     {} (detailed={})\
        \n    actuator:   sensitive={}\
        \n    shutdown:   prestop={}s drain={}s",
        config.server.host,
        config.server.port,
        config.log.level,
        config.log.format,
        config.health.path,
        config.health.detailed,
        config.actuator.sensitive,
        config.server.prestop_grace_secs,
        config.server.shutdown_timeout_secs,
    )
}

/// Resolve a project-relative subdirectory (e.g. `"dist"` or `"static"`)
/// against `AUTUMN_MANIFEST_DIR` if set, otherwise use it as-is.
pub(crate) fn project_dir(subdir: &str, env: &dyn crate::config::Env) -> std::path::PathBuf {
    env.var("AUTUMN_MANIFEST_DIR").map_or_else(
        |_| std::path::PathBuf::from(subdir),
        |d| std::path::PathBuf::from(d).join(subdir),
    )
}

/// Wait for a shutdown signal (Ctrl+C, SIGTERM on Unix, or a canary rollback
/// flag file written by a controller).
///
/// Returns when any signal is received. Axum's `with_graceful_shutdown`
/// then stops accepting new connections and drains in-flight requests.
///
/// The canary rollback arm lets a progressive-delivery controller drain and
/// retire a bad canary replica without sending `SIGTERM` by hand: it writes
/// [`crate::canary::CANARY_ROLLBACK_FLAG_FILE`] and Autumn runs the identical
/// graceful-shutdown sequence (ready → 503, prestop grace, drain, clean exit).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
        tracing::info!("Received Ctrl+C, starting graceful shutdown");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
        tracing::info!("Received SIGTERM, starting graceful shutdown");
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let canary_rollback = async {
        canary_rollback_signal(std::path::Path::new(
            crate::canary::CANARY_ROLLBACK_FLAG_FILE,
        ))
        .await;
        tracing::info!("Canary rollback signalled, starting graceful shutdown");
    };

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
        () = canary_rollback => {},
    }
}

/// Resolve when the canary rollback flag file is present at `path`.
///
/// A rollback signal is intentionally **sticky across restarts**: if the flag is
/// already present at boot (e.g. a supervisor restarted the process after a
/// rollback), this resolves immediately so the replica drains and exits again
/// rather than rejoining the canary cohort. The replica keeps draining until a
/// controller clears the signal with `autumn canary promote` (or scales the
/// replica to zero). At startup the framework also flips `/ready` to draining
/// when the flag is present, so a restarted rolled-back replica never serves
/// canary traffic.
///
/// Uses async stat so the 500 ms poll never blocks the executor thread.
async fn canary_rollback_signal(path: &std::path::Path) {
    let interval = std::time::Duration::from_millis(500);
    loop {
        if tokio::fs::metadata(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[cfg(feature = "db")]
    const APP_TEST_MIGRATIONS: crate::migrate::EmbeddedMigrations =
        diesel_migrations::embed_migrations!("test_migrations");

    /// Shared no-op `MailDeliveryQueue` used by builder tests so the trait
    /// impl body is defined once and exercised by at least one test.
    #[cfg(feature = "mail")]
    struct MailTestNoopQueue;

    #[cfg(feature = "mail")]
    impl crate::mail::MailDeliveryQueue for MailTestNoopQueue {
        fn enqueue<'a>(
            &'a self,
            _mail: crate::mail::Mail,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), crate::mail::MailError>> + Send + 'a>,
        > {
            Box::pin(async { Ok(()) })
        }
    }

    #[cfg(feature = "mail")]
    fn test_mail() -> crate::mail::Mail {
        crate::mail::Mail::builder()
            .to("test@example.com")
            .subject("hi")
            .text("hello")
            .build()
            .expect("test mail should build")
    }

    /// Helper to build a test router with default config and no database.
    pub fn test_router(routes: Vec<Route>) -> axum::Router {
        let config = AutumnConfig::default();
        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        crate::router::build_router(routes, &config, state)
    }

    #[tokio::test]
    async fn canary_rollback_signal_resolves_when_flag_newly_written() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("canary-rollback.json");

        // Flag is absent at boot; writing it after start must resolve the signal.
        let writer_path = path.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            crate::canary::CanaryState::write_rollback_flag(
                &writer_path,
                &crate::canary::RollbackSignal::default(),
            )
            .unwrap();
        });

        let signalled = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            canary_rollback_signal(&path),
        )
        .await;
        assert!(signalled.is_ok(), "rollback signal should resolve");
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn canary_rollback_signal_resolves_immediately_when_flag_present_at_boot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("canary-rollback.json");
        // A rollback flag is sticky across restarts: present at boot must trigger
        // again so a supervisor restart cannot rejoin a rolled-back replica.
        crate::canary::CanaryState::write_rollback_flag(
            &path,
            &crate::canary::RollbackSignal::default(),
        )
        .unwrap();

        let signalled = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            canary_rollback_signal(&path),
        )
        .await;
        assert!(
            signalled.is_ok(),
            "a flag present at boot must trigger rollback (sticky across restarts)"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn build_state_applies_replica_fallback_policy_to_read_routing() {
        let mut config = AutumnConfig::default();
        config.database.primary_url = Some("postgres://localhost/primary".to_owned());
        config.database.primary_pool_size = Some(5);
        config.database.replica_url = Some("postgres://localhost/replica".to_owned());
        config.database.replica_pool_size = Some(2);
        config.database.replica_fallback = crate::config::ReplicaFallback::Primary;
        let topology = crate::db::create_topology(&config.database)
            .expect("topology should build")
            .expect("database should be configured");

        let state = build_state(
            &config,
            Some(&topology),
            None,
            #[cfg(feature = "ws")]
            None,
        );
        state
            .probes()
            .mark_replica_unready("replica migrations lag primary");

        assert_eq!(state.read_pool().expect("read pool").status().max_size, 5);
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn custom_pool_provider_preserves_configured_replica_topology() {
        struct PassthroughPoolProvider;

        impl crate::db::DatabasePoolProvider for PassthroughPoolProvider {
            async fn create_pool(
                &self,
                config: &crate::config::DatabaseConfig,
            ) -> Result<
                Option<
                    diesel_async::pooled_connection::deadpool::Pool<
                        diesel_async::AsyncPgConnection,
                    >,
                >,
                crate::db::PoolError,
            > {
                crate::db::create_pool(config)
            }
        }

        let mut config = AutumnConfig::default();
        config.database.primary_url = Some("postgres://localhost/primary".to_owned());
        config.database.primary_pool_size = Some(5);
        config.database.replica_url = Some("postgres://localhost/replica".to_owned());
        config.database.replica_pool_size = Some(2);
        config.database.replica_fallback = crate::config::ReplicaFallback::FailReadiness;
        let AppBuilder {
            pool_provider_factory,
            ..
        } = app().with_pool_provider(PassthroughPoolProvider);

        let database = setup_database(
            &config,
            Vec::new(),
            pool_provider_factory,
            None,
            None,
            false,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        )
        .await
        .expect("custom provider should build database topology");
        let topology = database.topology.expect("database should be configured");

        assert_eq!(topology.primary().status().max_size, 5);
        assert_eq!(
            topology
                .replica()
                .expect("custom provider should create replica pool")
                .status()
                .max_size,
            2
        );

        let state = build_state(
            &config,
            Some(&topology),
            None,
            #[cfg(feature = "ws")]
            None,
        );
        state
            .probes()
            .mark_replica_connection_unready("replica connection failed");

        assert!(state.read_pool().is_none());
        let (status, _) = crate::probe::readiness_response(&state).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[cfg(feature = "db")]
    fn sharded_test_config() -> AutumnConfig {
        let mut config = AutumnConfig::default();
        config.database.primary_url = Some("postgres://localhost/control".to_owned());
        config.database.shards = vec![
            crate::config::ShardConfig {
                name: "shard0".to_owned(),
                primary_url: "postgres://localhost/shard0".to_owned(),
                slots: Some(vec![crate::config::SlotSpec::Range("0-8191".to_owned())]),
                replica_url: None,
                primary_pool_size: Some(3),
                replica_pool_size: None,
                replica_fallback: None,
            },
            crate::config::ShardConfig {
                name: "shard1".to_owned(),
                primary_url: "postgres://localhost/shard1".to_owned(),
                slots: Some(vec![crate::config::SlotSpec::Range(
                    "8192-16383".to_owned(),
                )]),
                replica_url: Some("postgres://localhost/shard1_ro".to_owned()),
                primary_pool_size: None,
                replica_pool_size: Some(2),
                replica_fallback: None,
            },
        ];
        config
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn setup_database_builds_shard_set_from_config() {
        let config = sharded_test_config();

        let database = setup_database(
            &config,
            Vec::new(),
            None,
            None,
            None,
            false,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        )
        .await
        .expect("sharded config should bootstrap");

        assert!(database.topology.is_some(), "control role configured");
        let shards = database.shards.expect("shards configured");
        assert_eq!(shards.len(), 2);
        assert_eq!(
            shards
                .by_name("shard0")
                .expect("shard0")
                .primary_pool()
                .status()
                .max_size,
            3
        );
        assert_eq!(
            shards
                .by_name("shard1")
                .expect("shard1")
                .replica_pool()
                .expect("shard1 replica")
                .status()
                .max_size,
            2
        );

        let state = build_state(
            &config,
            database.topology.as_ref(),
            Some(shards),
            #[cfg(feature = "ws")]
            None,
        );
        let state_shards = state.shards().expect("state should expose shards");
        assert_eq!(state_shards.len(), 2);
        // Routing works end-to-end through state-held shards.
        let routed = state_shards.route("tenant-1").await.expect("route");
        assert!(["shard0", "shard1"].contains(&routed.name()));
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn custom_pool_provider_builds_shard_topologies() {
        struct CountingProvider(std::sync::Arc<std::sync::atomic::AtomicUsize>);

        impl crate::db::DatabasePoolProvider for CountingProvider {
            async fn create_pool(
                &self,
                config: &crate::config::DatabaseConfig,
            ) -> Result<
                Option<
                    diesel_async::pooled_connection::deadpool::Pool<
                        diesel_async::AsyncPgConnection,
                    >,
                >,
                crate::db::PoolError,
            > {
                crate::db::create_pool(config)
            }

            async fn create_shard_topology(
                &self,
                shard: &crate::config::ShardConfig,
                defaults: &crate::config::DatabaseConfig,
            ) -> Result<crate::db::DatabaseTopology, crate::db::PoolError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                crate::db::create_shard_topology(shard, defaults)
            }
        }

        let config = sharded_test_config();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let AppBuilder {
            pool_provider_factory,
            shard_provider_factory,
            ..
        } = app().with_pool_provider(CountingProvider(calls.clone()));

        let database = setup_database(
            &config,
            Vec::new(),
            pool_provider_factory,
            shard_provider_factory,
            None,
            false,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        )
        .await
        .expect("provider should build shard topologies");

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(database.shards.expect("shards").len(), 2);
    }

    #[cfg(feature = "db")]
    #[test]
    fn repository_commit_hook_worker_starts_after_job_runtime_initialization() {
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let server_init = "initialize_job_runtime(jobs, &state, &server_shutdown, &config.jobs)";
        let server_worker = "start_repository_commit_hook_worker(\n                pool,\n                server_shutdown.child_token(),\n            );";
        let task_init = "initialize_job_runtime(jobs, &state, &task_shutdown, &config.jobs)";
        let task_worker = "start_repository_commit_hook_worker(\n                pool,\n                task_shutdown.child_token(),\n            );";

        assert!(
            source
                .find(server_init)
                .expect("normal server path should initialize jobs")
                < source
                    .find(server_worker)
                    .expect("normal server path should start repository hook worker"),
            "normal server startup must initialize jobs before repository commit hooks can enqueue them"
        );
        assert!(
            source
                .find(task_init)
                .expect("task runner path should initialize jobs")
                < source
                    .find(task_worker)
                    .expect("task runner path should start repository hook worker"),
            "task runner startup must initialize jobs before repository commit hooks can enqueue them"
        );
    }

    #[test]
    fn state_initializers_run_before_job_runtime_initialization() {
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let server_start = source
            .find("pub async fn run(self)")
            .expect("normal server path should exist");
        let build_mode_start = source
            .find("async fn run_build_mode(self)")
            .expect("static build path should follow server path");
        let task_start = source
            .find("async fn run_one_off_task_mode(self, requested_name: String)")
            .expect("task runner path should exist");
        let server_source = &source[server_start..build_mode_start];
        let task_source = &source[task_start..];
        let server_init = "initialize_job_runtime(jobs, &state, &server_shutdown, &config.jobs)";
        let task_init = "initialize_job_runtime(jobs, &state, &task_shutdown, &config.jobs)";
        let server_initializer = server_source
            .find("run_state_initializers(state_initializers, &state);")
            .expect("normal server path should run state initializers");
        let task_initializer = task_source
            .find("run_state_initializers(state_initializers, &state);")
            .expect("task runner path should run state initializers");
        let server_job = server_source
            .find(server_init)
            .expect("normal server path should initialize jobs");
        let task_job = task_source
            .find(task_init)
            .expect("task runner path should initialize jobs");

        assert!(
            server_initializer < server_job,
            "normal server startup must install state-initialized resources before job workers start"
        );
        assert!(
            task_initializer < task_job,
            "task runner startup must install state-initialized resources before job workers start"
        );
    }

    #[test]
    fn static_builds_run_state_initializers_before_router_build() {
        let source = include_str!("app.rs").replace("\r\n", "\n");
        let build_mode_start = source
            .find("async fn run_build_mode(self)")
            .expect("static build path should exist");
        let dump_mode_start = source
            .find("async fn run_dump_routes_mode(self)")
            .expect("route dump path should follow static build path");
        let build_mode_source = &source[build_mode_start..dump_mode_start];
        let state_initializer = build_mode_source
            .find("run_state_initializers(state_initializers, &state);")
            .expect("static build path should run state initializers");
        let router_build = build_mode_source
            .find("let router = crate::router::try_build_router_inner(")
            .expect("static build path should build a router");

        assert!(
            state_initializer < router_build,
            "static builds must install state-initialized resources before rendering routes"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn hooked_repository_apps_include_hook_queue_framework_migration() {
        let migrations = migrations_with_repository_framework_migrations(
            vec![APP_TEST_MIGRATIONS],
            true,
            false,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        );
        let names = migration_names(&migrations);

        assert!(
            names
                .iter()
                .any(|name| name == REPOSITORY_COMMIT_HOOK_QUEUE_MIGRATION),
            "hooked repository apps must auto-register the durable hook queue migration"
        );
        assert!(
            names.iter().all(|name| !name.contains("api_tokens")),
            "hooked repository apps must not auto-register unrelated framework migrations: {names:?}"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn runtime_hooked_apps_include_hook_queue_framework_migration_without_app_migrations() {
        let migrations = migrations_with_repository_framework_migrations(
            Vec::new(),
            true,
            false,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        );
        let names = migration_names(&migrations);

        assert!(
            names
                .iter()
                .any(|name| name == REPOSITORY_COMMIT_HOOK_QUEUE_MIGRATION),
            "runtime hooked repository apps must install the durable hook queue even when app migrations are managed elsewhere"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn versioned_repository_apps_include_version_history_framework_migration() {
        let migrations = migrations_with_repository_framework_migrations(
            vec![APP_TEST_MIGRATIONS],
            false,
            true,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        );
        let names = migration_names(&migrations);

        assert!(
            names.iter().any(|name| name == VERSION_HISTORY_MIGRATION),
            "versioned repository apps must auto-register the version-history migration"
        );
        assert!(
            names
                .iter()
                .all(|name| !name.contains("repository_commit_hook_queue")),
            "versioned-only repository apps must not auto-register the durable hook queue: {names:?}"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn runtime_versioned_apps_include_version_history_framework_migration_without_app_migrations() {
        let migrations = migrations_with_repository_framework_migrations(
            Vec::new(),
            false,
            true,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        );
        let names = migration_names(&migrations);

        assert!(
            names.iter().any(|name| name == VERSION_HISTORY_MIGRATION),
            "runtime versioned repository apps must install version history even when app migrations are managed elsewhere"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn static_builds_do_not_auto_add_hook_queue_when_no_migrations_registered() {
        let migrations = migrations_with_repository_framework_migrations(
            Vec::new(),
            true,
            true,
            RepositoryCommitHookQueueMigrationMode::StaticBuild,
        );

        assert!(
            migrations.is_empty(),
            "static/export builds that pass no migrations must not mutate the database"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn directory_migration_required_only_at_runtime_with_shards_and_routing() {
        use RepositoryCommitHookQueueMigrationMode::{Runtime, StaticBuild};

        // The happy path: routing on, shards present, real runtime boot.
        assert!(directory_migration_is_required(true, true, Runtime));

        // A static build must never create the directory table, even with
        // routing enabled and shards configured.
        assert!(!directory_migration_is_required(true, true, StaticBuild));

        // Routing disabled, or no shards, means no directory table at all.
        assert!(!directory_migration_is_required(false, true, Runtime));
        assert!(!directory_migration_is_required(true, false, Runtime));
    }

    #[test]
    fn shard_map_migration_required_only_at_runtime_with_shards() {
        use RepositoryCommitHookQueueMigrationMode::{Runtime, StaticBuild};

        // The happy path: shards present, real runtime boot.
        assert!(shard_map_migration_is_required(true, Runtime));

        // A static build must never create the shard-map table.
        assert!(!shard_map_migration_is_required(true, StaticBuild));

        // No shards means no shard-map table.
        assert!(!shard_map_migration_is_required(false, Runtime));
    }

    #[cfg(feature = "db")]
    #[test]
    fn unhooked_apps_do_not_auto_add_hook_queue_framework_migration() {
        let migrations = migrations_with_repository_framework_migrations(
            Vec::new(),
            false,
            false,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        );

        assert!(
            migrations.is_empty(),
            "unhooked apps should not get durable hook queue migrations for free"
        );
    }

    #[cfg(feature = "db")]
    fn migration_names(migrations: &[crate::migrate::EmbeddedMigrations]) -> Vec<String> {
        use diesel::migration::{Migration, MigrationSource as _};
        use diesel::pg::Pg;

        migrations
            .iter()
            .flat_map(|source| {
                let migrations: Vec<Box<dyn Migration<Pg>>> = source.migrations().unwrap();
                migrations
            })
            .map(|migration| migration.name().to_string())
            .collect()
    }

    #[cfg(feature = "db")]
    #[test]
    fn control_framework_filter_skips_control_but_keeps_shard_required_sets() {
        // The full control set is skipped on shards...
        assert!(migration_set_is_control_framework(
            &crate::migrate::FRAMEWORK_MIGRATIONS
        ));
        // ...but the standalone shard-required sets are kept (not flagged),
        // even though their migrations are duplicated into the control
        // `migrations/` directory.
        assert!(!migration_set_is_control_framework(
            &crate::version_history::VERSION_HISTORY_MIGRATIONS
        ));
        assert!(!migration_set_is_control_framework(
            &crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS
        ));
    }

    #[cfg(feature = "db")]
    #[test]
    fn sharded_app_with_full_framework_still_gets_shard_required_sets() {
        use diesel::migration::{Migration, MigrationSource as _};
        use diesel::pg::Pg;

        // A sharded app that registers the full control FRAMEWORK_MIGRATIONS and
        // also uses commit hooks + versioning. The hook-queue / version-history
        // migrations are present *inside* the control set, but that set is
        // stripped from shard targets by `migration_set_is_control_framework`, so
        // the standalone shard-required sets must still be appended — otherwise
        // shards never get those tables.
        let migrations = migrations_with_repository_framework_migrations(
            vec![crate::migrate::FRAMEWORK_MIGRATIONS],
            true,
            true,
            RepositoryCommitHookQueueMigrationMode::Runtime,
        );

        // The migration names the shard apply loop will actually run: every set
        // that is not the control framework set (which gets stripped on shards).
        let shard_names: Vec<String> = migrations
            .iter()
            .filter(|set| !migration_set_is_control_framework(set))
            .flat_map(|set| {
                let ms: Vec<Box<dyn Migration<Pg>>> = set.migrations().unwrap_or_default();
                ms.into_iter()
                    .map(|m| m.name().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();

        assert!(
            shard_names
                .iter()
                .any(|name| name == REPOSITORY_COMMIT_HOOK_QUEUE_MIGRATION),
            "shards must receive the commit-hook queue migration even when the full \
             control framework set is also registered: {shard_names:?}"
        );
        assert!(
            shard_names
                .iter()
                .any(|name| name == VERSION_HISTORY_MIGRATION),
            "shards must receive the version-history migration even when the full \
             control framework set is also registered: {shard_names:?}"
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn configure_replica_migration_check_stores_recheck_urls() {
        let mut config = AutumnConfig::default();
        config.database.primary_url = Some("postgres://localhost/primary".to_owned());
        config.database.replica_url = Some("postgres://localhost/replica".to_owned());
        let topology = crate::db::create_topology(&config.database)
            .expect("topology should build")
            .expect("database should be configured");

        let state = build_state(
            &config,
            Some(&topology),
            None,
            #[cfg(feature = "ws")]
            None,
        );

        assert!(
            state.probes().replica_migration_check().is_none(),
            "build_state should not enable migration checks without registered migrations"
        );

        configure_replica_migration_check(
            &state,
            Some((
                "postgres://localhost/primary".to_owned(),
                "postgres://localhost/replica".to_owned(),
            )),
        );

        let check = state
            .probes()
            .replica_migration_check()
            .expect("replica migration check should be configured");

        assert_eq!(check.primary_url, "postgres://localhost/primary");
        assert_eq!(check.replica_url, "postgres://localhost/replica");
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn replica_migration_readiness_marks_ready_endpoint_degraded() {
        let mut config = AutumnConfig::default();
        config.database.primary_url = Some("postgres://localhost/primary".to_owned());
        config.database.primary_pool_size = Some(5);
        config.database.replica_url = Some("postgres://localhost/replica".to_owned());
        config.database.replica_pool_size = Some(2);
        config.database.replica_fallback = crate::config::ReplicaFallback::FailReadiness;
        let topology = crate::db::create_topology(&config.database)
            .expect("topology should build")
            .expect("database should be configured");
        let state = build_state(
            &config,
            Some(&topology),
            None,
            #[cfg(feature = "ws")]
            None,
        );

        apply_replica_migration_readiness(
            &state,
            Some(crate::migrate::ReplicaMigrationReadiness::Stale {
                primary_latest: Some("00000000000002".to_owned()),
                replica_latest: Some("00000000000001".to_owned()),
            }),
        );

        let (status, _) = crate::probe::readiness_response(&state).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn blocking_replica_migration_readiness_reports_unknown_connection_errors() {
        let readiness = crate::migrate::check_replica_migration_readiness_blocking(
            "not-a-primary-url".to_owned(),
            "not-a-replica-url".to_owned(),
        )
        .await;

        assert!(matches!(
            readiness,
            crate::migrate::ReplicaMigrationReadiness::Unknown(_)
        ));
    }

    #[cfg(feature = "ws")]
    #[test]
    fn with_channels_backend_overrides_config_driven_backend_selection() {
        let builder = app().with_channels_backend(crate::channels::LocalChannelsBackend::new(4));
        let AppBuilder {
            channels_backend, ..
        } = builder;
        assert!(channels_backend.is_some());

        let mut config = AutumnConfig::default();
        config.channels.backend = crate::config::ChannelBackend::Redis;
        config.channels.redis.url = None;

        let state = build_state(
            &config,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "ws")]
            channels_backend,
        );
        let mut rx = state.channels().subscribe("override");

        state
            .broadcast()
            .publish("override", "ok")
            .expect("custom local backend should publish");

        assert_eq!(rx.try_recv().expect("message should arrive").as_str(), "ok");
    }

    /// Helper to create a simple GET route for testing.
    pub fn test_get_route(path: &'static str, name: &'static str) -> Route {
        Route {
            method: http::Method::GET,
            path,
            handler: axum::routing::get(|| async { "ok" }),
            name,
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path,
                operation_id: name,
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            api_version: None,
            sunset_opt_out: false,
        }
    }

    #[cfg(feature = "i18n")]
    fn test_i18n_bundle(key: &str, value: &str) -> Arc<crate::i18n::Bundle> {
        let mut messages = std::collections::HashMap::new();
        let mut en = std::collections::HashMap::new();
        en.insert(key.to_owned(), value.to_owned());
        messages.insert("en".to_owned(), en);
        Arc::new(crate::i18n::Bundle::from_messages(
            messages,
            &crate::i18n::I18nConfig::default(),
        ))
    }

    #[cfg(feature = "i18n")]
    #[test]
    fn i18n_auto_defers_loading_until_runtime_config_is_available() {
        let builder = app().i18n_auto();

        assert!(builder.i18n_bundle.is_none());
        assert!(builder.i18n_auto_load);
    }

    #[cfg(feature = "i18n")]
    #[derive(Clone)]
    struct StaticConfigLoader {
        config: AutumnConfig,
    }

    #[cfg(feature = "i18n")]
    impl crate::config::ConfigLoader for StaticConfigLoader {
        async fn load(&self) -> Result<AutumnConfig, crate::config::ConfigError> {
            Ok(self.config.clone())
        }
    }

    #[cfg(feature = "i18n")]
    struct NoopTelemetryProvider;

    #[cfg(feature = "i18n")]
    impl crate::telemetry::TelemetryProvider for NoopTelemetryProvider {
        fn init(
            &self,
            _log: &crate::config::LogConfig,
            _telemetry: &crate::config::TelemetryConfig,
            _profile: Option<&str>,
        ) -> Result<crate::telemetry::TelemetryGuard, crate::telemetry::TelemetryInitError>
        {
            Ok(crate::telemetry::TelemetryGuard::disabled())
        }
    }

    #[cfg(feature = "i18n")]
    #[tokio::test]
    async fn i18n_auto_uses_config_loader_output_for_bundle_dir() {
        let project = tempfile::tempdir().expect("project dir");
        let i18n_dir = project.path().join("custom-i18n");
        std::fs::create_dir_all(&i18n_dir).expect("i18n dir");
        std::fs::write(i18n_dir.join("en.ftl"), "nav.home = Loader Home\n").expect("bundle");

        let mut config = AutumnConfig::default();
        config.i18n.dir = "custom-i18n".to_owned();
        let builder = app()
            .with_config_loader(StaticConfigLoader { config })
            .with_telemetry_provider(NoopTelemetryProvider)
            .i18n_auto();
        let AppBuilder {
            config_loader_factory,
            telemetry_provider,
            i18n_bundle,
            i18n_auto_load,
            ..
        } = builder;

        let (loaded_config, _guard) =
            load_config_and_telemetry(config_loader_factory, telemetry_provider).await;
        let env = crate::config::MockEnv::new().with(
            "AUTUMN_MANIFEST_DIR",
            project.path().to_str().expect("utf-8 path"),
        );
        let bundle = resolve_i18n_bundle(i18n_bundle, i18n_auto_load, &loaded_config, &env)
            .expect("bundle loaded from configured dir");

        assert_eq!(bundle.translate("en", "nav.home", &[]), "Loader Home");
    }

    #[cfg(feature = "i18n")]
    #[tokio::test]
    async fn i18n_bundle_layer_is_applied_to_static_route_rendering() {
        async fn localized(locale: crate::i18n::Locale) -> String {
            locale.t("nav.home")
        }

        let config = AutumnConfig::default();
        let state = AppState::for_test();
        let custom_layers = install_i18n_bundle_layer(
            Vec::new(),
            &state,
            Some(test_i18n_bundle("nav.home", "Home")),
        );
        let router = crate::router::try_build_router_inner(
            vec![Route {
                method: http::Method::GET,
                path: "/about",
                handler: axum::routing::get(localized),
                name: "localized",
                api_doc: crate::openapi::ApiDoc {
                    method: "GET",
                    path: "/about",
                    operation_id: "localized",
                    success_status: 200,
                    ..Default::default()
                },
                repository: None,
                idempotency: crate::route::RouteIdempotency::Direct,
                timeout: crate::route::RouteTimeout::Inherit,
                api_version: None,
                sunset_opt_out: false,
            }],
            &config,
            state,
            crate::router::RouterContext {
                exception_filters: Vec::new(),
                scoped_groups: Vec::new(),
                merge_routers: Vec::new(),
                nest_routers: Vec::new(),
                custom_layers,
                static_gate_layers: Vec::new(),
                #[cfg(feature = "maud")]
                error_page_renderer: None,
                session_store: None,
                #[cfg(feature = "openapi")]
                openapi: None,
                #[cfg(feature = "mcp")]
                mcp: None,
            },
        )
        .expect("router builds");
        let tmp = tempfile::tempdir().expect("dist parent");
        let dist = tmp.path().join("dist");

        crate::static_gen::render_static_routes(
            router,
            &[crate::static_gen::StaticRouteMeta {
                path: "/about",
                name: "localized",
                revalidate: None,
                params_fn: None,
            }],
            &dist,
        )
        .await
        .expect("static render succeeds");

        let html = std::fs::read_to_string(dist.join("about/index.html")).expect("rendered html");
        assert_eq!(html, "Home");
    }

    #[test]
    fn app_builder_routes_adds_routes() {
        let builder = app();
        assert_eq!(builder.routes.len(), 0);

        let builder = builder.routes(vec![test_get_route("/1", "route1")]);
        assert_eq!(builder.routes.len(), 1);

        let builder = builder.routes(vec![
            test_get_route("/2", "route2"),
            test_get_route("/3", "route3"),
        ]);
        assert_eq!(builder.routes.len(), 3);

        assert_eq!(builder.routes[0].path, "/1");
        assert_eq!(builder.routes[1].path, "/2");
        assert_eq!(builder.routes[2].path, "/3");
    }

    #[test]
    fn app_builder_extensions_store_and_update_typed_values() {
        let builder = app()
            .with_extension::<String>("haunted".into())
            .update_extension::<String, _, _>(String::new, |value| value.push_str(" harvest"));

        let value = builder
            .extension::<String>()
            .expect("string extension should be present");
        assert_eq!(value, "haunted harvest");
    }

    #[cfg(feature = "mail")]
    #[tokio::test]
    async fn app_builder_with_mail_delivery_queue_stores_queue_for_install() {
        let builder = app().with_mail_delivery_queue(MailTestNoopQueue);
        let factory = builder
            .mail_delivery_queue_factory
            .expect("with_mail_delivery_queue should store a factory on the builder");

        // Invoke the trivial wrapper closure built by with_mail_delivery_queue
        // and verify it returns the wrapped queue successfully.
        let state = AppState::for_test();
        let queue = factory(&state).expect("trivial factory should produce the queue");
        assert!(Arc::strong_count(&queue) >= 1);
        // Cover the enqueue method body by invoking it once.
        queue
            .enqueue(test_mail())
            .await
            .expect("noop queue should always succeed");
    }

    #[cfg(feature = "mail")]
    #[test]
    fn app_builder_with_mail_delivery_queue_factory_runs_with_app_state() {
        let observed_profile: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let captured = Arc::clone(&observed_profile);
        let builder = app().with_mail_delivery_queue_factory(move |state| {
            *captured.lock().expect("lock") = Some(state.profile().to_owned());
            Ok::<_, crate::AutumnError>(MailTestNoopQueue)
        });

        let factory = builder
            .mail_delivery_queue_factory
            .expect("factory should be stored on the builder");
        let state = AppState::for_test().with_profile("dev");
        let _queue = factory(&state).expect("factory should succeed");

        assert_eq!(
            observed_profile.lock().expect("lock").as_deref(),
            Some("dev"),
            "factory must run with the live AppState"
        );
    }

    #[cfg(feature = "mail")]
    #[test]
    fn app_builder_with_mail_delivery_queue_factory_propagates_errors() {
        let builder = app().with_mail_delivery_queue_factory(|_state| {
            Err::<MailTestNoopQueue, _>(crate::AutumnError::service_unavailable_msg("factory boom"))
        });

        let factory = builder
            .mail_delivery_queue_factory
            .expect("factory present");
        let state = AppState::for_test();
        match factory(&state) {
            Ok(_) => panic!("factory should have errored"),
            Err(err) => assert!(err.to_string().contains("factory boom")),
        }
    }

    #[tokio::test]
    async fn startup_and_shutdown_hooks_run_in_expected_order() {
        let events = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let startup_events = Arc::clone(&events);
        let shutdown_a = Arc::clone(&events);
        let shutdown_b = Arc::clone(&events);
        let builder = app()
            .on_startup(move |_state| {
                let startup_events = Arc::clone(&startup_events);
                async move {
                    startup_events
                        .lock()
                        .expect("events lock poisoned")
                        .push("start");
                    Ok(())
                }
            })
            .on_shutdown(move || {
                let shutdown_a = Arc::clone(&shutdown_a);
                async move {
                    shutdown_a
                        .lock()
                        .expect("events lock poisoned")
                        .push("stop-a");
                }
            })
            .on_shutdown(move || {
                let shutdown_b = Arc::clone(&shutdown_b);
                async move {
                    shutdown_b
                        .lock()
                        .expect("events lock poisoned")
                        .push("stop-b");
                }
            });

        run_startup_hooks(&builder.startup_hooks, AppState::for_test())
            .await
            .expect("startup hooks should succeed");
        run_shutdown_hooks(&builder.shutdown_hooks).await;

        let recorded_events = events.lock().expect("events lock poisoned").clone();
        assert_eq!(recorded_events, vec!["start", "stop-b", "stop-a"]);
    }

    fn startup_noop_job_handler(
        _state: AppState,
        _payload: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = crate::AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move { Ok(()) })
    }

    #[tokio::test]
    async fn startup_hooks_can_enqueue_jobs_after_runtime_init() {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let builder = app()
            .jobs(vec![crate::job::JobInfo {
                name: "startup-seed".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: startup_noop_job_handler,
            }])
            .on_startup(|_state| async {
                crate::job::enqueue("startup-seed", serde_json::json!({ "kind": "warmup" })).await
            });

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();

        initialize_job_runtime(
            builder.jobs.clone(),
            &state,
            &shutdown,
            &crate::config::JobConfig::default(),
        )
        .expect("job runtime should initialize before startup hooks");

        run_startup_hooks(&builder.startup_hooks, state.clone())
            .await
            .expect("startup hook should be able to enqueue jobs");

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let snapshot = state.job_registry().snapshot();
                let status = snapshot
                    .get("startup-seed")
                    .expect("job should be registered before startup hooks run");
                if status.total_successes == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("startup-enqueued job should complete");

        shutdown.cancel();
        crate::job::clear_global_job_client();
    }

    #[tokio::test]
    async fn initialize_job_runtime_propagates_redis_init_errors() {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let config = crate::config::JobConfig {
            backend: "redis".to_string(),
            ..Default::default()
        };

        let error = initialize_job_runtime(
            vec![crate::job::JobInfo {
                name: "startup-seed".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: startup_noop_job_handler,
            }],
            &state,
            &shutdown,
            &config,
        )
        .expect_err("redis init errors should abort startup");

        #[cfg(feature = "redis")]
        assert!(
            error
                .to_string()
                .contains("jobs.backend=redis requires jobs.redis.url"),
            "unexpected error: {error}"
        );

        #[cfg(not(feature = "redis"))]
        assert!(
            error
                .to_string()
                .contains("jobs.backend=redis requested but redis feature is disabled"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn startup_hook_errors_propagate() {
        let builder = app().on_startup(|_state| async {
            Err(crate::AutumnError::service_unavailable_msg(
                "startup ritual failed",
            ))
        });

        let error = run_startup_hooks(&builder.startup_hooks, AppState::for_test())
            .await
            .expect_err("startup hook should fail");
        assert!(error.to_string().contains("startup ritual failed"));
    }

    #[tokio::test]
    async fn build_router_mounts_user_routes() {
        let router = test_router(vec![test_get_route("/test", "test_handler")]);

        let response = router
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn build_router_mounts_health_check_at_default_path() {
        let router = test_router(vec![test_get_route("/dummy", "dummy")]);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn build_router_mounts_health_check_at_custom_path() {
        let mut config = AutumnConfig::default();
        config.health.path = "/healthz".to_owned();
        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let router =
            crate::router::build_router(vec![test_get_route("/dummy", "dummy")], &config, state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn build_router_adds_request_id_header() {
        let router = test_router(vec![test_get_route("/test", "test")]);

        let response = router
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert!(response.headers().contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn build_router_unknown_route_returns_404() {
        let router = test_router(vec![test_get_route("/exists", "exists")]);

        let response = router
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn build_router_multiple_routes() {
        let router = test_router(vec![test_get_route("/a", "a"), test_get_route("/b", "b")]);

        let resp_a = router
            .clone()
            .oneshot(Request::builder().uri("/a").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp_a.status(), StatusCode::OK);

        let resp_b = router
            .oneshot(Request::builder().uri("/b").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp_b.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn build_router_post_route() {
        let post_routes = vec![Route {
            method: http::Method::POST,
            path: "/submit",
            handler: axum::routing::post(|| async { "posted" }),
            name: "submit",
            api_doc: crate::openapi::ApiDoc {
                method: "POST",
                path: "/submit",
                operation_id: "submit",
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            api_version: None,
            sunset_opt_out: false,
        }];
        let config = AutumnConfig::default();
        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let router = crate::router::build_router(post_routes, &config, state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn build_router_merges_methods_on_same_path() {
        let route_list = vec![
            Route {
                method: http::Method::GET,
                path: "/admin",
                handler: axum::routing::get(|| async { "list" }),
                name: "admin_list",
                api_doc: crate::openapi::ApiDoc {
                    method: "GET",
                    path: "/admin",
                    operation_id: "admin_list",
                    success_status: 200,
                    ..Default::default()
                },
                repository: None,
                idempotency: crate::route::RouteIdempotency::Direct,
                timeout: crate::route::RouteTimeout::Inherit,
                api_version: None,
                sunset_opt_out: false,
            },
            Route {
                method: http::Method::POST,
                path: "/admin",
                handler: axum::routing::post(|| async { "created" }),
                name: "create",
                api_doc: crate::openapi::ApiDoc {
                    method: "POST",
                    path: "/admin",
                    operation_id: "create",
                    success_status: 200,
                    ..Default::default()
                },
                repository: None,
                idempotency: crate::route::RouteIdempotency::Direct,
                timeout: crate::route::RouteTimeout::Inherit,
                api_version: None,
                sunset_opt_out: false,
            },
        ];
        let config = AutumnConfig::default();
        let router = crate::router::build_router(route_list, &config, AppState::for_test());

        // GET /admin should return "list"
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"list");

        // POST /admin should return "created" (not 405!)
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"created");
    }

    #[cfg(feature = "htmx")]
    #[tokio::test]
    async fn htmx_handler_returns_javascript_with_correct_headers() {
        let app = axum::Router::new().route(
            crate::htmx::HTMX_JS_PATH,
            axum::routing::get(crate::router::htmx_handler),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri(crate::htmx::HTMX_JS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("application/javascript"),
            "Expected application/javascript, got {content_type}"
        );

        let cache_control = response
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            cache_control.contains("immutable"),
            "Expected immutable cache, got {cache_control}"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        // Body length matches the embedded file
        assert_eq!(body.len(), crate::htmx::HTMX_JS.len());

        // Body starts with valid JavaScript
        let start = std::str::from_utf8(&body[..50]).expect("htmx should be valid UTF-8");
        assert!(
            start.contains("htmx") || start.contains("function"),
            "Response doesn't look like htmx JavaScript: {start}"
        );
    }

    #[cfg(feature = "htmx")]
    #[tokio::test]
    async fn htmx_csrf_handler_returns_csp_compatible_javascript() {
        let app = axum::Router::new().route(
            crate::htmx::HTMX_CSRF_JS_PATH,
            axum::routing::get(crate::router::htmx_csrf_handler),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri(crate::htmx::HTMX_CSRF_JS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/javascript")
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let js = std::str::from_utf8(&body).expect("csrf helper should be valid utf-8");

        assert!(js.contains("htmx:configRequest"));
        assert!(js.contains("X-CSRF-Token"));
        assert!(!js.contains("<script"));
    }

    #[cfg(feature = "htmx")]
    #[tokio::test]
    async fn build_router_serves_htmx_js() {
        let router = test_router(vec![test_get_route("/dummy", "dummy")]);

        let response = router
            .oneshot(
                Request::builder()
                    .uri(crate::htmx::HTMX_JS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("javascript"));
    }

    #[cfg(feature = "htmx")]
    #[tokio::test]
    async fn build_router_serves_htmx_csrf_js() {
        let router = test_router(vec![test_get_route("/dummy", "dummy")]);

        let response = router
            .oneshot(
                Request::builder()
                    .uri(crate::htmx::HTMX_CSRF_JS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let csp = response
            .headers()
            .get("content-security-policy")
            .expect("framework JS should still receive security headers")
            .to_str()
            .unwrap();
        assert!(csp.contains("script-src 'self'"), "csp = {csp}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let js = std::str::from_utf8(&body).expect("csrf helper should be valid utf-8");
        assert!(js.contains("htmx:configRequest"));
        assert!(js.contains("X-CSRF-Token"));
    }

    #[tokio::test]
    async fn build_router_serves_default_favicon_without_404() {
        let router = test_router(vec![test_get_route("/dummy", "dummy")]);

        let response = router
            .oneshot(
                Request::builder()
                    .uri(crate::router::DEFAULT_FAVICON_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            response.headers().contains_key("content-security-policy"),
            "framework fallback responses should still receive security headers"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn build_router_does_not_override_user_favicon_route() {
        let router = test_router(vec![test_get_route(
            crate::router::DEFAULT_FAVICON_PATH,
            "favicon",
        )]);

        let response = router
            .oneshot(
                Request::builder()
                    .uri(crate::router::DEFAULT_FAVICON_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn build_router_serves_static_files_for_unmatched_paths() {
        use std::collections::HashMap;

        // Create a temp dist/ with a static page
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(dist.join("docs")).expect("mkdir");
        std::fs::write(dist.join("docs/index.html"), "<h1>Static Docs</h1>").expect("write");

        let manifest = crate::static_gen::StaticManifest {
            generated_at: "2026-03-27T00:00:00Z".to_owned(),
            autumn_version: "0.2.0".to_owned(),
            routes: HashMap::from([(
                "/docs".to_owned(),
                crate::static_gen::ManifestEntry {
                    file: "docs/index.html".to_owned(),
                    revalidate: None,
                },
            )]),
        };
        let json = serde_json::to_string(&manifest).expect("serialize");
        std::fs::write(dist.join("manifest.json"), json).expect("write manifest");

        // No dynamic route for /docs — only a static file.
        let config = AutumnConfig::default();
        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let router = crate::router::build_router_with_static(
            vec![test_get_route("/other", "other_page")],
            &config,
            state,
            Some(dist.as_path()),
        );

        // GET /docs/ should serve the pre-built HTML via static-first
        // middleware (manifest lookup with trailing-slash normalization).
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/docs/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let csp = response
            .headers()
            .get("content-security-policy")
            .expect("static-first HTML should still receive security headers")
            .to_str()
            .unwrap();
        assert!(csp.contains("script-src 'self'"), "csp = {csp}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "<h1>Static Docs</h1>");
    }

    #[tokio::test]
    async fn build_mode_static_rendering_bypasses_startup_barrier() {
        temp_env::async_with_vars([("AUTUMN_BUILD_STATIC", Some("1"))], async {
            let config = AutumnConfig::default();
            let state = AppState::for_test().with_startup_complete(false);
            let router = crate::router::build_router(
                vec![Route {
                    method: http::Method::GET,
                    path: "/about",
                    handler: axum::routing::get(|| async { "About Page Content" }),
                    name: "about",
                    api_doc: crate::openapi::ApiDoc {
                        method: "GET",
                        path: "/about",
                        operation_id: "about",
                        success_status: 200,
                        ..Default::default()
                    },
                    repository: None,
                    idempotency: crate::route::RouteIdempotency::Direct,
                    timeout: crate::route::RouteTimeout::Inherit,
                    api_version: None,
                    sunset_opt_out: false,
                }],
                &config,
                state,
            );
            let tmp = tempfile::tempdir().unwrap();
            let dist = tmp.path().join("dist");

            let result = crate::static_gen::render_static_routes(
                router,
                &[crate::static_gen::StaticRouteMeta {
                    path: "/about",
                    name: "about",
                    revalidate: None,
                    params_fn: None,
                }],
                &dist,
            )
            .await;

            assert!(result.is_ok(), "build failed: {:?}", result.err());
            let html = std::fs::read_to_string(dist.join("about/index.html")).unwrap();
            assert_eq!(html, "About Page Content");
        })
        .await;
    }

    #[tokio::test]
    async fn build_router_injects_live_reload_script_when_enabled() {
        let reload_file = tempfile::NamedTempFile::new().expect("reload state file");
        std::fs::write(reload_file.path(), r#"{"version":0,"kind":"full"}"#).expect("write");
        temp_env::async_with_vars(
            [
                ("AUTUMN_DEV_RELOAD", Some("1")),
                (
                    "AUTUMN_DEV_RELOAD_STATE",
                    Some(reload_file.path().to_str().expect("utf-8 path")),
                ),
            ],
            async {
                let router = test_router(vec![Route {
                    method: http::Method::GET,
                    path: "/page",
                    handler: axum::routing::get(|| async {
                        axum::response::Html("<html><body><main>ok</main></body></html>")
                    }),
                    name: "page",
                    api_doc: crate::openapi::ApiDoc {
                        method: "GET",
                        path: "/page",
                        operation_id: "page",
                        success_status: 200,
                        ..Default::default()
                    },
                    repository: None,
                    idempotency: crate::route::RouteIdempotency::Direct,
                    timeout: crate::route::RouteTimeout::Inherit,
                    api_version: None,
                    sunset_opt_out: false,
                }]);

                let response = router
                    .oneshot(Request::builder().uri("/page").body(Body::empty()).unwrap())
                    .await
                    .unwrap();

                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let html = std::str::from_utf8(&body).expect("utf-8");
                assert!(html.contains("/__autumn/live-reload"));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn build_router_mounts_dev_reload_script_endpoint_when_enabled() {
        // The injected <script src="/__autumn/live-reload.js"> tag only works
        // under the default CSP (`script-src 'self'`) if the framework
        // actually serves the JS at that path. This guards against the
        // regression where the script endpoint is forgotten.
        let reload_file = tempfile::NamedTempFile::new().expect("reload state file");
        std::fs::write(reload_file.path(), r#"{"version":0,"kind":"full"}"#).expect("write");
        temp_env::async_with_vars(
            [
                ("AUTUMN_DEV_RELOAD", Some("1")),
                (
                    "AUTUMN_DEV_RELOAD_STATE",
                    Some(reload_file.path().to_str().expect("utf-8 path")),
                ),
            ],
            async {
                let router = test_router(vec![test_get_route("/dummy", "dummy")]);

                let response = router
                    .oneshot(
                        Request::builder()
                            .uri("/__autumn/live-reload.js")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok()),
                    Some("application/javascript; charset=utf-8")
                );
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let js = std::str::from_utf8(&body).expect("utf-8");
                assert!(js.contains("fetch("), "js body: {js}");
            },
        )
        .await;
    }

    #[tokio::test]
    async fn build_router_mounts_dev_reload_endpoint_when_enabled() {
        let reload_file = tempfile::NamedTempFile::new().expect("reload state file");
        std::fs::write(reload_file.path(), r#"{"version":7,"kind":"css"}"#).expect("write");
        temp_env::async_with_vars(
            [
                ("AUTUMN_DEV_RELOAD", Some("1")),
                (
                    "AUTUMN_DEV_RELOAD_STATE",
                    Some(reload_file.path().to_str().expect("utf-8 path")),
                ),
            ],
            async {
                let router = test_router(vec![test_get_route("/dummy", "dummy")]);

                let response = router
                    .oneshot(
                        Request::builder()
                            .uri("/__autumn/live-reload")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers().get("cache-control").unwrap(),
                    "no-store, no-cache, must-revalidate"
                );
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                assert_eq!(&body[..], br#"{"version":7,"kind":"css"}"#);
            },
        )
        .await;
    }

    #[tokio::test]
    async fn build_router_disables_cache_for_static_assets_in_dev_reload_mode() {
        let project = tempfile::tempdir().expect("project dir");
        let static_dir = project.path().join("static");
        std::fs::create_dir_all(&static_dir).expect("mkdir");
        std::fs::write(static_dir.join("demo.txt"), "hello").expect("write static file");
        let reload_file = tempfile::NamedTempFile::new().expect("reload state file");
        std::fs::write(reload_file.path(), r#"{"version":0,"kind":"full"}"#).expect("write");
        temp_env::async_with_vars(
            [
                (
                    "AUTUMN_MANIFEST_DIR",
                    Some(project.path().to_str().expect("utf-8 path")),
                ),
                ("AUTUMN_DEV_RELOAD", Some("1")),
                (
                    "AUTUMN_DEV_RELOAD_STATE",
                    Some(reload_file.path().to_str().expect("utf-8 path")),
                ),
            ],
            async {
                let router = test_router(vec![test_get_route("/dummy", "dummy")]);

                let response = router
                    .oneshot(
                        Request::builder()
                            .uri("/static/demo.txt")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers().get("cache-control").unwrap(),
                    "no-store, no-cache, must-revalidate"
                );
            },
        )
        .await;
    }

    #[test]
    fn app_builder_accepts_static_routes() {
        use crate::static_gen::StaticRouteMeta;
        let metas = vec![StaticRouteMeta {
            path: "/about",
            name: "about",
            revalidate: None,
            params_fn: None,
        }];
        let builder = app().static_routes(metas);
        assert_eq!(builder.static_metas.len(), 1);
    }

    #[test]
    fn project_dir_defaults_to_subdir() {
        // When AUTUMN_MANIFEST_DIR is not set, project_dir returns the
        // subdir name as-is (relative to cwd).
        let env = crate::config::MockEnv::new();
        let dir = super::project_dir("dist", &env);
        assert_eq!(dir, std::path::PathBuf::from("dist"));
    }

    /// Helper to build a test router with custom config.
    pub fn test_router_with_config(routes: Vec<Route>, config: &AutumnConfig) -> axum::Router {
        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        crate::router::build_router(routes, config, state)
    }

    #[tokio::test]
    async fn cors_wildcard_allows_any_origin() {
        let mut config = AutumnConfig::default();
        config.cors.allowed_origins = vec!["*".to_owned()];
        let router = test_router_with_config(vec![test_get_route("/test", "test")], &config);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Origin", "https://example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "*"
        );
    }

    #[tokio::test]
    async fn cors_specific_origin_reflected() {
        let mut config = AutumnConfig::default();
        config.cors.allowed_origins = vec!["https://example.com".to_owned()];
        let router = test_router_with_config(vec![test_get_route("/test", "test")], &config);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Origin", "https://example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://example.com"
        );
    }

    #[tokio::test]
    async fn cors_disabled_when_no_origins() {
        let config = AutumnConfig::default();
        assert!(config.cors.allowed_origins.is_empty());
        let router = test_router_with_config(vec![test_get_route("/test", "test")], &config);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/test")
                    .header("Origin", "https://example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
    }

    #[tokio::test]
    async fn cors_preflight_returns_204() {
        let mut config = AutumnConfig::default();
        config.cors.allowed_origins = vec!["https://example.com".to_owned()];
        let router = test_router_with_config(vec![test_get_route("/test", "test")], &config);

        let response = router
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/test")
                    .header("Origin", "https://example.com")
                    .header("Access-Control-Request-Method", "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .contains_key("access-control-allow-methods")
        );
    }

    #[tokio::test]
    async fn build_router_with_static_skips_without_manifest() {
        // When dist/ exists but has no manifest.json, fall back to
        // the app router without the static layer.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).expect("mkdir");
        // No manifest.json — just an empty dist/

        let config = AutumnConfig::default();
        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let router = crate::router::build_router_with_static(
            vec![test_get_route("/test", "test")],
            &config,
            state,
            Some(dist.as_path()),
        );

        let response = router
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn build_router_with_static_none_dist() {
        // When dist_dir is None, return the app router directly.
        let config = AutumnConfig::default();
        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let router = crate::router::build_router_with_static(
            vec![test_get_route("/test", "test")],
            &config,
            state,
            None,
        );

        let response = router
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ── Startup transparency helper tests ─────────────────────────

    #[test]
    fn format_route_lines_lists_user_routes() {
        let routes = vec![
            test_get_route("/", "index"),
            test_get_route("/users/{id}", "get_user"),
        ];
        let config = AutumnConfig::default();
        let output = format_route_lines(&routes, &[], &config);
        assert!(output.contains("-> index"));
        assert!(output.contains("/ GET"));
        assert!(output.contains("/users/{id}"));
        assert!(output.contains("-> get_user"));
    }

    #[test]
    fn config_runtime_drift_format_route_lines_uses_actuator_prefix() {
        let mut config = AutumnConfig::default();
        config.actuator.prefix = "/ops".to_owned();
        let output = format_route_lines(&[], &[], &config);
        assert!(output.contains("-> health"));
        assert!(output.contains("/ops/*"));
    }

    #[test]
    fn format_task_lines_none_when_empty() {
        assert!(format_task_lines(&[]).is_none());
    }

    #[test]
    fn format_task_lines_fixed_delay() {
        let tasks = vec![crate::task::TaskInfo {
            name: "cleanup".into(),
            schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_secs(300)),
            coordination: crate::task::TaskCoordination::Fleet,
            handler: |_| Box::pin(async { Ok(()) }),
        }];
        let output = format_task_lines(&tasks).unwrap();
        assert!(output.contains("cleanup (every 300s)"));
    }

    #[test]
    fn format_task_lines_cron() {
        let tasks = vec![crate::task::TaskInfo {
            name: "nightly".into(),
            schedule: crate::task::Schedule::Cron {
                expression: "0 0 * * *".into(),
                timezone: None,
            },
            coordination: crate::task::TaskCoordination::Fleet,
            handler: |_| Box::pin(async { Ok(()) }),
        }];
        let output = format_task_lines(&tasks).unwrap();
        assert!(output.contains("nightly (cron 0 0 * * *)"));
    }

    #[test]
    fn format_middleware_list_default() {
        let config = AutumnConfig::default();
        let output = format_middleware_list(&config);
        assert!(output.contains("RequestId"));
        assert!(output.contains("SecurityHeaders"));
        assert!(output.contains("Session (in-memory)"));
        assert!(output.contains("Metrics"));
        // CORS and CSRF should not be present with defaults
        assert!(!output.contains("CORS"));
        assert!(!output.contains("CSRF"));
    }

    #[test]
    fn format_middleware_list_with_cors_and_csrf() {
        let config = AutumnConfig {
            cors: crate::config::CorsConfig {
                allowed_origins: vec!["https://example.com".into()],
                ..crate::config::CorsConfig::default()
            },
            security: crate::security::config::SecurityConfig {
                csrf: crate::security::config::CsrfConfig {
                    enabled: true,
                    ..crate::security::config::CsrfConfig::default()
                },
                ..crate::security::config::SecurityConfig::default()
            },
            ..AutumnConfig::default()
        };
        let output = format_middleware_list(&config);
        assert!(output.contains("CORS"));
        assert!(output.contains("CSRF"));
    }

    #[test]
    fn mask_database_url_with_password() {
        let masked = mask_database_url("postgres://user:secret@localhost:5432/mydb", 10);
        assert!(masked.contains("****"));
        assert!(!masked.contains("secret"));
        assert!(masked.contains("postgres://user:****@localhost:5432/mydb"));
        assert!(masked.contains("pool_size=10"));
    }

    #[test]
    fn mask_database_url_without_password() {
        let masked = mask_database_url("postgres://localhost/mydb", 5);
        assert!(!masked.contains("****"));
        assert!(masked.contains("postgres://localhost/mydb"));
        assert!(masked.contains("pool_size=5"));
    }

    #[test]
    fn mask_database_url_edge_cases() {
        // Special chars in password
        // The url crate parses `p@ssw:rd!` where `@` creates problems if unencoded,
        // but url crate seems to treat `user:p` as auth and `@ssw:rd!` as host if it's poorly formed,
        // let's stick to valid URL formats for testing.

        // URL encoded characters
        let masked2 = mask_database_url("postgres://user:p%40ssw%3Ard%21@localhost:5432/mydb", 10);
        assert!(masked2.contains("****"));
        assert!(!masked2.contains("p%40ssw%3Ard%21"));
        assert!(masked2.contains("postgres://user:****@localhost:5432/mydb"));

        // No user, just password
        let masked3 = mask_database_url("postgres://:secret@localhost:5432/mydb", 10);
        assert!(masked3.contains("****"));
        assert!(!masked3.contains("secret"));
        assert!(masked3.contains("postgres://:****@localhost:5432/mydb"));
    }
    #[test]
    fn mask_database_url_invalid_url_fallback() {
        let masked = mask_database_url("this is completely invalid as a URL with supersecret", 10);
        assert!(masked.contains("****"));
        assert!(!masked.contains("supersecret"));
        assert!(masked.contains("pool_size=10"));
    }

    #[test]
    fn format_config_summary_defaults() {
        let config = AutumnConfig::default();
        let output = format_config_summary(&config);
        assert!(output.contains("profile:    none"));
        assert!(output.contains("server:     127.0.0.1:3000"));
        assert!(output.contains("database:   not configured"));
        assert!(output.contains("log_level:"));
        assert!(output.contains("telemetry:  disabled"));
        assert!(output.contains("health:     /health"));
    }

    #[test]
    fn format_config_summary_with_db() {
        let config = AutumnConfig {
            database: crate::config::DatabaseConfig {
                url: Some("postgres://user:pass@host/db".into()),
                pool_size: 20,
                ..crate::config::DatabaseConfig::default()
            },
            ..AutumnConfig::default()
        };
        let output = format_config_summary(&config);
        assert!(output.contains("user:****@host/db"));
        assert!(output.contains("pool_size=20"));
        assert!(!output.contains("pass"));
    }

    #[test]
    fn format_config_summary_with_profile() {
        let config = AutumnConfig {
            profile: Some("prod".into()),
            ..AutumnConfig::default()
        };
        let output = format_config_summary(&config);
        assert!(output.contains("profile:    prod"));
    }

    #[test]
    fn format_config_summary_with_telemetry() {
        let config = AutumnConfig {
            telemetry: crate::config::TelemetryConfig {
                enabled: true,
                service_name: "orders-api".into(),
                otlp_endpoint: Some("http://otel-collector:4317".into()),
                ..crate::config::TelemetryConfig::default()
            },
            ..AutumnConfig::default()
        };
        let output = format_config_summary(&config);
        assert!(output.contains("telemetry:  Grpc -> http://otel-collector:4317"));
    }

    #[test]
    fn log_startup_transparency_runs_without_panic() {
        // Exercises the tracing::info! calls inside log_startup_transparency.
        // No subscriber installed, so output is discarded -- we just verify
        // the function doesn't panic.
        let routes = vec![test_get_route("/", "index")];
        let tasks = vec![crate::task::TaskInfo {
            name: "cleanup".into(),
            schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_secs(60)),
            coordination: crate::task::TaskCoordination::Fleet,
            handler: |_| Box::pin(async { Ok(()) }),
        }];
        let config = AutumnConfig::default();
        log_startup_transparency(&routes, &tasks, &[], &config);
    }

    #[test]
    fn log_startup_transparency_no_tasks() {
        let routes = vec![test_get_route("/health", "check")];
        let config = AutumnConfig::default();
        log_startup_transparency(&routes, &[], &[], &config);
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn start_task_scheduler_broadcasts_events() {
        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
        };

        let mut rx = state.channels().subscribe("sys:tasks");

        let task = crate::task::TaskInfo {
            name: "test_broadcaster".into(),
            // 1ms delay so it fires immediately
            schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_millis(1)),
            coordination: crate::task::TaskCoordination::Fleet,
            handler: |_| Box::pin(async { Ok(()) }),
        };

        // Start scheduler in background so we don't block
        let state_clone = state.clone();
        tokio::spawn(async move {
            super::start_task_scheduler(
                vec![task],
                &state_clone,
                &tokio_util::sync::CancellationToken::new(),
            );
        });

        // First message should be "started"
        let msg1 = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timeout waiting for start event")
            .expect("channel closed");
        let json1: serde_json::Value = serde_json::from_str(msg1.as_str()).unwrap();
        assert_eq!(json1["event"], "started");
        assert_eq!(json1["task"], "test_broadcaster");

        // Second message should be "success"
        let msg2 = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timeout waiting for success event")
            .expect("channel closed");
        let json2: serde_json::Value = serde_json::from_str(msg2.as_str()).unwrap();
        assert_eq!(json2["event"], "success");
        assert_eq!(json2["task"], "test_broadcaster");
        assert!(json2.get("duration_ms").is_some());
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn start_task_scheduler_broadcasts_failure_events() {
        let state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
        };

        let mut rx = state.channels().subscribe("sys:tasks");

        let task = crate::task::TaskInfo {
            name: "test_failing_task".into(),
            schedule: crate::task::Schedule::FixedDelay(std::time::Duration::from_millis(1)),
            coordination: crate::task::TaskCoordination::Fleet,
            handler: |_| {
                Box::pin(async { Err(crate::AutumnError::bad_request_msg("forced error")) })
            },
        };

        let state_clone = state.clone();
        tokio::spawn(async move {
            super::start_task_scheduler(
                vec![task],
                &state_clone,
                &tokio_util::sync::CancellationToken::new(),
            );
        });

        // First message: started
        let _ = rx.recv().await.unwrap();

        // Second message: failure
        let msg2 = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timeout waiting for failure event")
            .expect("channel closed");
        let json2: serde_json::Value = serde_json::from_str(msg2.as_str()).unwrap();
        assert_eq!(json2["event"], "failure");
        assert_eq!(json2["task"], "test_failing_task");
        assert_eq!(json2["error"], "forced error");
    }

    #[tokio::test]
    async fn execute_task_result_ok_returns_duration() {
        let state = AppState::for_test();
        let handler: crate::task::TaskHandler = |_| Box::pin(async { Ok(()) });
        let start = std::time::Instant::now();
        let result =
            super::execute_task_result(&state, handler, start, "test_task", "fixed_delay").await;
        assert!(result.is_ok(), "expected Ok from successful handler");
        // duration_ms should be a reasonable value (not MAX)
        assert!(result.unwrap() < u64::MAX);
    }

    #[tokio::test]
    async fn execute_task_result_err_returns_duration_and_message() {
        let state = AppState::for_test();
        let handler: crate::task::TaskHandler =
            |_| Box::pin(async { Err(crate::AutumnError::bad_request_msg("test error")) });
        let start = std::time::Instant::now();
        let result =
            super::execute_task_result(&state, handler, start, "test_task", "fixed_delay").await;
        assert!(result.is_err(), "expected Err from failing handler");
        let (duration_ms, msg) = result.unwrap_err();
        assert!(duration_ms < u64::MAX);
        assert!(msg.contains("test error"));
    }

    fn instantly_panicking_scheduled_handler(
        _state: AppState,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send>> {
        panic!("panic before scheduled future")
    }

    #[tokio::test]
    async fn execute_task_result_reports_immediate_handler_panics() {
        let state = AppState::for_test();
        let start = std::time::Instant::now();
        let result = super::execute_task_result(
            &state,
            instantly_panicking_scheduled_handler,
            start,
            "test_task",
            "fixed_delay",
        )
        .await;

        let (duration_ms, msg) = result.expect_err("expected Err from panicking handler");
        assert!(duration_ms < u64::MAX);
        assert!(msg.contains("scheduled task handler panicked: panic before scheduled future"));
    }

    #[tokio::test]
    async fn execute_fixed_delay_task_does_not_timeout_in_process_runs() {
        let state = AppState::for_test();
        state.task_registry.register_scheduled(
            "slow_task",
            "every 1s",
            crate::task::TaskCoordination::Fleet,
            "in_process",
            "replica-a",
        );
        let handler: crate::task::TaskHandler = |_| {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                Ok(())
            })
        };
        let coordinator = std::sync::Arc::new(
            crate::scheduler::InProcessSchedulerCoordinator::new("replica-a"),
        );

        super::execute_fixed_delay_task(
            "slow_task".to_owned(),
            state.clone(),
            handler,
            std::time::Duration::from_secs(1),
            crate::task::TaskCoordination::Fleet,
            coordinator,
            std::time::Duration::from_millis(10),
        )
        .await;

        let snapshot = state.task_registry.snapshot();
        let status = &snapshot["slow_task"];
        assert_eq!(status.status, "idle");
        assert_eq!(status.last_result.as_deref(), Some("ok"));
        assert_eq!(status.total_runs, 1);
        assert_eq!(status.total_failures, 0);
        assert!(status.last_error.is_none());
    }

    static SKIPPED_LEASE_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct DenyingSchedulerCoordinator;

    impl crate::scheduler::SchedulerCoordinator for DenyingSchedulerCoordinator {
        fn backend(&self) -> &'static str {
            "postgres"
        }

        fn replica_id(&self) -> &'static str {
            "replica-a"
        }

        fn try_acquire<'a>(
            &'a self,
            _task_name: &'a str,
            _tick_key: &'a str,
            _coordination: crate::task::TaskCoordination,
        ) -> crate::scheduler::SchedulerFuture<
            'a,
            crate::AutumnResult<Option<crate::scheduler::SchedulerLease>>,
        > {
            Box::pin(async { Ok(None) })
        }
    }

    struct GrantingSchedulerCoordinator {
        backend: &'static str,
        tick_keys: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        release_count: Option<std::sync::Arc<AtomicUsize>>,
    }

    impl crate::scheduler::SchedulerCoordinator for GrantingSchedulerCoordinator {
        fn backend(&self) -> &'static str {
            self.backend
        }

        fn replica_id(&self) -> &'static str {
            "replica-a"
        }

        fn try_acquire<'a>(
            &'a self,
            _task_name: &'a str,
            tick_key: &'a str,
            _coordination: crate::task::TaskCoordination,
        ) -> crate::scheduler::SchedulerFuture<
            'a,
            crate::AutumnResult<Option<crate::scheduler::SchedulerLease>>,
        > {
            Box::pin(async move {
                self.tick_keys.lock().unwrap().push(tick_key.to_owned());
                let lease = self.release_count.as_ref().map_or_else(
                    || crate::scheduler::SchedulerLease::local(self.backend, "replica-a"),
                    |release_count| {
                        crate::scheduler::SchedulerLease::tracked(
                            self.backend,
                            "replica-a",
                            std::sync::Arc::clone(release_count),
                        )
                    },
                );
                Ok(Some(lease))
            })
        }
    }

    fn counted_scheduled_handler(
        _state: AppState,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send>> {
        Box::pin(async {
            SKIPPED_LEASE_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn execute_fixed_delay_task_skips_handler_when_lease_is_not_acquired() {
        SKIPPED_LEASE_HANDLER_CALLS.store(0, Ordering::SeqCst);
        let state = AppState::for_test();
        state.task_registry.register_scheduled(
            "claimed_elsewhere",
            "every 1s",
            crate::task::TaskCoordination::Fleet,
            "postgres",
            "replica-a",
        );
        let coordinator = std::sync::Arc::new(DenyingSchedulerCoordinator);

        super::execute_fixed_delay_task(
            "claimed_elsewhere".to_owned(),
            state.clone(),
            counted_scheduled_handler,
            std::time::Duration::from_secs(1),
            crate::task::TaskCoordination::Fleet,
            coordinator,
            std::time::Duration::from_secs(1),
        )
        .await;

        let snapshot = state.task_registry.snapshot();
        let status = &snapshot["claimed_elsewhere"];
        assert_eq!(SKIPPED_LEASE_HANDLER_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(status.total_runs, 0);
        assert!(status.current_leader.is_none());
        assert!(status.last_tick.is_none());
    }

    #[tokio::test]
    async fn execute_fixed_delay_task_records_distributed_lease_ttl_timeout() {
        let state = AppState::for_test();
        state.task_registry.register_scheduled(
            "slow_distributed_task",
            "every 1s",
            crate::task::TaskCoordination::Fleet,
            "postgres",
            "replica-a",
        );
        let handler: crate::task::TaskHandler = |_| {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(())
            })
        };
        let coordinator = std::sync::Arc::new(GrantingSchedulerCoordinator {
            backend: "postgres",
            tick_keys: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            release_count: None,
        });

        super::execute_fixed_delay_task(
            "slow_distributed_task".to_owned(),
            state.clone(),
            handler,
            std::time::Duration::from_secs(1),
            crate::task::TaskCoordination::Fleet,
            coordinator,
            std::time::Duration::from_millis(10),
        )
        .await;

        let snapshot = state.task_registry.snapshot();
        let status = &snapshot["slow_distributed_task"];
        assert_eq!(status.status, "idle");
        assert_eq!(status.last_result.as_deref(), Some("failed"));
        assert_eq!(status.total_runs, 1);
        assert_eq!(status.total_failures, 1);
        assert!(
            status
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("lease TTL"))
        );
    }

    #[tokio::test]
    async fn execute_cron_task_uses_scheduled_occurrence_for_tick_key() {
        let state = AppState::for_test();
        state.task_registry.register_scheduled(
            "cron_review_task",
            "cron */10 * * * * *",
            crate::task::TaskCoordination::Fleet,
            "postgres",
            "replica-a",
        );
        let tick_keys = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let coordinator = std::sync::Arc::new(GrantingSchedulerCoordinator {
            backend: "postgres",
            tick_keys: std::sync::Arc::clone(&tick_keys),
            release_count: None,
        });
        let handler: crate::task::TaskHandler = |_| Box::pin(async { Ok(()) });
        let scheduled_unix_secs = 1_700_000_000;

        super::execute_cron_task(
            "cron_review_task".to_owned(),
            state.clone(),
            handler,
            crate::task::TaskCoordination::Fleet,
            coordinator,
            std::time::Duration::from_secs(30),
            scheduled_unix_secs,
        )
        .await;

        assert_eq!(
            tick_keys.lock().unwrap().as_slice(),
            ["cron_review_task:1700000000"]
        );
    }

    #[tokio::test]
    async fn execute_fixed_delay_task_releases_lease_when_handler_panics() {
        let state = AppState::for_test();
        state.task_registry.register_scheduled(
            "panic_task",
            "every 1s",
            crate::task::TaskCoordination::Fleet,
            "postgres",
            "replica-a",
        );
        let release_count = std::sync::Arc::new(AtomicUsize::new(0));
        let coordinator = std::sync::Arc::new(GrantingSchedulerCoordinator {
            backend: "postgres",
            tick_keys: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            release_count: Some(std::sync::Arc::clone(&release_count)),
        });
        let handler: crate::task::TaskHandler = |_| {
            Box::pin(async {
                panic!("forced scheduled panic");
                #[allow(unreachable_code)]
                Ok(())
            })
        };

        super::execute_fixed_delay_task(
            "panic_task".to_owned(),
            state.clone(),
            handler,
            std::time::Duration::from_secs(1),
            crate::task::TaskCoordination::Fleet,
            coordinator,
            std::time::Duration::from_secs(30),
        )
        .await;

        let snapshot = state.task_registry.snapshot();
        let status = &snapshot["panic_task"];
        assert_eq!(release_count.load(Ordering::SeqCst), 1);
        assert_eq!(status.status, "idle");
        assert_eq!(status.last_result.as_deref(), Some("failed"));
        assert_eq!(status.total_runs, 1);
        assert_eq!(status.total_failures, 1);
        assert!(
            status
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("scheduled task handler panicked"))
        );
    }

    #[test]
    fn next_cron_occurrence_skips_overdue_slots() {
        use chrono::TimeZone as _;

        let cron = "0 * * * * *"
            .parse::<croner::Cron>()
            .expect("cron expression should parse");
        let stale_cursor = chrono_tz::UTC
            .with_ymd_and_hms(2026, 5, 5, 12, 0, 0)
            .unwrap();
        let now = chrono_tz::UTC
            .with_ymd_and_hms(2026, 5, 5, 12, 30, 5)
            .unwrap();
        let next = super::next_cron_occurrence_after(&cron, &stale_cursor, &now)
            .expect("next cron occurrence should resolve");

        assert_eq!(
            next,
            chrono_tz::UTC
                .with_ymd_and_hms(2026, 5, 5, 12, 31, 0)
                .unwrap()
        );
    }

    #[test]
    fn cron_occurrence_is_overdue_after_later_slot_passed() {
        use chrono::TimeZone as _;

        let cron = "0 * * * * *"
            .parse::<croner::Cron>()
            .expect("cron expression should parse");
        let scheduled_at = chrono_tz::UTC
            .with_ymd_and_hms(2026, 5, 5, 12, 1, 0)
            .unwrap();
        let slightly_late = chrono_tz::UTC
            .with_ymd_and_hms(2026, 5, 5, 12, 1, 5)
            .unwrap();
        let after_later_slot = chrono_tz::UTC
            .with_ymd_and_hms(2026, 5, 5, 12, 30, 5)
            .unwrap();

        assert!(
            !super::cron_occurrence_is_overdue(&cron, &scheduled_at, &slightly_late)
                .expect("overdue check should resolve")
        );
        assert!(
            super::cron_occurrence_is_overdue(&cron, &scheduled_at, &after_later_slot)
                .expect("overdue check should resolve")
        );
    }

    #[cfg(feature = "storage")]
    mod storage_preflight {
        use super::super::{StorageBootstrap, preflight_storage};
        use crate::AppState;
        use crate::config::AutumnConfig;
        use crate::storage::{BlobStoreState, StorageBackend, StorageConfig, StorageLocalConfig};

        fn config_with_storage(storage: StorageConfig) -> AutumnConfig {
            AutumnConfig {
                profile: Some("dev".into()),
                storage,
                ..AutumnConfig::default()
            }
        }

        #[test]
        fn preflight_returns_none_when_disabled() {
            let cfg = config_with_storage(StorageConfig {
                backend: StorageBackend::Disabled,
                ..StorageConfig::default()
            });
            assert!(preflight_storage(&cfg).is_none());
        }

        #[test]
        fn preflight_provisions_local_backend_against_tempdir() {
            let dir = tempfile::tempdir().unwrap();
            let cfg = config_with_storage(StorageConfig {
                backend: StorageBackend::Local,
                local: StorageLocalConfig {
                    root: dir.path().to_path_buf(),
                    ..StorageLocalConfig::default()
                },
                ..StorageConfig::default()
            });
            let bootstrap = preflight_storage(&cfg).expect("local backend should provision");
            assert_eq!(bootstrap.store.provider_id(), "default");
            assert!(bootstrap.serving.is_some(), "local backend mounts a route");
        }

        #[tokio::test]
        async fn install_registers_blob_store_on_state() {
            let dir = tempfile::tempdir().unwrap();
            let cfg = config_with_storage(StorageConfig {
                backend: StorageBackend::Local,
                local: StorageLocalConfig {
                    root: dir.path().to_path_buf(),
                    ..StorageLocalConfig::default()
                },
                ..StorageConfig::default()
            });
            let bootstrap: StorageBootstrap = preflight_storage(&cfg).unwrap();

            let state = AppState::for_test();
            assert!(state.extension::<BlobStoreState>().is_none());
            let serving = bootstrap.install(&state);
            assert!(serving.is_some());
            assert!(state.extension::<BlobStoreState>().is_some());
        }

        #[test]
        fn with_blob_store_stores_custom_store() {
            use crate::storage::{
                Blob, BlobFuture, BlobMeta, BlobStore, BlobStoreError, ByteStream,
            };
            use bytes::Bytes;
            use std::time::Duration;

            struct FakeStore;
            impl BlobStore for FakeStore {
                fn provider_id(&self) -> &'static str {
                    "fake"
                }
                fn put<'a>(&'a self, _k: &'a str, _ct: &'a str, _b: Bytes) -> BlobFuture<'a, Blob> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn put_stream<'a>(
                    &'a self,
                    _k: &'a str,
                    _ct: &'a str,
                    _d: ByteStream<'a>,
                ) -> BlobFuture<'a, Blob> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn get<'a>(&'a self, _k: &'a str) -> BlobFuture<'a, Bytes> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn delete<'a>(&'a self, _k: &'a str) -> BlobFuture<'a, ()> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn head<'a>(&'a self, _k: &'a str) -> BlobFuture<'a, Option<BlobMeta>> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn presigned_url<'a>(
                    &'a self,
                    _k: &'a str,
                    _e: Duration,
                ) -> BlobFuture<'a, String> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
            }

            let builder = crate::app().with_blob_store(FakeStore);
            assert!(builder.blob_store.is_some());
        }

        #[tokio::test]
        async fn with_blob_store_is_installed_on_state() {
            use crate::storage::{
                Blob, BlobFuture, BlobMeta, BlobStore, BlobStoreError, ByteStream,
            };
            use bytes::Bytes;
            use std::time::Duration;

            struct FakeStore;
            impl BlobStore for FakeStore {
                fn provider_id(&self) -> &'static str {
                    "fake-installed"
                }
                fn put<'a>(&'a self, _k: &'a str, _ct: &'a str, _b: Bytes) -> BlobFuture<'a, Blob> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn put_stream<'a>(
                    &'a self,
                    _k: &'a str,
                    _ct: &'a str,
                    _d: ByteStream<'a>,
                ) -> BlobFuture<'a, Blob> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn get<'a>(&'a self, _k: &'a str) -> BlobFuture<'a, Bytes> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn delete<'a>(&'a self, _k: &'a str) -> BlobFuture<'a, ()> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn head<'a>(&'a self, _k: &'a str) -> BlobFuture<'a, Option<BlobMeta>> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
                fn presigned_url<'a>(
                    &'a self,
                    _k: &'a str,
                    _e: Duration,
                ) -> BlobFuture<'a, String> {
                    Box::pin(async { Err(BlobStoreError::Unsupported("fake".into())) })
                }
            }

            let builder = crate::app().with_blob_store(FakeStore);
            let bootstrap = builder.blob_store.map(|store| StorageBootstrap {
                store,
                serving: None,
            });
            let state = AppState::for_test();
            assert!(state.extension::<BlobStoreState>().is_none());
            if let Some(b) = bootstrap {
                b.install(&state);
            }
            let installed = state
                .extension::<BlobStoreState>()
                .expect("store should be installed");
            assert_eq!(installed.store().provider_id(), "fake-installed");
        }
    }

    // ── Route source attribution ───────────────────────────────────────────

    /// A minimal plugin that registers one route with a known name.
    struct TestPlugin {
        name: &'static str,
        route: Route,
    }

    impl crate::plugin::Plugin for TestPlugin {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed(self.name)
        }

        fn build(self, app: AppBuilder) -> AppBuilder {
            app.routes(vec![self.route])
        }
    }

    #[test]
    fn routes_registered_before_plugin_are_user_sourced() {
        let user_route = test_get_route("/home", "home");
        let builder = app().routes(vec![user_route]);
        assert_eq!(builder.route_sources.len(), 1);
        assert_eq!(
            builder.route_sources[0],
            crate::route_listing::RouteSource::User
        );
    }

    #[test]
    fn routes_registered_inside_plugin_are_plugin_sourced() {
        let plugin_route = test_get_route("/plugin-page", "plugin_page");
        let plugin = TestPlugin {
            name: "my-plugin",
            route: plugin_route,
        };
        let builder = app().plugin(plugin);
        assert_eq!(builder.route_sources.len(), 1);
        assert_eq!(
            builder.route_sources[0],
            crate::route_listing::RouteSource::Plugin("my-plugin".to_owned())
        );
    }

    #[test]
    fn routes_registered_after_plugin_revert_to_user_sourced() {
        let plugin_route = test_get_route("/plugin-page", "plugin_page");
        let user_route = test_get_route("/home", "home");
        let plugin = TestPlugin {
            name: "my-plugin",
            route: plugin_route,
        };
        let builder = app().plugin(plugin).routes(vec![user_route]);
        assert_eq!(builder.route_sources.len(), 2);
        assert_eq!(
            builder.route_sources[0],
            crate::route_listing::RouteSource::Plugin("my-plugin".to_owned())
        );
        assert_eq!(
            builder.route_sources[1],
            crate::route_listing::RouteSource::User
        );
    }

    /// A plugin that registers a route and then registers a nested plugin.
    struct OuterPlugin;

    impl crate::plugin::Plugin for OuterPlugin {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            "outer".into()
        }

        fn build(self, app: AppBuilder) -> AppBuilder {
            let inner = TestPlugin {
                name: "inner",
                route: test_get_route("/inner", "inner"),
            };
            app.plugin(inner)
                .routes(vec![test_get_route("/outer-after", "outer_after")])
        }
    }

    #[test]
    fn outer_plugin_source_restored_after_nested_plugin() {
        let builder = app().plugin(OuterPlugin);
        // Routes: [/inner from "inner", /outer-after from "outer"]
        assert_eq!(builder.route_sources.len(), 2);
        assert_eq!(
            builder.route_sources[0],
            crate::route_listing::RouteSource::Plugin("inner".to_owned()),
            "first route should be attributed to inner plugin"
        );
        assert_eq!(
            builder.route_sources[1],
            crate::route_listing::RouteSource::Plugin("outer".to_owned()),
            "second route should be re-attributed to outer plugin after nested build"
        );
    }

    // ── shutdown hook timeout tests ───────────────────────────────────────────

    #[tokio::test]
    async fn shutdown_hooks_with_timeout_runs_all_fast_hooks() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&counter);
        let c2 = Arc::clone(&counter);

        let hooks: Vec<ShutdownHook> = vec![
            Box::new(move || {
                let c = Arc::clone(&c1);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            }),
            Box::new(move || {
                let c = Arc::clone(&c2);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            }),
        ];

        run_shutdown_hooks_with_timeout(
            &hooks,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(10),
        )
        .await;

        assert_eq!(counter.load(Ordering::SeqCst), 2, "both hooks must run");
    }

    #[tokio::test]
    async fn shutdown_hooks_with_timeout_tolerates_slow_hook_overrun() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let fast_ran = Arc::new(AtomicBool::new(false));
        let fr = Arc::clone(&fast_ran);

        let hooks: Vec<ShutdownHook> = vec![
            // hook 0 (first registered → runs LAST in LIFO): fast
            Box::new(move || {
                let fr = Arc::clone(&fr);
                Box::pin(async move {
                    fr.store(true, Ordering::SeqCst);
                })
            }),
            // hook 1 (last registered → runs FIRST in LIFO): slow, exceeds per-hook budget
            Box::new(|| {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                })
            }),
        ];

        // Per-hook budget = 50 ms (hook 0 will overrun).
        // Total budget = 1 s (ample for hook 1 after the overrun is cut short).
        run_shutdown_hooks_with_timeout(
            &hooks,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_secs(1),
        )
        .await;

        assert!(
            fast_ran.load(Ordering::SeqCst),
            "fast hook must still run even after slow hook overruns its per-hook budget"
        );
    }

    // Verify that build_state registers a SharedReqwestClient so that
    // Client::from_state can reuse the shared connection pool on every request.
    #[cfg(feature = "http-client")]
    #[test]
    fn build_state_registers_shared_reqwest_client() {
        let config = AutumnConfig::default();
        let state = build_state(
            &config,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "db")]
            None,
            #[cfg(feature = "ws")]
            None,
        );
        assert!(
            state
                .extension::<crate::http_client::SharedReqwestClient>()
                .is_some(),
            "build_state must register a SharedReqwestClient for connection-pool sharing"
        );
    }
}

#[cfg(all(test, unix))]
mod unix_socket_tests {
    use super::prepare_unix_socket_path;

    #[test]
    fn prepare_unix_socket_path_noop_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.sock");
        prepare_unix_socket_path(&path).expect("absent path is fine");
        assert!(!path.exists());
    }

    #[test]
    fn prepare_unix_socket_path_removes_stale_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stale.sock");
        // Bind then drop a real socket to leave a stale socket file behind.
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind socket");
        drop(listener);
        assert!(path.exists(), "socket file should exist before prepare");
        prepare_unix_socket_path(&path).expect("stale socket should be removed");
        assert!(!path.exists(), "stale socket should be unlinked");
    }

    #[test]
    fn prepare_unix_socket_path_refuses_live_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("live.sock");
        // Keep the listener bound so a connect probe succeeds.
        let _listener = std::os::unix::net::UnixListener::bind(&path).expect("bind socket");
        let err = prepare_unix_socket_path(&path).expect_err("must refuse a live socket");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(path.exists(), "live socket must not be removed");
    }

    #[test]
    fn prepare_unix_socket_path_errors_on_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"i am a regular file").expect("write file");
        let err = prepare_unix_socket_path(&path).expect_err("must refuse a non-socket file");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(path.exists(), "regular file must not be removed");
    }
}
